//! An exo agent runs a command in a smolvm microVM through the whole harness:
//! agent → `create_sandbox` → `run_in_sandbox` → streamed stdout, so a break in
//! the wiring shows up here rather than in the backend-only tests.
//!
//! ```text
//! docker save alpine:latest -o /tmp/alpine.tar
//! EXO_SMOLVM_TEST_IMAGE=/tmp/alpine.tar \
//!   cargo test -p exo --test smolvm_harness_e2e -- --ignored --nocapture
//! ```

use std::env;

use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, CreateSandboxRequest, ExoHarness, NewAgentRequest,
    RunInSandboxRequest, SandboxBackendRegistration, SandboxProvider, SecretBackendChoice,
};
use futures::io::AsyncReadExt;
use tokio::sync::{Mutex, OnceCell};

/// Serialises the e2e tests: cleanup reaps by owner pid and every test in a
/// binary shares one, so a finishing test would delete a running test's machine
/// — which surfaced as an empty exec, not as an obvious cleanup bug.
static E2E_GATE: OnceCell<Mutex<()>> = OnceCell::const_new();

async fn e2e_gate() -> &'static Mutex<()> {
    E2E_GATE.get_or_init(|| async { Mutex::new(()) }).await
}
use tempfile::TempDir;

fn test_image() -> Option<String> {
    match env::var("EXO_SMOLVM_TEST_IMAGE") {
        Ok(value) if !value.is_empty() => Some(value),
        _ => {
            eprintln!("skipping smolvm harness e2e: EXO_SMOLVM_TEST_IMAGE is not set");
            None
        }
    }
}

#[tokio::test]
#[ignore]
async fn agent_runs_a_command_in_a_smolvm_microvm() {
    let Some(image) = test_image() else { return };
    let _serial = e2e_gate().await.lock().await;
    let tempdir = TempDir::new().expect("tempdir");
    // Configured as a deployment would: registered by name, made the default.
    let harness = BasicExoHarness::new(BasicExoHarnessConfig {
        root: tempdir.path().to_path_buf(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::Smolvm,
        sandbox_backends: vec![
            SandboxBackendRegistration::from_builtin_provider(SandboxProvider::Smolvm)
                .expect("smolvm is a builtin provider"),
        ],
    })
    .await
    .expect("harness");

    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "smolvm-e2e".to_string(),
            name: "smolvm e2e".to_string(),
        })
        .await
        .expect("agent");

    // Selected through the registry; nothing here constructs the backend.
    let sandbox_id = agent
        .create_sandbox(CreateSandboxRequest {
            name: Some("smolvm-e2e".to_string()),
            provider: SandboxProvider::Smolvm,
            image,
            resources: Default::default(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            // Off, so reaching the guest kernel proves the VM boundary.
            enable_networking: Some(false),
            idle_seconds: Some(120),
        })
        .await
        .expect("create smolvm sandbox");

    let process = agent
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "uname -s; printf ' '; uname -r".to_string(),
            ],
            env: Default::default(),
        })
        .await
        .expect("run in smolvm sandbox");

    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut output = String::new();
    stdout.read_to_string(&mut output).await.expect("stdout");
    let exit = parts.wait.await.expect("exit");

    println!("guest reported: {}", output.trim());
    assert_eq!(exit, 0, "command failed: {output}");
    assert!(
        output.starts_with("Linux"),
        "expected a Linux guest kernel, got {output:?}"
    );
    // A matching string would mean the command escaped to the host.
    let host = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .expect("host uname");
    let host_release = String::from_utf8_lossy(&host.stdout).trim().to_string();
    assert!(
        !output.contains(&host_release),
        "guest kernel matches the host ({host_release}) — this did not run in a VM"
    );

    // Each run uses a fresh sandbox id, so without this every run strands a warm
    // VM. The owner-pid label identifies ours without reading harness records.
    reap_machines_owned_by_this_process();
}

/// Delete smolvm machines labelled with this process as owner.
fn reap_machines_owned_by_this_process() {
    let binary = env::var("SMOLVM_BIN").unwrap_or_else(|_| "smolvm".to_string());
    let Ok(listing) = std::process::Command::new(&binary)
        .args(["machine", "ls", "--json"])
        .output()
    else {
        return;
    };
    let Ok(machines) = serde_json::from_slice::<serde_json::Value>(&listing.stdout) else {
        return;
    };
    let mine = std::process::id().to_string();
    for machine in machines.as_array().into_iter().flatten() {
        let owned = machine
            .get("labels")
            .and_then(|l| l.get("exo.sandbox.owner-pid"))
            .and_then(|v| v.as_str())
            .is_some_and(|owner| owner == mine);
        if !owned {
            continue;
        }
        if let Some(name) = machine.get("name").and_then(|v| v.as_str()) {
            let _ = std::process::Command::new(&binary)
                .args(["machine", "delete", "--name", name, "--force"])
                .output();
        }
    }
}

/// The shape a real deployment gets: no explicit networking or lifetime, so exo's
/// defaults apply (networking on, warm), booting a registry image. The other e2e
/// pins the opposite of all three, so it would not catch a default-path break.
///
/// ```text
/// EXO_SMOLVM_REGISTRY_IMAGE=localhost:5057/alpine:latest \
///   cargo test -p exo --test smolvm_harness_e2e default_shape -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn agent_runs_the_default_sandbox_shape() {
    let Ok(image) = env::var("EXO_SMOLVM_REGISTRY_IMAGE") else {
        eprintln!("skipping default-shape e2e: EXO_SMOLVM_REGISTRY_IMAGE is not set");
        return;
    };
    let _serial = e2e_gate().await.lock().await;
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(BasicExoHarnessConfig {
        root: tempdir.path().to_path_buf(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::Smolvm,
        sandbox_backends: vec![
            SandboxBackendRegistration::from_builtin_provider(SandboxProvider::Smolvm)
                .expect("smolvm is a builtin provider"),
        ],
    })
    .await
    .expect("harness");

    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "smolvm-default".to_string(),
            name: "smolvm default".to_string(),
        })
        .await
        .expect("agent");

    let sandbox_id = agent
        .create_sandbox(CreateSandboxRequest {
            name: Some("smolvm-default".to_string()),
            provider: SandboxProvider::Smolvm,
            image,
            resources: Default::default(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            // Left unset on purpose: this is the whole point of the test.
            enable_networking: None,
            idle_seconds: None,
        })
        .await
        .expect("create default sandbox");

    let process = agent
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "uname -sr".to_string(),
            ],
            env: Default::default(),
        })
        .await
        .expect("run in default sandbox");

    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut output = String::new();
    stdout.read_to_string(&mut output).await.expect("stdout");
    let exit = parts.wait.await.expect("exit");

    println!("default-shape guest: {}", output.trim());
    assert_eq!(exit, 0, "command failed: {output}");
    assert!(
        output.starts_with("Linux"),
        "expected a Linux guest, got {output:?}"
    );

    reap_machines_owned_by_this_process();
}

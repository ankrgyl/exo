//! Live teleport test: a sandbox running on local Docker is snapshotted and
//! moved to Daytona — the SAME sandbox id, resumed remotely with its files
//! intact. Drives the harness API (not just the backend), i.e. the exact code
//! path behind the REPL's `/teleport daytona` command:
//!
//!   1. create a sandbox on Docker, write a marker file
//!   2. `snapshot_sandbox` (-> DockerImageTar payload in exoharness storage)
//!   3. `start_sandbox` with `provider: Some(Daytona)` — the cross-provider
//!      override routes the restore through the Daytona backend's Docker
//!      bridge (`docker load` -> `daytona snapshot push` -> create)
//!   4. exec on the same sandbox id — now running on Daytona — and read the
//!      marker file back
//!
//! Ignored by default — requires a local Docker daemon and `DAYTONA_API_KEY`
//! (+ optional `DAYTONA_ORGANIZATION_ID` / `DAYTONA_TARGET`). Run with:
//!
//! ```bash
//! cargo test -p exo --test teleport_docker_to_daytona -- --ignored --nocapture
//! ```

use std::process::Command;

use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, ConversationHandle, CreateSandboxRequest,
    DaytonaBackendSpec, ExoHarness, FileSystemMount, FileSystemMountMode, NewAgentRequest,
    NewConversationRequest, PutSecretRequest, RunInSandboxRequest, SandboxBackendRegistration,
    SandboxId, SandboxProvider, Secret, SecretBackendChoice, StartSandboxRequest,
};
use futures::io::AsyncReadExt;
use tempfile::TempDir;

#[tokio::test]
#[ignore = "live: needs docker and DAYTONA_API_KEY"]
async fn teleport_docker_sandbox_to_daytona_keeps_files() {
    let Ok(api_key) = std::env::var("DAYTONA_API_KEY") else {
        eprintln!("skipping: DAYTONA_API_KEY not set");
        return;
    };
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }

    let root = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(BasicExoHarnessConfig {
        root: root.path().to_path_buf(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::Docker,
        sandbox_backends: vec![
            SandboxBackendRegistration::docker(),
            SandboxBackendRegistration::daytona(DaytonaBackendSpec::with_conventional_secrets()),
        ],
    })
    .await
    .expect("harness");

    // Daytona credentials live in the secret store (resolved lazily on first use).
    seed_secret(&harness, "DAYTONA_API_KEY", &api_key).await;
    if let Ok(v) = std::env::var("DAYTONA_ORGANIZATION_ID") {
        seed_secret(&harness, "DAYTONA_ORGANIZATION_ID", &v).await;
    }
    if let Ok(v) = std::env::var("DAYTONA_TARGET") {
        seed_secret(&harness, "DAYTONA_TARGET", &v).await;
    }

    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "teleport".into(),
            name: "teleport".into(),
        })
        .await
        .expect("agent");
    let conv = agent
        .new_conversation(NewConversationRequest {
            slug: Some("teleport-conv".into()),
            name: Some("teleport".into()),
        })
        .await
        .expect("conversation");

    // 1. Create a Docker sandbox and write a marker file.
    let sandbox_id = conv
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::Docker,
            image: "docker.io/library/ubuntu:24.04".into(),
            default_workdir: Some("/".into()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(false),
            egress_policy: None,
            idle_seconds: Some(300),
        })
        .await
        .expect("create docker sandbox");
    let (rc, _, _) = exec_shell(
        conv.as_ref(),
        &sandbox_id,
        "echo teleport-marker > /exo-teleport.txt",
    )
    .await;
    assert_eq!(rc, 0, "writing the marker on docker should succeed");

    // 2. Snapshot the Docker sandbox (-> DockerImageTar in exoharness storage).
    let snapshot_id = conv
        .snapshot_sandbox(sandbox_id.clone())
        .await
        .expect("snapshot docker sandbox");

    // 3. Restore under Daytona via the provider override (-> bridge).
    conv.start_sandbox(StartSandboxRequest {
        id: sandbox_id.clone(),
        snapshot_id,
        idle_seconds: None,
        provider: Some(SandboxProvider::Daytona),
    })
    .await
    .expect("start_sandbox on daytona from the docker snapshot");

    // 4. The file written on Docker is present on the Daytona sandbox.
    let (rc, stdout, _) = exec_shell(
        conv.as_ref(),
        &sandbox_id,
        "cat /exo-teleport.txt && echo --- && hostname",
    )
    .await;
    eprintln!("after teleport, daytona sees:\n{stdout}");
    assert_eq!(
        rc, 0,
        "exec on the teleported daytona sandbox should succeed"
    );
    assert!(
        stdout.contains("teleport-marker"),
        "file should survive the docker->daytona teleport: {stdout:?}"
    );

    let _ = conv.stop_sandbox(sandbox_id).await;

    // 5. Make-before-break: a teleport that fails must leave the source
    //    sandbox running. Host mounts are rejected by the Daytona backend
    //    inside acquire_from_snapshot, which makes the failure deterministic
    //    (and local — nothing reaches the Daytona API).
    let mount_dir = TempDir::new().expect("mount dir");
    let mounted_id = conv
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::Docker,
            image: "docker.io/library/ubuntu:24.04".into(),
            default_workdir: Some("/".into()),
            file_system_mounts: Some(vec![FileSystemMount {
                host_path: mount_dir.path().display().to_string(),
                mount_path: "/mnt/host".into(),
                mode: FileSystemMountMode::ReadOnly,
                internal: None,
            }]),
            durable_file_systems: None,
            enable_networking: Some(false),
            egress_policy: None,
            idle_seconds: Some(300),
        })
        .await
        .expect("create mounted docker sandbox");
    let (rc, _, _) = exec_shell(conv.as_ref(), &mounted_id, "echo alive > /still-here.txt").await;
    assert_eq!(rc, 0);

    let snapshot_id = conv
        .snapshot_sandbox(mounted_id.clone())
        .await
        .expect("snapshot mounted docker sandbox");
    let error = conv
        .start_sandbox(StartSandboxRequest {
            id: mounted_id.clone(),
            snapshot_id,
            idle_seconds: None,
            provider: Some(SandboxProvider::Daytona),
        })
        .await
        .expect_err("teleporting a mounted sandbox to daytona must fail");
    eprintln!("failed teleport (expected): {error:#}");

    let (rc, stdout, _) = exec_shell(conv.as_ref(), &mounted_id, "cat /still-here.txt").await;
    assert_eq!(
        rc, 0,
        "the source sandbox must still be running after a failed teleport"
    );
    assert_eq!(stdout.trim(), "alive");

    let _ = conv.stop_sandbox(mounted_id).await;
}

async fn seed_secret(harness: &BasicExoHarness, name: &str, value: &str) {
    harness
        .put_secret(PutSecretRequest {
            name: name.into(),
            secret: Secret::Key {
                value: value.into(),
            },
        })
        .await
        .expect("put secret");
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn exec_shell(
    conversation: &dyn ConversationHandle,
    sandbox_id: &SandboxId,
    cmd: &str,
) -> (i32, String, String) {
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id.clone(),
            command: vec!["/bin/bash".into(), "-c".into(), cmd.into()],
            env: Default::default(),
        })
        .await
        .unwrap_or_else(|error| panic!("run_in_sandbox({cmd:?}) failed: {error:#}"));

    let mut parts = process.into_parts();
    drop(parts.stdin);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = tokio::join!(
        parts.stdout.read_to_end(&mut stdout_bytes),
        parts.stderr.read_to_end(&mut stderr_bytes),
        parts.wait,
    );
    stdout_result.expect("read stdout");
    stderr_result.expect("read stderr");
    let exit_code = wait_result.expect("wait on sandbox process");
    (
        exit_code,
        String::from_utf8_lossy(&stdout_bytes).into_owned(),
        String::from_utf8_lossy(&stderr_bytes).into_owned(),
    )
}

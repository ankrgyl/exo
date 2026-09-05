//! Live tests for the smolvm sandbox backend. These boot real VMs, so they are
//! `#[ignore]`d and gated on an image:
//!
//! ```text
//! docker save alpine:latest -o /tmp/alpine.tar
//! EXO_SMOLVM_TEST_IMAGE=/tmp/alpine.tar \
//!   cargo test -p exo --test smolvm_sandbox_live -- --ignored --nocapture
//! ```
//!
//! A local archive rather than a registry reference, so the tests run air-gapped.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use exoharness::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxBackendRegistration, SandboxCommand,
    SandboxKey, SandboxLifecycleConfig, SandboxMount, SandboxMountAccess, SandboxNetworkPolicy,
    SandboxProvider, SandboxRequest, SandboxSpec, SmolvmExecutionMode, SmolvmSandboxBackend,
};
use futures::io::AsyncReadExt;
use tokio::sync::{OnceCell, RwLock};

/// Serialises the one test that counts *host-wide* VM processes against every
/// test that boots one. Readers boot freely; the counter takes the write side so
/// it observes a quiet host, instead of passing only when other tests happen to
/// be skipping.
static VM_COUNT_GATE: OnceCell<RwLock<()>> = OnceCell::const_new();

async fn vm_gate() -> &'static RwLock<()> {
    VM_COUNT_GATE
        .get_or_init(|| async { RwLock::new(()) })
        .await
}

/// Registration is the seam the harness selects on; no VM needed to check it.
#[test]
fn smolvm_is_a_builtin_provider() {
    SandboxBackendRegistration::from_builtin_provider(SandboxProvider::Smolvm)
        .expect("smolvm should be a built-in sandbox provider");
}

/// The tests are useless without an image; skip loudly rather than fail.
fn test_image() -> Option<String> {
    match env::var("EXO_SMOLVM_TEST_IMAGE") {
        Ok(value) if !value.is_empty() => Some(value),
        _ => {
            eprintln!("skipping smolvm live test: EXO_SMOLVM_TEST_IMAGE is not set");
            None
        }
    }
}

/// Whether a `smolvm` binary is actually installed. Same "skip loudly" contract
/// as [`test_image`]: the probe-backed assertions below describe the INSTALLED
/// engine, so without one there is nothing to assert about — and asserting
/// anyway turns "this machine has no smolvm" into a red build.
fn smolvm_installed() -> bool {
    let bin = env::var("SMOLVM_BIN").unwrap_or_else(|_| "smolvm".to_string());
    match std::process::Command::new(&bin).arg("--version").output() {
        Ok(out) if out.status.success() => true,
        _ => {
            eprintln!("skipping smolvm live test: no usable `{bin}` on PATH");
            false
        }
    }
}

fn workspace_dir(tag: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("exo-smolvm-live-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workspace dir");
    dir
}

/// `tag` keys the sandbox, distinct per test: warm derives the machine name from
/// the key, so tests sharing one would fight over a single machine.
fn request(
    image: String,
    workspace: &Path,
    network: SandboxNetworkPolicy,
    tag: &str,
    idle_ttl: Option<Duration>,
) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::AgentSandbox {
            agent_id: "smolvm-live".into(),
            sandbox_id: tag.into(),
        },
        spec: SandboxSpec {
            image,
            mounts: vec![SandboxMount {
                host_path: workspace.to_path_buf(),
                guest_path: "/workspace".into(),
                access: SandboxMountAccess::ReadWrite,
                internal: false,
            }],
            durable_file_systems: Vec::new(),
            network_policy: network.into(),
            default_workdir: "/".into(),
        },
        lifecycle: SandboxLifecycleConfig { idle_ttl },
        provider_state: None,
    }
}

/// Warm sandboxes outlive the test by design, so whoever made one deletes it.
/// The handle reports its machine name via `provider_state`.
async fn cleanup(handle: &Arc<dyn ManagedSandboxHandle>) {
    let _ = handle.stop().await;
    let Some(machine) = handle
        .provider_state()
        .and_then(|state| state.get("machine")?.as_str().map(str::to_string))
    else {
        return; // one-shot handles own nothing to clean up
    };
    let binary = env::var("SMOLVM_BIN").unwrap_or_else(|_| "smolvm".to_string());
    let _ = tokio::process::Command::new(binary)
        .args(["machine", "delete", "--name", &machine, "--force"])
        .output()
        .await;
}

fn command(argv: &[&str]) -> SandboxCommand {
    SandboxCommand {
        argv: argv.iter().map(|a| a.to_string()).collect(),
        env: HashMap::new(),
        display_argv: None,
        cwd: None,
        timeout: Some(Duration::from_secs(120)),
    }
}

/// The whole point: the workload runs behind a hypervisor with its own kernel.
#[tokio::test]
#[ignore]
async fn smolvm_exec_runs_under_a_guest_kernel() {
    let Some(image) = test_image() else { return };
    let _shared = vm_gate().await.read().await;
    let workspace = workspace_dir("kernel");
    let backend = SmolvmSandboxBackend::new();

    let handle = backend
        .acquire(request(
            image,
            &workspace,
            SandboxNetworkPolicy::deny_all(),
            "kernel",
            None,
        ))
        .await
        .expect("acquire smolvm sandbox");

    let output = handle
        .exec(&command(&["uname", "-sr"]))
        .await
        .expect("exec uname");

    println!("guest uname: {}", output.stdout.trim());
    assert!(output.ok, "uname failed: {}", output.stderr);

    let host = std::process::Command::new("uname")
        .arg("-sr")
        .output()
        .expect("host uname");
    let host = String::from_utf8_lossy(&host.stdout).trim().to_string();
    println!("host uname:  {host}");

    assert!(
        output.stdout.trim().starts_with("Linux"),
        "guest should report a Linux kernel, got {:?}",
        output.stdout
    );
    assert_ne!(
        output.stdout.trim(),
        host,
        "guest kernel matches the host kernel — this is not a VM"
    );

    cleanup(&handle).await;
}

/// Mount-backed state is what makes one-shot viable: the workspace has to survive
/// execs even though the VM does not.
#[tokio::test]
#[ignore]
async fn smolvm_workspace_mount_round_trips_between_execs() {
    let Some(image) = test_image() else { return };
    let _shared = vm_gate().await.read().await;
    let workspace = workspace_dir("mount");
    // Pinned: warm would satisfy this trivially by reusing the machine.
    let backend = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::OneShot);

    let handle = backend
        .acquire(request(
            image,
            &workspace,
            SandboxNetworkPolicy::deny_all(),
            "mount",
            None,
        ))
        .await
        .expect("acquire smolvm sandbox");

    let write = handle
        .exec(&command(&[
            "sh",
            "-c",
            "echo written-in-the-vm > /workspace/marker",
        ]))
        .await
        .expect("exec write");
    assert!(write.ok, "write failed: {}", write.stderr);

    // Visible on the host: the mount is real, not a copy.
    let host_marker = std::fs::read_to_string(workspace.join("marker")).expect("read host marker");
    assert_eq!(host_marker.trim(), "written-in-the-vm");

    // Visible to a *second, separate* VM: state survives the sandbox exiting.
    let read = handle
        .exec(&command(&["cat", "/workspace/marker"]))
        .await
        .expect("exec read");
    assert!(read.ok, "read failed: {}", read.stderr);
    assert_eq!(read.stdout.trim(), "written-in-the-vm");
}

/// The streaming contract the harness uses: incremental output, real exit code.
#[tokio::test]
#[ignore]
async fn smolvm_start_process_streams_and_exits() {
    let Some(image) = test_image() else { return };
    let _shared = vm_gate().await.read().await;
    let workspace = workspace_dir("stream");
    let backend = SmolvmSandboxBackend::new();

    let handle = backend
        .acquire(request(
            image,
            &workspace,
            SandboxNetworkPolicy::deny_all(),
            "stream",
            None,
        ))
        .await
        .expect("acquire smolvm sandbox");

    let parts = handle
        .start_process(&command(&["sh", "-c", "echo hello; echo world"]))
        .await
        .expect("start_process");

    let mut stdout = parts.stdout;
    let mut buf = String::new();
    stdout.read_to_string(&mut buf).await.expect("read stdout");
    let code = parts.wait.await.expect("wait");

    println!("streamed stdout: {buf:?} exit={code}");
    assert!(buf.contains("hello"), "missing first line: {buf:?}");
    assert!(buf.contains("world"), "missing second line: {buf:?}");
    assert_eq!(code, 0);

    cleanup(&handle).await;
}

/// The default must resolve against the installed smolvm rather than assume.
#[tokio::test]
#[ignore]
async fn auto_mode_selects_warm_on_a_current_smolvm() {
    let backend = SmolvmSandboxBackend::new();
    // Holds regardless of what is installed — `Auto` is the configured default,
    // not a probe result, so it is worth asserting even on a bare machine.
    assert_eq!(backend.mode(), SmolvmExecutionMode::Auto);
    if !smolvm_installed() {
        return;
    }
    assert!(
        backend.warm_supported().await,
        "a smolvm >= 1.7.2 should support warm; check `smolvm --version`"
    );
}

/// A timed-out exec must not strand its VM: exo SIGKILLs the CLI, which does not
/// stop the microVM, so the timeout is pushed into smolvm instead.
#[tokio::test]
#[ignore]
async fn timed_out_exec_leaves_no_orphaned_vm() {
    let Some(image) = test_image() else { return };
    // Exclusive: this test counts host-wide VM processes.
    let _exclusive = vm_gate().await.write().await;
    let workspace = workspace_dir("timeout");
    let backend = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::OneShot);

    let before = settled_vm_count().await;
    let handle = backend
        .acquire(request(
            image,
            &workspace,
            SandboxNetworkPolicy::deny_all(),
            "timeout",
            None,
        ))
        .await
        .expect("acquire smolvm sandbox");

    let mut cmd = command(&["sleep", "120"]);
    cmd.timeout = Some(Duration::from_secs(5));
    let started = std::time::Instant::now();
    let outcome = handle.exec(&cmd).await;
    let elapsed = started.elapsed();
    println!(
        "timed-out exec returned after {elapsed:?}: ok={:?}",
        outcome.as_ref().map(|o| o.ok)
    );

    // The VM-count check below is satisfied by a VM that was reaped correctly AND
    // by one that never booted, so it cannot stand alone: a total regression of
    // the boot path (a bad `SMOLVM_BOOT_BINARY` is enough) creates nothing to
    // strand and sails through.
    //
    // `ok` alone does not separate those either — a VM that fails to start also
    // reports `Ok(ok: false)`, just immediately. Elapsed time is the honest
    // discriminator: a real timeout cannot return before the timeout.
    let outcome = outcome.expect("exec `sleep 120`");
    assert!(
        !outcome.ok,
        "`sleep 120` under a 5s timeout must not report success: {outcome:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(4),
        "returned in {elapsed:?} — that is a failure to start, not a timeout"
    );

    // Give a stranded VM time to show up before counting.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let after = running_vm_count();
    println!("smolvm VM processes: before={before} after={after}");
    // `<=`, not `==`: an earlier test's VM may still be exiting while this runs,
    // which only ever lowers the count. A strand raises it.
    assert!(
        after <= before,
        "a timed-out exec stranded a microVM (before={before}, after={after})"
    );
}

/// A VM count taken once the host stops changing. The write lock keeps other
/// tests from starting VMs, but not from still tearing theirs down, so a raw
/// count here would make the comparison depend on that timing.
async fn settled_vm_count() -> usize {
    let mut last = running_vm_count();
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = running_vm_count();
        if now == last {
            return now;
        }
        last = now;
    }
    last
}

/// Snapshot round-trip through the backend API.
///
/// Gated on a REGISTRY image, unlike the other tests: `pack create --from-vm`
/// re-pulls by manifest, and resolving that needs the sandbox to have network.
///
/// ```text
/// docker run -d --rm -p 5055:5000 registry:2
/// docker tag alpine:latest localhost:5055/alpine:latest && docker push localhost:5055/alpine:latest
/// EXO_SMOLVM_REGISTRY_IMAGE=localhost:5055/alpine:latest cargo test -p exo \
///   --test smolvm_sandbox_live snapshot -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn snapshot_round_trip_preserves_guest_state() {
    let Ok(image) = env::var("EXO_SMOLVM_REGISTRY_IMAGE") else {
        eprintln!("skipping snapshot test: EXO_SMOLVM_REGISTRY_IMAGE is not set");
        return;
    };
    let workspace = workspace_dir("snap");
    let backend = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::Warm);
    let ttl = Some(Duration::from_secs(600));

    let source = backend
        .acquire(request(
            image.clone(),
            &workspace,
            SandboxNetworkPolicy::allow_all(),
            "snap-source",
            ttl,
        ))
        .await
        .expect("acquire source sandbox");

    let write = source
        .exec(&command(&["sh", "-c", "echo packed-state > /root/marker"]))
        .await
        .expect("exec write");
    assert!(write.ok, "write failed: {}", write.stderr);

    let payload = source.snapshot().await.expect("snapshot");
    println!(
        "snapshot format={} manifest={} bytes",
        payload.format,
        payload.bytes.len()
    );

    // Restored under a *different* key, so it is genuinely a new machine.
    let restored = backend
        .acquire_from_snapshot(
            request(
                image,
                &workspace,
                SandboxNetworkPolicy::allow_all(),
                "snap-restored",
                ttl,
            ),
            payload,
        )
        .await
        .expect("acquire_from_snapshot");

    let read = restored
        .exec(&command(&["cat", "/root/marker"]))
        .await
        .expect("exec read");
    println!("restored guest state: {:?}", read.stdout.trim());
    assert!(read.ok, "read failed: {}", read.stderr);
    assert_eq!(read.stdout.trim(), "packed-state");

    cleanup(&source).await;
    cleanup(&restored).await;
}

/// A machine whose owner is gone must be reclaimed by a later run — what labels
/// buy, since the in-process TTL sweep dies with the process. Needs smolvm 1.8.0.
#[tokio::test]
#[ignore]
async fn abandoned_machines_are_reaped_by_a_later_backend() {
    let Some(image) = test_image() else { return };
    let _shared = vm_gate().await.read().await;
    let workspace = workspace_dir("abandoned");
    let backend = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::Warm);
    if !backend.labels_supported().await {
        eprintln!("skipping: installed smolvm predates machine labels");
        return;
    }

    // The state a crashed harness leaves: labelled ours, owned by a pid that
    // cannot exist (one past Linux's pid_max).
    let orphan = "exo-abandoned-probe";
    let binary = env::var("SMOLVM_BIN").unwrap_or_else(|_| "smolvm".to_string());
    let _ = std::process::Command::new(&binary)
        .args(["machine", "delete", "--name", orphan, "--force"])
        .output();
    let created = std::process::Command::new(&binary)
        .args([
            "machine",
            "create",
            "--name",
            orphan,
            "--image",
            &image,
            "--label",
            "exo.sandbox.key=agent:abandoned:1",
            "--label",
            "exo.sandbox.owner-pid=4194305",
            "--",
            "sleep",
            "infinity",
        ])
        .output()
        .expect("create orphan machine");
    assert!(
        created.status.success(),
        "could not create orphan: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(machine_names().contains(&orphan.to_string()));

    // Any acquire runs the sweep.
    let live = backend
        .acquire(request(
            image,
            &workspace,
            SandboxNetworkPolicy::deny_all(),
            "abandoned-sweeper",
            Some(Duration::from_secs(600)),
        ))
        .await
        .expect("acquire sweeper sandbox");

    let remaining = machine_names();
    println!(
        "orphan present after sweep: {}",
        remaining.contains(&orphan.to_string())
    );
    assert!(
        !remaining.contains(&orphan.to_string()),
        "abandoned machine survived the sweep"
    );

    cleanup(&live).await;
}

fn machine_names() -> Vec<String> {
    let binary = env::var("SMOLVM_BIN").unwrap_or_else(|_| "smolvm".to_string());
    let out = std::process::Command::new(&binary)
        .args(["machine", "ls", "--json"])
        .output()
        .expect("machine ls");
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m.get("name")?.as_str().map(str::to_string))
        .collect()
}

/// Count live `_boot-vm` processes by executable, not by a command-line pattern
/// that would also match this test's own argv.
fn running_vm_count() -> usize {
    let output = std::process::Command::new("ps")
        .args(["ax", "-o", "command"])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("smolvm-bin") && line.contains("_boot-vm"))
        .count()
}

/// Warm keeps guest-local mutation — what one-shot cannot carry — between execs.
#[tokio::test]
#[ignore]
async fn smolvm_warm_mode_persists_guest_local_state() {
    let Some(image) = test_image() else { return };
    let _shared = vm_gate().await.read().await;
    let workspace = workspace_dir("warm");
    let backend = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::Warm);

    let handle = backend
        .acquire(request(
            image,
            &workspace,
            SandboxNetworkPolicy::deny_all(),
            "warm",
            Some(Duration::from_secs(600)),
        ))
        .await
        .expect("acquire warm smolvm sandbox");

    // Written outside any mount, so only a surviving VM can still have it.
    let write = handle
        .exec(&command(&["sh", "-c", "echo warm-state > /root/marker"]))
        .await
        .expect("exec write");
    assert!(write.ok, "write failed: {}", write.stderr);

    let read = handle
        .exec(&command(&["cat", "/root/marker"]))
        .await
        .expect("exec read");
    println!("guest-local state across execs: {:?}", read.stdout.trim());
    assert!(read.ok, "read failed: {}", read.stderr);
    assert_eq!(
        read.stdout.trim(),
        "warm-state",
        "guest-local state did not survive between execs — the machine was not reused"
    );

    handle.stop().await.expect("stop warm sandbox");
    cleanup(&handle).await;
}

/// Denied unless asked for, and enforced at the VM edge rather than by a rule.
#[tokio::test]
#[ignore]
async fn smolvm_network_is_denied_unless_requested() {
    let Some(image) = test_image() else { return };
    let _shared = vm_gate().await.read().await;
    let workspace = workspace_dir("net");
    let backend = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::OneShot);

    let handle = backend
        .acquire(request(
            image,
            &workspace,
            SandboxNetworkPolicy::deny_all(),
            "net",
            None,
        ))
        .await
        .expect("acquire smolvm sandbox");

    // Without this, the denial below passes for the wrong reason on any guest
    // that simply has no `wget`: the command fails, `ok` is false, and the test
    // reports a network policy it never exercised.
    let probe = handle
        .exec(&command(&["sh", "-c", "command -v wget"]))
        .await
        .expect("exec wget probe");
    assert!(
        probe.ok,
        "guest image has no wget, so a failed fetch would prove nothing"
    );

    let output = handle
        .exec(&command(&[
            "sh",
            "-c",
            "wget -q -T 3 -O /dev/null http://example.com",
        ]))
        .await
        .expect("exec wget");

    println!(
        "wget exit={:?} stderr={:?}",
        output.exit_code, output.stderr
    );
    assert!(
        !output.ok,
        "network reached the internet with SandboxNetworkPolicy::deny_all()"
    );
}

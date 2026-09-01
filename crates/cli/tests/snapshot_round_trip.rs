//! End-to-end snapshot + rewind round-trip for the Docker sandbox backend.
//!
//! This test is the canonical reference for how filesystem snapshots are
//! used. It drives the harness API directly (no binary spawn, no LLM mock)
//! and exercises the same lifecycle the manual REPL demo does:
//!
//!   1. create a sandbox
//!   2. write `version 1` to a file via a real shell tool call
//!   3. `snapshot_sandbox` — capture filesystem state
//!   4. overwrite the file to `version 2` and create a sibling file
//!   5. `start_sandbox` with the captured snapshot id — rewind
//!   6. read the file back: it reads `version 1`, the sibling file is gone
//!
//! Backend-agnostic at the source level: docker works the same on Linux and
//! macOS (Docker Desktop / Colima). `#[ignore]`'d so the regular `cargo test`
//! skips it; CI runs it via `cargo test -- --ignored`. Self-skips when
//! `EXO_TEST_SANDBOX_BACKEND` is set to anything other than `docker` (so the
//! local-process matrix cells don't false-fail), and when `docker info` is
//! unhappy on the runner.

use std::process::Command;

use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, ConversationHandle, CreateSandboxRequest, ExoHarness,
    NewAgentRequest, NewConversationRequest, RunInSandboxRequest, SandboxBackendRegistration,
    SandboxId, SandboxProvider, SecretBackendChoice, StartSandboxRequest,
};
use futures::io::AsyncReadExt;
use tempfile::TempDir;

/// Default sandbox image. We pin explicitly here rather than relying on
/// `DEFAULT_SANDBOX_IMAGE` so a future change to the default doesn't
/// silently affect what this test exercises.
const SANDBOX_IMAGE: &str = "docker.io/library/ubuntu:24.04";

#[tokio::test]
#[ignore = "spawns a real docker container; run with `cargo test -- --ignored`"]
async fn filesystem_snapshot_and_rewind_round_trip() {
    if !preflight() {
        return;
    }

    let root_dir = TempDir::new().expect("tempdir for harness root");
    let harness = BasicExoHarness::new(BasicExoHarnessConfig {
        root: root_dir.path().to_path_buf(),
        // Static cipher key keeps the test off the filesystem for secret state;
        // it's orthogonal to what we're testing (sandbox snapshots).
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::Docker,
        sandbox_backends: vec![SandboxBackendRegistration::docker()],
    })
    .await
    .expect("BasicExoHarness::new should succeed");

    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "snap-test-agent".into(),
            name: "snap-test agent".into(),
        })
        .await
        .expect("new_agent");
    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some("snap-test-conv".into()),
            name: Some("snap-test conversation".into()),
        })
        .await
        .expect("new_conversation");

    // ───── Phase 1: create a docker sandbox and write the initial state ─────
    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::Docker,
            image: SANDBOX_IMAGE.into(),
            resources: Default::default(),
            default_workdir: Some("/".into()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(false),
            idle_seconds: Some(60),
        })
        .await
        .expect("create_sandbox");

    let (rc, _, _) = exec_shell(
        conversation.as_ref(),
        &sandbox_id,
        "echo 'version 1' > /tmp/demo.txt",
    )
    .await;
    assert_eq!(rc, 0, "writing v1 should succeed");

    let (rc, stdout, _) = exec_shell(conversation.as_ref(), &sandbox_id, "cat /tmp/demo.txt").await;
    assert_eq!(rc, 0);
    assert_eq!(stdout.trim(), "version 1");

    // ───── Phase 2: capture a snapshot of the sandbox at v1 ─────
    let snapshot_id = conversation
        .snapshot_sandbox(sandbox_id.clone())
        .await
        .expect("snapshot_sandbox should succeed");

    // Verify the on-disk artefacts. Snapshots are stored under
    //   <root>/agents/<agent>/conversations/<conv>/snapshots/<snapshot-id>/
    // as a `manifest.json` + `payload.bin` pair. Using read_dir() here means
    // the assertions document the layout without hard-coding the
    // (test-generated) agent/conversation UUIDs.
    let snapshot_dir = find_single_subdir(
        &find_single_subdir(&root_dir.path().join("agents")).join("conversations"),
    )
    .join("snapshots")
    .join(snapshot_id.to_string());
    assert!(
        snapshot_dir.exists(),
        "snapshot directory should exist at {}",
        snapshot_dir.display()
    );
    assert!(
        snapshot_dir.join("manifest.json").exists(),
        "snapshot manifest.json should exist"
    );
    let payload = snapshot_dir.join("payload.bin");
    assert!(payload.exists(), "snapshot payload.bin should exist");
    assert!(
        payload.metadata().unwrap().len() > 0,
        "snapshot payload.bin should not be empty"
    );

    // ───── Phase 3: mutate the sandbox after the snapshot ─────
    //
    // Two distinct mutations so the rewind correctness check has two
    // independent signals:
    //   (a) /tmp/demo.txt is overwritten — rewind must roll its content back
    //   (b) /tmp/post-snapshot.txt is created — rewind must make it disappear
    let (rc, _, _) = exec_shell(
        conversation.as_ref(),
        &sandbox_id,
        "echo 'version 2' > /tmp/demo.txt && touch /tmp/post-snapshot.txt",
    )
    .await;
    assert_eq!(rc, 0);

    let (_, stdout, _) = exec_shell(conversation.as_ref(), &sandbox_id, "cat /tmp/demo.txt").await;
    assert_eq!(
        stdout.trim(),
        "version 2",
        "before rewind, file should be v2"
    );

    let (rc, _, _) = exec_shell(
        conversation.as_ref(),
        &sandbox_id,
        "test -f /tmp/post-snapshot.txt",
    )
    .await;
    assert_eq!(rc, 0, "post-snapshot file should exist before rewind");

    // ───── Phase 4: rewind to the snapshot ─────
    conversation
        .start_sandbox(StartSandboxRequest {
            id: sandbox_id.clone(),
            snapshot_id,
            idle_seconds: None,
            provider: None,
        })
        .await
        .expect("start_sandbox (rewind) should succeed");

    // ───── Phase 5: prove the rewind landed ─────
    let (_, stdout, _) = exec_shell(conversation.as_ref(), &sandbox_id, "cat /tmp/demo.txt").await;
    assert_eq!(
        stdout.trim(),
        "version 1",
        "after rewind, /tmp/demo.txt should be 'version 1' again"
    );

    let (rc, _, _) = exec_shell(
        conversation.as_ref(),
        &sandbox_id,
        "test -f /tmp/post-snapshot.txt",
    )
    .await;
    assert_ne!(rc, 0, "after rewind, the post-snapshot file should be gone");

    // ───── Cleanup ─────
    //
    // Explicitly stop the sandbox so the docker container is removed instead
    // of leaking past the test's lifetime. TempDir handles the on-disk side.
    let _ = conversation.stop_sandbox(sandbox_id).await;
}

/// Run a shell command in the conversation's sandbox and collect everything.
/// Drives the streaming `SandboxProcess` to completion and returns the exit
/// code along with full stdout/stderr.
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

fn preflight() -> bool {
    // Bail early if docker isn't usable.
    let docker_ok = Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !docker_ok {
        eprintln!("docker not available, skipping filesystem-snapshot round-trip test");
        return false;
    }

    // The integration workflow runs this test on every (os, sandbox-backend)
    // matrix cell — including `*/local-process`. Skip cleanly for cells that
    // aren't testing the docker backend instead of failing them. macOS+docker
    // cells (Colima via the Intel runner) exercise this test exactly like
    // Linux+docker cells.
    let backend = std::env::var("EXO_TEST_SANDBOX_BACKEND").unwrap_or_else(|_| "docker".into());
    if backend != "docker" {
        eprintln!(
            "filesystem-snapshot round-trip test is docker-only; skipping \
             (EXO_TEST_SANDBOX_BACKEND={backend})"
        );
        return false;
    }

    true
}

/// Read a directory expected to contain exactly one record entry and return
/// that entry's path. Used to walk into the per-agent / per-conversation
/// directories without hardcoding the test-generated UUIDs. Skips `by-slug`,
/// the slug-uniqueness index the harness keeps next to the agent record
/// directories.
fn find_single_subdir(parent: &std::path::Path) -> std::path::PathBuf {
    let mut entries: Vec<_> = std::fs::read_dir(parent)
        .unwrap_or_else(|error| panic!("read_dir({}) failed: {error:#}", parent.display()))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != "by-slug")
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one record entry under {}, found {}",
        parent.display(),
        entries.len()
    );
    entries.pop().unwrap().path()
}

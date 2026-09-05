//! Live native Daytona snapshot + rewind round-trip, through the harness API.
//! The Daytona analog of `snapshot_round_trip.rs` (Docker): create a Daytona
//! sandbox, write a file, `snapshot_sandbox` (native checkpoint), mutate,
//! `start_sandbox` to rewind, and confirm the filesystem reverted.
//!
//! Ignored by default. Requires `DAYTONA_API_KEY` (+ optional
//! `DAYTONA_ORGANIZATION_ID` / `DAYTONA_TARGET`; sandbox snapshots are
//! available in the shared `us` region). Run with:
//!
//! ```bash
//! cargo test -p exo --test snapshot_round_trip_daytona -- --ignored --nocapture
//! ```

use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, ConversationHandle, CreateSandboxRequest,
    DaytonaBackendSpec, ExoHarness, NewAgentRequest, NewConversationRequest, PutSecretRequest,
    RunInSandboxRequest, SandboxBackendRegistration, SandboxId, SandboxProvider, Secret,
    SecretBackendChoice, StartSandboxRequest,
};
use futures::io::AsyncReadExt;
use tempfile::TempDir;

#[tokio::test]
#[ignore = "live: needs DAYTONA_API_KEY (snapshots available in the shared `us` region)"]
async fn daytona_snapshot_and_rewind_round_trip() {
    let Ok(api_key) = std::env::var("DAYTONA_API_KEY") else {
        eprintln!("skipping: DAYTONA_API_KEY not set");
        return;
    };

    let root = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(BasicExoHarnessConfig {
        root: root.path().to_path_buf(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::Daytona,
        sandbox_backends: vec![SandboxBackendRegistration::daytona(
            DaytonaBackendSpec::with_conventional_secrets(),
        )],
    })
    .await
    .expect("harness");

    seed_secret(&harness, "DAYTONA_API_KEY", &api_key).await;
    if let Ok(v) = std::env::var("DAYTONA_ORGANIZATION_ID") {
        seed_secret(&harness, "DAYTONA_ORGANIZATION_ID", &v).await;
    }
    if let Ok(v) = std::env::var("DAYTONA_TARGET") {
        seed_secret(&harness, "DAYTONA_TARGET", &v).await;
    }

    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "daysnap".into(),
            name: "daysnap".into(),
        })
        .await
        .expect("agent");
    let conv = agent
        .new_conversation(NewConversationRequest {
            slug: Some("daysnap-conv".into()),
            name: Some("daysnap".into()),
        })
        .await
        .expect("conversation");

    // Phase 1: create a Daytona sandbox and write v1.
    let sandbox_id = conv
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::Daytona,
            image: String::new(),
            default_workdir: Some("/".into()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            network_policy: None,
            idle_seconds: Some(300),
        })
        .await
        .expect("create daytona sandbox");
    let (rc, _, _) = exec_shell(
        conv.as_ref(),
        &sandbox_id,
        "echo 'version 1' > /tmp/demo.txt",
    )
    .await;
    assert_eq!(rc, 0, "writing v1 should succeed");

    // Phase 2: snapshot (native Daytona checkpoint).
    let snapshot_id = conv
        .snapshot_sandbox(sandbox_id.clone())
        .await
        .expect("snapshot_sandbox");

    // Phase 3: mutate after the snapshot.
    let (rc, _, _) = exec_shell(
        conv.as_ref(),
        &sandbox_id,
        "echo 'version 2' > /tmp/demo.txt && touch /tmp/post-snapshot.txt",
    )
    .await;
    assert_eq!(rc, 0);
    let (_, stdout, _) = exec_shell(conv.as_ref(), &sandbox_id, "cat /tmp/demo.txt").await;
    assert_eq!(
        stdout.trim(),
        "version 2",
        "before rewind, file should be v2"
    );

    // Phase 4: rewind to the snapshot.
    conv.start_sandbox(StartSandboxRequest {
        id: sandbox_id.clone(),
        snapshot_id,
        idle_seconds: None,
        provider: None,
    })
    .await
    .expect("start_sandbox (rewind)");

    // Phase 5: prove the rewind landed.
    let (_, stdout, _) = exec_shell(conv.as_ref(), &sandbox_id, "cat /tmp/demo.txt").await;
    eprintln!("after rewind, /tmp/demo.txt = {stdout:?}");
    assert_eq!(
        stdout.trim(),
        "version 1",
        "after rewind, /tmp/demo.txt should be 'version 1' again"
    );
    let (rc, _, _) = exec_shell(conv.as_ref(), &sandbox_id, "test -f /tmp/post-snapshot.txt").await;
    assert_ne!(rc, 0, "after rewind, the post-snapshot file should be gone");

    let _ = conv.stop_sandbox(sandbox_id).await;
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

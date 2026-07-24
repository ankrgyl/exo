//! The Groundhog-backed conversation event store, exercised through the
//! public harness API against a real `groundhog serve` process — including
//! exo's own harness contract tests.
//!
//! Tests self-skip when no groundhog binary is available: set `GROUNDHOG_BIN`
//! or build ground-core at the fallback path below.

#![cfg(feature = "basic-backend")]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use exoharness::{
    AddEventsRequest, BasicExoHarness, BasicExoHarnessConfig, EventData, ExoHarness,
    GroundhogStoreConfig, NewAgentRequest, NewConversationRequest, SandboxBackendRegistration,
    SandboxProvider, SecretBackendChoice,
};

const FALLBACK_GROUNDHOG_BIN: &str = "/Users/arvind/GroundCo/ground-core/target/debug/groundhog";

fn groundhog_bin() -> Option<PathBuf> {
    let bin = std::env::var_os("GROUNDHOG_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(FALLBACK_GROUNDHOG_BIN));
    bin.exists().then_some(bin)
}

/// A `groundhog init` + `groundhog serve` child, killed on drop.
struct GroundhogServer {
    child: Child,
    socket: PathBuf,
    #[expect(dead_code, reason = "removes the data dir when the test ends")]
    dir: tempfile::TempDir,
}

impl Drop for GroundhogServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_groundhog(bin: &PathBuf) -> GroundhogServer {
    let dir = tempfile::Builder::new()
        .prefix("gh-store-")
        .tempdir_in(std::env::temp_dir())
        .expect("temp dir");
    let status = Command::new(bin)
        .arg("init")
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("groundhog init");
    assert!(status.success(), "groundhog init failed");
    let child = Command::new(bin)
        .arg("serve")
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("groundhog serve");
    let socket = dir.path().join("data/ground.sock");
    assert!(
        socket.as_os_str().len() < 100,
        "socket path too long for sockaddr_un: {}",
        socket.display()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "groundhog socket never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
    GroundhogServer { child, socket, dir }
}

fn harness_config(root: PathBuf, socket: PathBuf) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root,
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
        groundhog: Some(GroundhogStoreConfig {
            socket,
            source: "exo".to_string(),
        }),
    }
}

macro_rules! skip_without_groundhog {
    () => {
        match groundhog_bin() {
            Some(bin) => bin,
            None => {
                eprintln!("skipping: no groundhog binary (set GROUNDHOG_BIN)");
                return;
            }
        }
    };
}

/// Exo's own harness contract tests, with every conversation event stored in
/// and served from Groundhog.
#[tokio::test]
async fn contract_tests_pass_with_groundhog_event_store() {
    let bin = skip_without_groundhog!();
    let server = spawn_groundhog(&bin);
    let root = tempfile::tempdir().expect("root");
    let harness: Arc<dyn ExoHarness> = Arc::new(
        BasicExoHarness::new(harness_config(
            root.path().to_path_buf(),
            server.socket.clone(),
        ))
        .await
        .expect("harness"),
    );
    exoharness::contract_tests::supports_agent_and_conversation_crud(Arc::clone(&harness)).await;
    exoharness::contract_tests::list_conversations_returns_recent_first_and_paginates(Arc::clone(
        &harness,
    ))
    .await;
    exoharness::contract_tests::begin_turn_tracks_events_through_finish(Arc::clone(&harness)).await;
    exoharness::contract_tests::turn_events_continue_after_artifact_writes(Arc::clone(&harness))
        .await;
    exoharness::contract_tests::conversation_scope_overrides_agent_scope_and_fork_copies_bindings(
        Arc::clone(&harness),
    )
    .await;
}

/// The log is the only copy: no local event files exist, and a fresh harness
/// process on the same root serves the full history from Groundhog replay.
#[tokio::test]
async fn history_survives_restart_with_no_local_event_files() {
    let bin = skip_without_groundhog!();
    let server = spawn_groundhog(&bin);
    let root = tempfile::tempdir().expect("root");

    let (agent_id, conversation_id, event_ids) = {
        let harness = BasicExoHarness::new(harness_config(
            root.path().to_path_buf(),
            server.socket.clone(),
        ))
        .await
        .expect("harness");
        let agent = harness
            .new_agent(NewAgentRequest {
                slug: "demo".into(),
                name: "Demo".into(),
            })
            .await
            .expect("agent");
        let conversation = agent
            .new_conversation(NewConversationRequest::default())
            .await
            .expect("conversation");
        let added = conversation
            .add_events(AddEventsRequest {
                session_id: None,
                turn_id: None,
                data: vec![
                    EventData::Custom {
                        event_type: "demo_note".into(),
                        payload: serde_json::json!({"n": 1}),
                    },
                    EventData::Custom {
                        event_type: "demo_note".into(),
                        payload: serde_json::json!({"n": 2}),
                    },
                ],
            })
            .await
            .expect("append");
        (agent.record().id, conversation.record().id, added.event_ids)
    };

    // Nothing under the root may contain a per-event JSON file.
    let events_dirs = walkdir(root.path())
        .into_iter()
        .filter(|path| path.ends_with("events"))
        .collect::<Vec<_>>();
    assert_eq!(
        events_dirs,
        Vec::<PathBuf>::new(),
        "groundhog mode must not write local event files"
    );

    // A fresh harness on the same root reads everything back from the log.
    let harness = BasicExoHarness::new(harness_config(
        root.path().to_path_buf(),
        server.socket.clone(),
    ))
    .await
    .expect("harness restart");
    let agent = harness
        .get_agent(&agent_id)
        .await
        .expect("get agent")
        .expect("agent exists");
    let conversation = agent
        .get_conversation(&conversation_id)
        .await
        .expect("get conversation")
        .expect("conversation exists");
    let events = conversation.get_events(None).await.expect("events");
    let replayed_ids = events
        .events
        .iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();
    assert!(
        event_ids
            .iter()
            .all(|event_id| replayed_ids.contains(event_id)),
        "appended events must survive the restart"
    );
    let looked_up = conversation
        .get_event(event_ids[0])
        .await
        .expect("get_event");
    assert!(looked_up.is_some(), "point lookup must work after restart");
}

/// A second writer on the same stream is detected by the frontier
/// precondition instead of silently interleaving history.
#[tokio::test]
async fn concurrent_writer_is_detected_by_frontier_precondition() {
    let bin = skip_without_groundhog!();
    let server = spawn_groundhog(&bin);
    let root = tempfile::tempdir().expect("root");

    let harness_a = BasicExoHarness::new(harness_config(
        root.path().to_path_buf(),
        server.socket.clone(),
    ))
    .await
    .expect("harness a");
    let agent_a = harness_a
        .new_agent(NewAgentRequest {
            slug: "demo".into(),
            name: "Demo".into(),
        })
        .await
        .expect("agent");
    let conversation_a = agent_a
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");
    let note = |n: u64| AddEventsRequest {
        session_id: None,
        turn_id: None,
        data: vec![EventData::Custom {
            event_type: "demo_note".into(),
            payload: serde_json::json!({ "n": n }),
        }],
    };
    conversation_a.add_events(note(1)).await.expect("a writes");

    // Harness B opens the same root and stream, seeds the true frontier, and
    // appends; A's cached frontier is now stale.
    let harness_b = BasicExoHarness::new(harness_config(
        root.path().to_path_buf(),
        server.socket.clone(),
    ))
    .await
    .expect("harness b");
    let agent_b = harness_b
        .get_agent(&agent_a.record().id)
        .await
        .expect("get agent")
        .expect("agent exists");
    let conversation_b = agent_b
        .get_conversation(&conversation_a.record().id)
        .await
        .expect("get conversation")
        .expect("conversation exists");
    conversation_b.add_events(note(2)).await.expect("b writes");

    let error = conversation_a
        .add_events(note(3))
        .await
        .expect_err("stale writer must be rejected");
    assert!(
        error.to_string().contains("another writer"),
        "unexpected error: {error}"
    );
}

fn walkdir(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.push(path.clone());
                pending.push(path);
            }
        }
    }
    found
}

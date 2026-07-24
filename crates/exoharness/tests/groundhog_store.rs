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
            kernel_config: None,
        }),
    }
}

/// `harness_config` with the identity bound to a kernel-config file.
fn kernel_bound_config(root: PathBuf, socket: PathBuf, kernel: PathBuf) -> BasicExoHarnessConfig {
    let mut config = harness_config(root, socket);
    config
        .groundhog
        .as_mut()
        .expect("groundhog config present")
        .kernel_config = Some(kernel);
    config
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

fn note(n: u64) -> AddEventsRequest {
    AddEventsRequest {
        session_id: None,
        turn_id: None,
        data: vec![EventData::Custom {
            event_type: "demo_note".into(),
            payload: serde_json::json!({ "n": n }),
        }],
    }
}

/// Sources under the kernel-bound identity prefix, sorted, deduplicated.
async fn kernel_sources(client: &exoharness::groundhog::GroundhogClient) -> Vec<String> {
    let mut sources: Vec<String> = client
        .streams(None)
        .await
        .expect("streams")
        .into_iter()
        .map(|info| info.source)
        .filter(|source| source.starts_with("exo.k"))
        .collect();
    sources.sort();
    sources.dedup();
    sources
}

/// Changing the kernel config file retires the old identity's log, records
/// succession, and serves the full conversation history across the seam.
#[tokio::test]
async fn kernel_flip_retires_predecessor_and_preserves_history() {
    let bin = skip_without_groundhog!();
    let server = spawn_groundhog(&bin);
    let root = tempfile::tempdir().expect("root");
    let kernel = root.path().join("kernel.toml");
    std::fs::write(&kernel, "mutability = \"full\"\n").expect("kernel v1");

    let (agent_id, conversation_id, v1_event_ids) = {
        let harness = BasicExoHarness::new(kernel_bound_config(
            root.path().to_path_buf(),
            server.socket.clone(),
            kernel.clone(),
        ))
        .await
        .expect("harness v1");
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
        let added = conversation.add_events(note(1)).await.expect("v1 write");
        (agent.record().id, conversation.record().id, added.event_ids)
    };

    let client = exoharness::groundhog::GroundhogClient::new(server.socket.clone());
    let sources_before = kernel_sources(&client).await;
    assert_eq!(sources_before.len(), 1, "one identity before the flip");
    let old_source = sources_before[0].clone();

    // The kernel contract changes; the next harness is a different identity.
    std::fs::write(&kernel, "mutability = \"frozen\"\n").expect("kernel v2");
    let harness = BasicExoHarness::new(kernel_bound_config(
        root.path().to_path_buf(),
        server.socket.clone(),
        kernel.clone(),
    ))
    .await
    .expect("harness v2");
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

    // History spans the retired identity; the point lookup crosses the seam.
    let events = conversation.get_events(None).await.expect("events");
    let replayed_ids: Vec<_> = events.events.iter().map(|event| event.id).collect();
    assert!(
        v1_event_ids.iter().all(|id| replayed_ids.contains(id)),
        "pre-flip history must remain readable"
    );
    assert!(
        conversation
            .get_event(v1_event_ids[0])
            .await
            .expect("get_event")
            .is_some(),
        "point lookup must cross the lineage seam"
    );

    // The successor keeps writing; the predecessor admits nothing.
    conversation.add_events(note(2)).await.expect("v2 write");
    let sources_after = kernel_sources(&client).await;
    assert_eq!(sources_after.len(), 2, "both identities visible in the log");
    let new_source = sources_after
        .iter()
        .find(|source| **source != old_source)
        .expect("successor source")
        .clone();
    let refused = client
        .append(exoharness::groundhog::IngestBatch {
            batch_id: "post-retirement".into(),
            source: old_source.clone(),
            events: vec![exoharness::groundhog::IngestEvent {
                stream: "x1".into(),
                record_key: "x1".into(),
                kind: "demo_note".into(),
                occurred_at: None,
                payload: serde_json::json!(1),
            }],
            stream_precondition: None,
        })
        .await
        .expect_err("retired source must refuse appends");
    assert!(
        matches!(
            refused,
            exoharness::groundhog::GroundhogError::SourceRetired { .. }
        ),
        "unexpected error: {refused}"
    );

    // Succession is recorded as the successor's first event.
    let markers = client
        .replay_all(&exoharness::groundhog::ReplayQuery {
            source: Some(new_source.clone()),
            stream: Some(exoharness::groundhog::LINEAGE_STREAM.to_owned()),
            kind: Some(exoharness::groundhog::LINEAGE_KIND.to_owned()),
            ..Default::default()
        })
        .await
        .expect("lineage replay");
    assert_eq!(markers.len(), 1, "exactly one succession marker");
    let marker: exoharness::groundhog::LineageMarker =
        serde_json::from_value(markers[0].payload.clone()).expect("marker decodes");
    assert_eq!(marker.predecessor_source, old_source);
}

/// An unchanged kernel config is the same identity: restarts reuse the
/// source and never write lineage.
#[tokio::test]
async fn same_kernel_restart_reuses_source_without_lineage() {
    let bin = skip_without_groundhog!();
    let server = spawn_groundhog(&bin);
    let root = tempfile::tempdir().expect("root");
    let kernel = root.path().join("kernel.toml");
    std::fs::write(&kernel, "mutability = \"full\"\n").expect("kernel");

    let (agent_id, conversation_id) = {
        let harness = BasicExoHarness::new(kernel_bound_config(
            root.path().to_path_buf(),
            server.socket.clone(),
            kernel.clone(),
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
        conversation.add_events(note(1)).await.expect("write");
        (agent.record().id, conversation.record().id)
    };

    let harness = BasicExoHarness::new(kernel_bound_config(
        root.path().to_path_buf(),
        server.socket.clone(),
        kernel.clone(),
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
    conversation.add_events(note(2)).await.expect("write again");

    let client = exoharness::groundhog::GroundhogClient::new(server.socket.clone());
    assert_eq!(
        kernel_sources(&client).await.len(),
        1,
        "restart must not mint a new identity"
    );
    let lineage_rows: Vec<_> = client
        .streams(None)
        .await
        .expect("streams")
        .into_iter()
        .filter(|info| info.stream == exoharness::groundhog::LINEAGE_STREAM)
        .collect();
    assert_eq!(
        lineage_rows,
        vec![],
        "no succession without a kernel change"
    );
}

/// Two kernel changes leave a three-identity chain; reads traverse all of it.
#[tokio::test]
async fn second_flip_walks_the_full_lineage_chain() {
    let bin = skip_without_groundhog!();
    let server = spawn_groundhog(&bin);
    let root = tempfile::tempdir().expect("root");
    let kernel = root.path().join("kernel.toml");

    let mut agent_id = None;
    let mut conversation_id = None;
    for generation in 1..=3u64 {
        std::fs::write(&kernel, format!("generation = {generation}\n")).expect("kernel");
        let harness = BasicExoHarness::new(kernel_bound_config(
            root.path().to_path_buf(),
            server.socket.clone(),
            kernel.clone(),
        ))
        .await
        .expect("harness");
        let (agent, conversation) = match (agent_id, conversation_id) {
            (None, None) => {
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
                agent_id = Some(agent.record().id);
                conversation_id = Some(conversation.record().id);
                (agent, conversation)
            }
            (Some(agent_id), Some(conversation_id)) => {
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
                (agent, conversation)
            }
            _ => unreachable!("ids are set together"),
        };
        let _ = agent;
        conversation
            .add_events(note(generation))
            .await
            .expect("write");
        let events = conversation.get_events(None).await.expect("events");
        assert_eq!(
            events.events.len() as u64,
            // One conversation_created event, then one note per generation.
            1 + generation,
            "generation {generation} must see the whole chain"
        );
    }

    let client = exoharness::groundhog::GroundhogClient::new(server.socket.clone());
    assert_eq!(
        kernel_sources(&client).await.len(),
        3,
        "three identities in the log"
    );
}

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use anyhow::bail;
use async_trait::async_trait;
use bytes::Bytes;
use futures::io::AsyncReadExt;
use tempfile::TempDir;

use crate::test_support::local_test_config;
use crate::{
    BasicExoHarness, BeginTurnRequest, CreateSandboxRequest, EventData, EventKind, EventQuery,
    EventQueryDirection, ExoHarness, HttpExoHarness, ManagedSandboxBackend, ManagedSandboxHandle,
    RunInSandboxRequest, SandboxAttachment, SandboxCommand, SandboxCommandOutput,
    SandboxProcessEvent, SandboxProcessEventQuery, SandboxProcessParts, SandboxProcessStatus,
    SandboxProcessStdin, SandboxProvider, SandboxRequest, SnapshotKind, SnapshotPayload,
    StartSandboxProcessRequest, StartSandboxRequest, WaitSandboxProcessRequest,
    WriteSandboxProcessInputRequest, serve_exoharness_http_listener,
};

struct HttpHarnessFixture {
    harness: Arc<dyn ExoHarness>,
    server: actix_web::rt::task::JoinHandle<crate::Result<()>>,
    _tempdir: TempDir,
}

impl Drop for HttpHarnessFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn http_harness() -> HttpHarnessFixture {
    let tempdir = TempDir::new().expect("tempdir");
    let basic = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("basic harness");
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("listener");
    let addr = listener.local_addr().expect("local addr");
    let server = actix_web::rt::spawn(serve_exoharness_http_listener(listener, Arc::new(basic)));
    let harness: Arc<dyn ExoHarness> =
        Arc::new(HttpExoHarness::new(format!("http://{addr}")).expect("http harness"));

    HttpHarnessFixture {
        harness,
        server,
        _tempdir: tempdir,
    }
}

async fn http_harness_with_sandbox_backend(
    backend: Arc<dyn ManagedSandboxBackend>,
) -> HttpHarnessFixture {
    let tempdir = TempDir::new().expect("tempdir");
    let basic =
        BasicExoHarness::new_with_sandbox_backend(local_test_config(tempdir.path()), backend)
            .await
            .expect("basic harness");
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("listener");
    let addr = listener.local_addr().expect("local addr");
    let server = actix_web::rt::spawn(serve_exoharness_http_listener(listener, Arc::new(basic)));
    let harness: Arc<dyn ExoHarness> =
        Arc::new(HttpExoHarness::new(format!("http://{addr}")).expect("http harness"));

    HttpHarnessFixture {
        harness,
        server,
        _tempdir: tempdir,
    }
}

#[actix_web::test]
async fn http_exoharness_supports_agent_and_conversation_crud() {
    let fixture = http_harness().await;
    crate::contract_tests::supports_agent_and_conversation_crud(Arc::clone(&fixture.harness)).await;
}

#[actix_web::test]
async fn http_exoharness_supports_thread_and_conversation_apis() {
    let fixture = http_harness().await;
    crate::contract_tests::supports_thread_api_and_conversation_compatibility(Arc::clone(
        &fixture.harness,
    ))
    .await;
}

#[actix_web::test]
async fn http_exoharness_lists_conversations_recent_first_and_paginates() {
    let fixture = http_harness().await;
    crate::contract_tests::list_conversations_returns_recent_first_and_paginates(Arc::clone(
        &fixture.harness,
    ))
    .await;
}

#[actix_web::test]
async fn http_exoharness_begin_turn_tracks_events_through_finish() {
    let fixture = http_harness().await;
    crate::contract_tests::begin_turn_tracks_events_through_finish(Arc::clone(&fixture.harness))
        .await;
}

#[actix_web::test]
async fn http_exoharness_turn_events_continue_after_artifact_writes() {
    let fixture = http_harness().await;
    crate::contract_tests::turn_events_continue_after_artifact_writes(Arc::clone(&fixture.harness))
        .await;
}

#[actix_web::test]
async fn http_exoharness_conversation_scope_overrides_and_forks() {
    let fixture = http_harness().await;
    crate::contract_tests::conversation_scope_overrides_agent_scope_and_fork_copies_bindings(
        Arc::clone(&fixture.harness),
    )
    .await;
}

#[actix_web::test]
#[ignore = "set EXO_CONTRACT_TEST_URL and optional EXO_CONTRACT_TEST_BEARER or EXO_CONTRACT_TEST_BEARER_ENV"]
async fn hosted_http_exoharness_core_contract() {
    let harness = hosted_harness_from_env();
    crate::contract_tests::supports_agent_and_conversation_crud(Arc::clone(&harness)).await;
    crate::contract_tests::begin_turn_tracks_events_through_finish(Arc::clone(&harness)).await;
    crate::contract_tests::turn_events_continue_after_artifact_writes(Arc::clone(&harness)).await;
}

#[actix_web::test]
async fn http_exoharness_runs_noninteractive_sandbox_commands() {
    let fixture = http_harness().await;
    let agent = fixture
        .harness
        .new_agent(crate::NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(crate::NewConversationRequest::default())
        .await
        .expect("conversation");
    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "local".to_string(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox");
    assert_eq!(
        conversation.list_sandboxes().await.expect("list sandboxes")[0].id,
        sandbox_id
    );
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "printf hello".to_string(),
            ],
            env: Default::default(),
        })
        .await
        .expect("sandbox command");
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut output = Vec::new();
    stdout.read_to_end(&mut output).await.expect("stdout");
    assert_eq!(output, b"hello");
    assert_eq!(parts.wait.await.expect("exit"), 0);
}

#[actix_web::test]
async fn http_exoharness_runs_agent_scoped_sandbox_commands() {
    let fixture = http_harness().await;
    let agent = fixture
        .harness
        .new_agent(crate::NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(crate::NewConversationRequest::default())
        .await
        .expect("conversation");
    let sandbox_id = agent
        .create_sandbox(CreateSandboxRequest {
            name: Some("agent-http".to_string()),
            provider: SandboxProvider::LocalProcess,
            image: "local".to_string(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("agent sandbox");
    let process = agent
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "printf agent-http".to_string(),
            ],
            env: Default::default(),
        })
        .await
        .expect("agent sandbox command");
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut output = Vec::new();
    stdout.read_to_end(&mut output).await.expect("stdout");
    assert_eq!(output, b"agent-http");
    assert_eq!(parts.wait.await.expect("exit"), 0);

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED, EventKind::SANDBOX_STARTED]),
        }))
        .await
        .expect("events")
        .events;
    assert!(events.is_empty());

    let conversation_process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec!["true".to_string()],
            env: Default::default(),
        })
        .await;
    assert!(conversation_process.is_err());
}

#[actix_web::test]
async fn http_exoharness_supports_sandbox_process_events() {
    let fixture = http_harness().await;
    let agent = fixture
        .harness
        .new_agent(crate::NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(crate::NewConversationRequest::default())
        .await
        .expect("conversation");
    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "local".to_string(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox");
    let process = conversation
        .start_sandbox_process(StartSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            name: None,
            command: vec!["/bin/sh".to_string(), "-lc".to_string(), "cat".to_string()],
            env: Default::default(),
            cwd: None,
            mode: Default::default(),
            stdin: SandboxProcessStdin::Open,
            output: Default::default(),
            lifecycle: Default::default(),
        })
        .await
        .expect("process");
    conversation
        .write_sandbox_process_input(WriteSandboxProcessInputRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
            data: b"hello http process".to_vec(),
        })
        .await
        .expect("stdin write");
    conversation
        .close_sandbox_process_input(crate::CloseSandboxProcessInputRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
        })
        .await
        .expect("stdin close");
    let status = conversation
        .wait_sandbox_process(WaitSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
        })
        .await
        .expect("wait");
    assert_eq!(status, SandboxProcessStatus::Exited { exit_code: 0 });

    let events = conversation
        .get_sandbox_process_events(SandboxProcessEventQuery {
            sandbox_id,
            process_id: process.id,
            after: None,
            limit: None,
            follow: None,
        })
        .await
        .expect("events");
    assert!(events.events.iter().any(|event| matches!(
        event,
        SandboxProcessEvent::Stdout { data, .. }
            if String::from_utf8_lossy(data).contains("hello http process")
    )));
    assert!(matches!(
        events.events.last(),
        Some(SandboxProcessEvent::Exit { exit_code: 0, .. })
    ));
}

#[actix_web::test]
async fn http_exoharness_supports_turn_scoped_sandbox_snapshot_and_start() {
    let fixture = http_harness_with_sandbox_backend(Arc::new(SnapshotTestSandboxBackend)).await;
    let agent = fixture
        .harness
        .new_agent(crate::NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(crate::NewConversationRequest::default())
        .await
        .expect("conversation");
    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "local".to_string(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox");
    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: Vec::new(),
        })
        .await
        .expect("turn");

    let snapshot_id = turn
        .snapshot_sandbox(sandbox_id.clone())
        .await
        .expect("turn snapshot");
    turn.start_sandbox(StartSandboxRequest {
        id: sandbox_id.clone(),
        snapshot_id,
        idle_seconds: Some(60),
        provider: None,
    })
    .await
    .expect("turn start from snapshot");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![
                EventKind::SANDBOX_SNAPSHOTTED,
                EventKind::SANDBOX_STARTED,
            ]),
        }))
        .await
        .expect("events")
        .events;

    let snapshot_event = events
        .iter()
        .find(|event| matches!(event.data, EventData::SandboxSnapshotted { .. }))
        .expect("snapshot event");
    assert_eq!(snapshot_event.session_id, Some(turn.record().session_id));
    assert_eq!(snapshot_event.turn_id, Some(turn.record().id));

    let restored_start_event = events
        .iter()
        .find(|event| {
            matches!(
                event.data,
                EventData::SandboxStarted {
                    snapshot_id: Some(_),
                    ..
                }
            )
        })
        .expect("snapshot-backed start event");
    assert_eq!(
        restored_start_event.session_id,
        Some(turn.record().session_id)
    );
    assert_eq!(restored_start_event.turn_id, Some(turn.record().id));
}

struct SnapshotTestSandboxBackend;

#[async_trait]
impl ManagedSandboxBackend for SnapshotTestSandboxBackend {
    fn is_local(&self) -> bool {
        false
    }

    async fn acquire(
        &self,
        _request: SandboxRequest,
    ) -> crate::Result<Arc<dyn ManagedSandboxHandle>> {
        Ok(Arc::new(SnapshotTestSandboxHandle))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> crate::Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("snapshot test backend does not support attachment")
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> crate::Result<Arc<dyn ManagedSandboxHandle>> {
        assert_eq!(payload.bytes, Bytes::from_static(b"snapshot"));
        Ok(Arc::new(SnapshotTestSandboxHandle))
    }
}

struct SnapshotTestSandboxHandle;

#[async_trait]
impl ManagedSandboxHandle for SnapshotTestSandboxHandle {
    fn id(&self) -> &str {
        "snapshot-test-sandbox"
    }

    async fn exec(&self, _command: &SandboxCommand) -> crate::Result<SandboxCommandOutput> {
        bail!("snapshot test sandbox does not support exec")
    }

    async fn start_process(&self, _command: &SandboxCommand) -> crate::Result<SandboxProcessParts> {
        bail!("snapshot test sandbox does not support start_process")
    }

    async fn stop(&self) -> crate::Result<()> {
        Ok(())
    }

    async fn detach(&self) -> crate::Result<SandboxAttachment> {
        bail!("snapshot test handle does not support detachment")
    }

    async fn snapshot(&self) -> crate::Result<SnapshotPayload> {
        Ok(SnapshotPayload {
            kind: SnapshotKind::DaytonaSnapshot,
            bytes: Bytes::from_static(b"snapshot"),
        })
    }
}

fn hosted_harness_from_env() -> Arc<dyn ExoHarness> {
    let url = std::env::var("EXO_CONTRACT_TEST_URL")
        .expect("EXO_CONTRACT_TEST_URL must point at an ExoHarness HTTP endpoint");
    let mut harness = HttpExoHarness::new(url).expect("hosted http harness");
    if let Ok(token) = std::env::var("EXO_CONTRACT_TEST_BEARER") {
        harness = harness.with_bearer_token(token);
    } else if let Ok(env_name) = std::env::var("EXO_CONTRACT_TEST_BEARER_ENV") {
        let token = std::env::var(&env_name).unwrap_or_else(|_| {
            panic!("EXO_CONTRACT_TEST_BEARER_ENV references unset environment variable {env_name}")
        });
        harness = harness.with_bearer_token(token);
    }
    Arc::new(harness)
}

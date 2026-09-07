use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, AgentRecord, ConversationHandle, ConversationRecord, ExoHarness, SandboxProvider,
    SessionId, Uuid7,
};
use lingua::Message;
use lingua::universal::{UniversalStreamChunk, UserContent};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::io::{
    AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::{
    AgentConfig, AgentHarnessKind, AgentSandboxConfig, ConversationConfig, ConversationModelConfig,
    CreateAgentRequest, CreateConversationRequest, ExecutionStreamEvent, ExecutionStreamHandle,
    Harness, HarnessAgent, HarnessConversation, SandboxScope, SendRequest, SendResult,
};

use super::protocol::{
    AgentGetParams, ClientInfo, ConversationCreateParams, ConversationGetParams,
    ConversationListParams, EmptyParams, InitializeParams, PROTOCOL_VERSION, RequestId,
    TurnCancelParams, TurnStartParams,
};
use super::server::{AgentServer, MAX_FRAME_BYTES, cancel_for_shutdown};

#[tokio::test]
async fn protocol_discovery_and_conversation_methods() {
    let conversation = immediate_conversation("dev", Vec::new());
    let harness = test_harness(vec![conversation]);
    let (mut client, server) = TestClient::start(harness);

    client.send_raw("{").await;
    assert_error(client.read().await, -32700, None);

    client.send_raw("[]").await;
    assert_error(client.read().await, -32600, None);

    client.request(1, "agent/list", &EmptyParams {}).await;
    assert_error(client.read().await, -32004, Some("not_initialized"));

    client
        .request(
            2,
            "initialize",
            &InitializeParams {
                protocol_version: 2,
                client: None,
            },
        )
        .await;
    assert_error(
        client.read().await,
        -32003,
        Some("unsupported_protocol_version"),
    );

    initialize(&mut client, 3).await;

    client.request(4, "missing/method", &EmptyParams {}).await;
    assert_error(client.read().await, -32601, None);

    client.request(5, "agent/list", &EmptyParams {}).await;
    let frame = client.read().await;
    let result: AgentListView = parse_raw(frame.result.as_deref());
    assert_eq!(result.agents.len(), 1);
    assert_eq!(result.agents[0].slug, "agent");

    client
        .request(
            6,
            "agent/get",
            &AgentGetParams {
                agent_ref: "agent".to_string(),
            },
        )
        .await;
    let result: AgentGetView = parse_raw(client.read().await.result.as_deref());
    assert_eq!(result.agent.slug, "agent");

    client
        .request(
            61,
            "agent/get",
            &AgentGetParams {
                agent_ref: "missing".to_string(),
            },
        )
        .await;
    assert_error(client.read().await, -32002, Some("agent_not_found"));

    client
        .request(
            7,
            "conversation/list",
            &ConversationListParams {
                agent_ref: "agent".to_string(),
            },
        )
        .await;
    let result: ConversationListView = parse_raw(client.read().await.result.as_deref());
    assert_eq!(result.conversations.len(), 1);
    assert_eq!(result.conversations[0].slug, "dev");

    client
        .request(
            8,
            "conversation/get",
            &ConversationGetParams {
                agent_ref: "agent".to_string(),
                conversation_ref: "dev".to_string(),
            },
        )
        .await;
    let result: ConversationGetView = parse_raw(client.read().await.result.as_deref());
    assert_eq!(result.conversation.slug, "dev");

    client
        .request(
            9,
            "conversation/create",
            &ConversationCreateParams {
                agent_ref: "agent".to_string(),
                slug: Some("new".to_string()),
                name: Some("New conversation".to_string()),
                sandbox_image: None,
                sandbox_provider: Some(SandboxProvider::LocalProcess),
                shell_program: Some("/bin/sh".to_string()),
            },
        )
        .await;
    let result: ConversationGetView = parse_raw(client.read().await.result.as_deref());
    assert_eq!(result.conversation.slug, "new");

    client
        .send_raw(r#"{"jsonrpc":"2.0","id":10,"method":"agent/get","params":{}}"#)
        .await;
    assert_error(client.read().await, -32602, None);

    client
        .request(
            11,
            "turn/cancel",
            &TurnCancelParams {
                operation_id: Uuid7::now(),
            },
        )
        .await;
    assert_error(client.read().await, -32005, Some("unsupported"));

    client.close().await;
    server
        .await
        .expect("server task should join")
        .expect("server");
}

#[tokio::test]
async fn null_ids_invalid_utf8_and_oversized_frames_receive_errors_without_stopping_server() {
    let harness = test_harness(vec![immediate_conversation("dev", Vec::new())]);
    let (mut client, server) = TestClient::start(harness);

    client
        .send_raw(
            r#"{"jsonrpc":"2.0","id":null,"method":"initialize","params":{"protocol_version":1}}"#,
        )
        .await;
    assert!(client.read().await.result.is_some());

    client
        .send_raw(r#"{"jsonrpc":"1.0","method":"initialize","params":{"protocol_version":1}}"#)
        .await;
    let invalid_version = client.read().await;
    assert_eq!(invalid_version.id, RequestId::Null(()));
    assert_error(invalid_version, -32600, None);

    client.send_bytes(&[0xff]).await;
    assert_error(client.read().await, -32700, None);

    client.send_bytes(&vec![b' '; MAX_FRAME_BYTES + 1]).await;
    assert_error(client.read().await, -32700, None);

    client.request(1, "agent/list", &EmptyParams {}).await;
    assert!(client.read().await.result.is_some());

    client.close().await;
    server
        .await
        .expect("server task should join")
        .expect("server");
}

#[tokio::test]
async fn turn_stream_preserves_order_and_terminal_ids() {
    let send_result = SendResult {
        session_id: Uuid7::now(),
        turn_id: Uuid7::now(),
        latest_event_id: Uuid7::now(),
    };
    let events = vec![
        Ok(ExecutionStreamEvent::FirstChunk {
            ttft: Duration::from_millis(12),
        }),
        Ok(ExecutionStreamEvent::Chunk(
            UniversalStreamChunk::text_delta(0, "hello"),
        )),
        Ok(ExecutionStreamEvent::ToolCall {
            tool_call_id: "call-1".to_string(),
            tool_name: "shell".to_string(),
            arguments: serde_json::Map::new(),
        }),
        Ok(ExecutionStreamEvent::ToolResult {
            tool_call_id: "call-1".to_string(),
            result: serde_json::json!({"ok": true}),
        }),
        Ok(ExecutionStreamEvent::Completed(send_result.clone())),
    ];
    let conversation = immediate_conversation("dev", events);
    let harness = test_harness(vec![conversation]);
    let (mut client, server) = TestClient::start(harness);
    initialize(&mut client, 1).await;

    start_turn(&mut client, 2, "dev").await;
    let accepted = client.read().await;
    let accepted: AcceptedView = parse_raw(accepted.result.as_deref());
    assert_eq!(accepted.state, "accepted");

    let expected_methods = [
        "turn/started",
        "turn/event",
        "turn/event",
        "turn/event",
        "turn/event",
        "turn/completed",
    ];
    let expected_event_types = ["first_chunk", "chunk", "tool_call", "tool_result"];
    for (sequence, expected_method) in expected_methods.into_iter().enumerate() {
        let frame = client.read().await;
        assert_eq!(frame.method.as_deref(), Some(expected_method));
        let params: SequencedView = parse_raw(frame.params.as_deref());
        assert_eq!(params.operation_id, accepted.operation_id);
        assert_eq!(params.sequence, sequence as u64);
        if let Some(expected_event_type) = sequence
            .checked_sub(1)
            .and_then(|index| expected_event_types.get(index).copied())
        {
            let event: EventView = parse_raw(params.event.as_deref());
            assert_eq!(event.event_type, expected_event_type);
            if expected_event_type == "first_chunk" {
                assert_eq!(event.ttft_ms, Some(12));
            }
            if expected_event_type == "tool_result" {
                let result: ToolResultView = parse_raw(event.result.as_deref());
                assert!(result.ok);
            }
        }
    }

    let terminal: CompletedView = parse_raw(client.last_params());
    assert_eq!(terminal.session_id, send_result.session_id);
    assert_eq!(terminal.turn_id, send_result.turn_id);
    assert_eq!(terminal.latest_event_id, send_result.latest_event_id);

    client.close().await;
    server
        .await
        .expect("server task should join")
        .expect("server");
    assert!(client.read_optional().await.is_none());
}

#[tokio::test]
async fn executor_diagnostics_are_not_sent_to_clients() {
    let streamed = immediate_conversation(
        "streamed",
        vec![Err(anyhow!(
            "secret=do-not-leak at /private/runner.ts\nstack trace"
        ))],
    );
    let pre_stream = Arc::new(TestConversation::with_streams("pre-stream", Vec::new()));
    let harness = test_harness(vec![streamed, pre_stream]);
    let (mut client, server) = TestClient::start(harness);
    initialize(&mut client, 1).await;
    start_turn(&mut client, 2, "streamed").await;

    let accepted = client.read().await;
    assert!(accepted.result.is_some());
    assert_eq!(client.read().await.method.as_deref(), Some("turn/started"));
    let failed = client.read().await;
    assert_eq!(failed.method.as_deref(), Some("turn/failed"));
    let failed: FailedView = parse_raw(failed.params.as_deref());
    assert_eq!(failed.error.kind, "executor_error");
    assert_eq!(
        failed.error.message,
        "executor turn failed; see server logs"
    );

    start_turn(&mut client, 3, "pre-stream").await;
    assert!(client.read().await.result.is_some());
    let failed = client.read().await;
    assert_eq!(failed.method.as_deref(), Some("turn/failed"));
    let failed: FailedView = parse_raw(failed.params.as_deref());
    assert_eq!(
        failed.error.message,
        "executor turn failed; see server logs"
    );

    client.close().await;
    server
        .await
        .expect("server task should join")
        .expect("server");
    assert!(client.read_optional().await.is_none());
}

#[tokio::test]
async fn terminal_notification_releases_conversation_for_the_next_turn() {
    let conversation = Arc::new(TestConversation::with_streams(
        "dev",
        vec![
            immediate_stream(vec![Ok(completed_event())]),
            immediate_stream(vec![Ok(completed_event())]),
        ],
    ));
    let harness = test_harness(vec![conversation]);
    let (mut client, server) = TestClient::start(harness);
    initialize(&mut client, 1).await;

    for id in [2, 3] {
        start_turn(&mut client, id, "dev").await;
        assert!(client.read().await.result.is_some());
        assert_eq!(client.read().await.method.as_deref(), Some("turn/started"));
        assert_eq!(
            client.read().await.method.as_deref(),
            Some("turn/completed")
        );
    }

    client.close().await;
    server
        .await
        .expect("server task should join")
        .expect("server");
}

#[tokio::test]
async fn same_conversation_is_rejected_while_different_conversations_run() {
    let (first, first_tx) = pending_conversation("first");
    let (second, second_tx) = pending_conversation("second");
    let harness = test_harness(vec![first, second]);
    let (mut client, server) = TestClient::start(harness);
    initialize(&mut client, 1).await;

    start_turn(&mut client, 2, "first").await;
    let first_accepted: AcceptedView = parse_raw(client.read().await.result.as_deref());
    assert_eq!(client.read().await.method.as_deref(), Some("turn/started"));

    start_turn(&mut client, 3, "first").await;
    assert_error(client.read().await, -32001, Some("turn_already_active"));

    start_turn(&mut client, 4, "second").await;
    let second_accepted: AcceptedView = parse_raw(client.read().await.result.as_deref());
    assert_ne!(first_accepted.operation_id, second_accepted.operation_id);
    assert_eq!(client.read().await.method.as_deref(), Some("turn/started"));

    first_tx
        .send(Ok(completed_event()))
        .expect("first stream should be open");
    second_tx
        .send(Ok(completed_event()))
        .expect("second stream should be open");
    drop(first_tx);
    drop(second_tx);

    let mut completed = 0;
    while completed < 2 {
        if client.read().await.method.as_deref() == Some("turn/completed") {
            completed += 1;
        }
    }
    client.close_input().await;
    server
        .await
        .expect("server task should join")
        .expect("server");
    assert!(client.read_optional().await.is_none());
}

#[tokio::test]
async fn eof_cancels_a_permanently_pending_stream() {
    let (conversation, sender) = pending_conversation("pending");
    let harness = test_harness(vec![conversation]);
    let (mut client, server) = TestClient::start(harness);
    initialize(&mut client, 1).await;
    start_turn(&mut client, 2, "pending").await;
    assert!(client.read().await.result.is_some());
    assert_eq!(client.read().await.method.as_deref(), Some("turn/started"));

    client.close_input().await;
    let failed = client.read().await;
    assert_eq!(failed.method.as_deref(), Some("turn/failed"));
    let failed: FailedView = parse_raw(failed.params.as_deref());
    assert_eq!(failed.error.kind, "cancelled");
    assert_eq!(failed.error.message, "agent server input closed");
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("server should not wait for a pending stream")
        .expect("server task should join")
        .expect("server");
    assert!(sender.send(Ok(completed_event())).is_err());
    assert!(client.read_optional().await.is_none());
}

#[tokio::test]
async fn shutdown_preserves_a_buffered_completion() {
    let send_result = SendResult {
        session_id: Uuid7::now(),
        turn_id: Uuid7::now(),
        latest_event_id: Uuid7::now(),
    };
    let expected = send_result.clone();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(async move {
        event_tx
            .send(Ok(ExecutionStreamEvent::Completed(send_result)))
            .expect("completion receiver should be open");
    });
    let mut stream = ExecutionStreamHandle::with_task(
        UnboundedReceiverStream::new(event_rx),
        cancellation,
        task,
    );

    let (cancellation, completed) = cancel_for_shutdown(&mut stream).await;
    cancellation.expect("completed task should join");
    let completed = completed.expect("buffered completion should win");
    assert_eq!(completed.session_id, expected.session_id);
    assert_eq!(completed.turn_id, expected.turn_id);
    assert_eq!(completed.latest_event_id, expected.latest_event_id);
}

#[tokio::test]
async fn eof_cancels_a_stream_with_buffered_events() {
    let harness = test_harness(vec![backlogged_conversation("backlogged")]);
    let (mut client, server) = TestClient::start(harness);
    initialize(&mut client, 1).await;
    start_turn(&mut client, 2, "backlogged").await;
    assert!(client.read().await.result.is_some());
    assert_eq!(client.read().await.method.as_deref(), Some("turn/started"));

    client.close_input().await;
    let failed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let frame = client.read().await;
            if frame.method.as_deref() == Some("turn/failed") {
                return frame;
            }
        }
    })
    .await
    .expect("buffered events must not starve shutdown");
    let failed: FailedView = parse_raw(failed.params.as_deref());
    assert_eq!(failed.error.kind, "cancelled");
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("server should cancel a backlogged stream")
        .expect("server task should join")
        .expect("server");
    assert!(client.read_optional().await.is_none());
}

#[tokio::test]
async fn executor_task_panic_fails_only_its_turn() {
    let harness = test_harness(vec![panicking_conversation("panics")]);
    let (mut client, server) = TestClient::start(harness);
    initialize(&mut client, 1).await;
    start_turn(&mut client, 2, "panics").await;
    assert!(client.read().await.result.is_some());
    assert_eq!(client.read().await.method.as_deref(), Some("turn/started"));

    let failed = client.read().await;
    assert_eq!(failed.method.as_deref(), Some("turn/failed"));
    let failed: FailedView = parse_raw(failed.params.as_deref());
    assert_eq!(failed.error.kind, "executor_error");
    assert_eq!(
        failed.error.message,
        "executor turn failed; see server logs"
    );

    client.request(3, "agent/list", &EmptyParams {}).await;
    assert!(client.read().await.result.is_some());
    client.close().await;
    server
        .await
        .expect("server task should join")
        .expect("server");
}

#[tokio::test]
async fn output_failure_cancels_a_permanently_pending_stream() {
    let (conversation, sender) = pending_conversation("pending");
    let harness = test_harness(vec![conversation]);
    let (mut input, server_input) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(
        AgentServer::new(harness, AgentHarnessKind::Basic)
            .serve(server_input, FailOnThirdFlush::default()),
    );
    input
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":1}}
{"jsonrpc":"2.0","id":2,"method":"turn/start","params":{"agent_ref":"agent","conversation_ref":"pending","input":[{"role":"user","content":"hello"}]}}
"#,
        )
        .await
        .expect("write requests");
    input.flush().await.expect("flush requests");

    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("server should not wait after protocol output fails")
        .expect("server task should join")
        .expect_err("protocol output failure should stop the server");
    assert!(sender.send(Ok(completed_event())).is_err());
}

fn completed_event() -> ExecutionStreamEvent {
    ExecutionStreamEvent::Completed(SendResult {
        session_id: Uuid7::now(),
        turn_id: Uuid7::now(),
        latest_event_id: Uuid7::now(),
    })
}

fn immediate_conversation(
    slug: &str,
    events: Vec<Result<ExecutionStreamEvent>>,
) -> Arc<TestConversation> {
    Arc::new(TestConversation::new(slug, immediate_stream(events)))
}

fn immediate_stream(events: Vec<Result<ExecutionStreamEvent>>) -> ExecutionStreamHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    for event in events {
        tx.send(event).expect("test stream should be open");
    }
    drop(tx);
    ExecutionStreamHandle::new(UnboundedReceiverStream::new(rx))
}

fn pending_conversation(
    slug: &str,
) -> (
    Arc<TestConversation>,
    mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                event = rx.recv() => event,
                () = task_cancellation.cancelled() => return,
            };
            let Some(event) = event else {
                return;
            };
            let terminal = matches!(event, Err(_) | Ok(ExecutionStreamEvent::Completed(_)));
            if event_tx.send(event).is_err() || terminal {
                return;
            }
        }
    });
    (
        Arc::new(TestConversation::new(
            slug,
            ExecutionStreamHandle::with_task(
                UnboundedReceiverStream::new(event_rx),
                cancellation,
                task,
            ),
        )),
        tx,
    )
}

fn backlogged_conversation(slug: &str) -> Arc<TestConversation> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        loop {
            for _ in 0..64 {
                if event_tx
                    .send(Ok(ExecutionStreamEvent::Chunk(
                        UniversalStreamChunk::text_delta(0, "event"),
                    )))
                    .is_err()
                {
                    return;
                }
            }
            tokio::task::yield_now().await;
            if task_cancellation.is_cancelled() {
                return;
            }
        }
    });
    Arc::new(TestConversation::new(
        slug,
        ExecutionStreamHandle::with_task(
            UnboundedReceiverStream::new(event_rx),
            cancellation,
            task,
        ),
    ))
}

fn panicking_conversation(slug: &str) -> Arc<TestConversation> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(async move {
        drop(event_tx);
        panic!("test executor panic");
    });
    Arc::new(TestConversation::new(
        slug,
        ExecutionStreamHandle::with_task(
            UnboundedReceiverStream::new(event_rx),
            cancellation,
            task,
        ),
    ))
}

fn test_harness(conversations: Vec<Arc<TestConversation>>) -> Arc<dyn Harness> {
    Arc::new(TestHarness {
        agent: Arc::new(TestAgent {
            record: AgentRecord {
                id: Uuid7::now(),
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            },
            conversations: Mutex::new(conversations),
        }),
    })
}

struct TestHarness {
    agent: Arc<TestAgent>,
}

#[async_trait]
impl Harness for TestHarness {
    fn exoharness_handle(&self) -> Arc<dyn ExoHarness> {
        panic!("not used by agent server")
    }

    async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        Ok(vec![self.agent.record.clone()])
    }

    async fn get_agent(&self, agent_ref: &str) -> Result<Option<Arc<dyn HarnessAgent>>> {
        if matches_ref(&self.agent.record.id, &self.agent.record.slug, agent_ref) {
            Ok(Some(Arc::clone(&self.agent) as Arc<dyn HarnessAgent>))
        } else {
            Ok(None)
        }
    }

    async fn create_agent(&self, _request: CreateAgentRequest) -> Result<Arc<dyn HarnessAgent>> {
        Err(anyhow!("not implemented"))
    }

    async fn delete_agent(&self, _agent_ref: &str) -> Result<bool> {
        Err(anyhow!("not implemented"))
    }

    async fn flush_tracing(&self) -> Result<()> {
        Ok(())
    }
}

struct TestAgent {
    record: AgentRecord,
    conversations: Mutex<Vec<Arc<TestConversation>>>,
}

#[async_trait]
impl HarnessAgent for TestAgent {
    fn record(&self) -> &AgentRecord {
        &self.record
    }

    fn exoharness_handle(&self) -> Arc<dyn AgentHandle> {
        panic!("not used by agent server")
    }

    async fn config(&self) -> Result<AgentConfig> {
        Ok(AgentConfig {
            instructions: Vec::new(),
            harness: AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: false,
            sandbox: AgentSandboxConfig {
                scope: SandboxScope::Conversation,
                image: None,
                provider: SandboxProvider::LocalProcess,
                mounts: Vec::new(),
                enable_networking: false,
            },
            model: "test".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        })
    }

    async fn put_config(&self, _config: AgentConfig) -> Result<()> {
        Err(anyhow!("not implemented"))
    }

    async fn list_conversations(&self) -> Result<Vec<ConversationRecord>> {
        Ok(self
            .conversations
            .lock()
            .expect("conversations poisoned")
            .iter()
            .map(|conversation| conversation.record.clone())
            .collect())
    }

    async fn get_conversation(
        &self,
        conversation_ref: &str,
    ) -> Result<Option<Arc<dyn HarnessConversation>>> {
        Ok(self
            .conversations
            .lock()
            .expect("conversations poisoned")
            .iter()
            .find(|conversation| {
                matches_ref(
                    &conversation.record.id,
                    &conversation.record.slug,
                    conversation_ref,
                )
            })
            .cloned()
            .map(|conversation| conversation as Arc<dyn HarnessConversation>))
    }

    async fn create_conversation(
        &self,
        request: CreateConversationRequest,
    ) -> Result<Arc<dyn HarnessConversation>> {
        let id = Uuid7::now();
        let slug = request.slug.unwrap_or_else(|| id.to_string());
        let mut conversation = immediate_conversation(&slug, Vec::new());
        Arc::get_mut(&mut conversation)
            .expect("new conversation should be unique")
            .record
            .name = request.name.unwrap_or_else(|| slug.clone());
        self.conversations
            .lock()
            .expect("conversations poisoned")
            .push(Arc::clone(&conversation));
        Ok(conversation)
    }

    async fn delete_conversation(&self, _conversation_ref: &str) -> Result<bool> {
        Err(anyhow!("not implemented"))
    }
}

struct TestConversation {
    record: ConversationRecord,
    streams: Mutex<VecDeque<ExecutionStreamHandle>>,
}

impl TestConversation {
    fn new(slug: &str, stream: ExecutionStreamHandle) -> Self {
        Self::with_streams(slug, vec![stream])
    }

    fn with_streams(slug: &str, streams: Vec<ExecutionStreamHandle>) -> Self {
        Self {
            record: ConversationRecord {
                id: Uuid7::now(),
                slug: slug.to_string(),
                name: slug.to_string(),
                latest_event_id: None,
            },
            streams: Mutex::new(streams.into()),
        }
    }
}

#[async_trait]
impl HarnessConversation for TestConversation {
    fn record(&self) -> &ConversationRecord {
        &self.record
    }

    fn exoharness_handle(&self) -> Arc<dyn ConversationHandle> {
        panic!("not used by agent server")
    }

    async fn config(&self) -> Result<ConversationConfig> {
        Ok(ConversationConfig::default())
    }

    async fn put_config(&self, _config: ConversationConfig) -> Result<()> {
        Err(anyhow!("not implemented"))
    }

    async fn model_override(&self) -> Result<Option<ConversationModelConfig>> {
        Ok(None)
    }

    async fn put_model_override(&self, _config: Option<ConversationModelConfig>) -> Result<()> {
        Err(anyhow!("not implemented"))
    }

    async fn messages(&self) -> Result<Vec<Message>> {
        Ok(Vec::new())
    }

    async fn close_session(&self, _session_id: SessionId) -> Result<()> {
        Ok(())
    }

    async fn send(&self, _request: SendRequest) -> Result<SendResult> {
        Err(anyhow!("not implemented"))
    }

    async fn send_stream(&self, _request: SendRequest) -> Result<ExecutionStreamHandle> {
        let stream = self
            .streams
            .lock()
            .expect("streams poisoned")
            .pop_front()
            .ok_or_else(|| anyhow!("no executor stream available; secret=do-not-leak"))?;
        Ok(stream)
    }
}

fn matches_ref(id: &Uuid7, slug: &str, reference: &str) -> bool {
    id.to_string() == reference || slug == reference
}

struct TestClient {
    lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: Option<WriteHalf<DuplexStream>>,
    last_params: Option<Box<RawValue>>,
}

impl TestClient {
    fn start(harness: Arc<dyn Harness>) -> (Self, JoinHandle<Result<()>>) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client);
        let (server_reader, server_writer) = tokio::io::split(server);
        let task = tokio::spawn(
            AgentServer::new(harness, AgentHarnessKind::Basic).serve(server_reader, server_writer),
        );
        (
            Self {
                lines: BufReader::new(client_reader).lines(),
                writer: Some(client_writer),
                last_params: None,
            },
            task,
        )
    }

    async fn request<P: Serialize>(&mut self, id: i64, method: &str, params: &P) {
        let request = TestRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        self.send_raw(&serde_json::to_string(&request).expect("serialize request"))
            .await;
    }

    async fn send_raw(&mut self, line: &str) {
        self.send_bytes(line.as_bytes()).await;
    }

    async fn send_bytes(&mut self, bytes: &[u8]) {
        let writer = self.writer.as_mut().expect("client input is open");
        writer.write_all(bytes).await.expect("write request");
        writer.write_all(b"\n").await.expect("write delimiter");
        writer.flush().await.expect("flush request");
    }

    async fn read(&mut self) -> TestFrame {
        self.read_optional().await.expect("expected server frame")
    }

    async fn read_optional(&mut self) -> Option<TestFrame> {
        let line = self.lines.next_line().await.expect("read server frame")?;
        let frame: TestFrame = serde_json::from_str(&line).expect("valid JSON-RPC frame");
        assert_eq!(frame.jsonrpc, "2.0");
        self.last_params = frame.params.clone();
        Some(frame)
    }

    fn last_params(&self) -> Option<&RawValue> {
        self.last_params.as_deref()
    }

    async fn close_input(&mut self) {
        if let Some(mut writer) = self.writer.take() {
            writer.shutdown().await.expect("close client input");
        }
    }

    async fn close(&mut self) {
        self.close_input().await;
    }
}

#[derive(Default)]
struct FailOnThirdFlush {
    flushes: usize,
}

impl AsyncWrite for FailOnThirdFlush {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.flushes == 2 {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test output closed",
            )))
        } else {
            self.flushes += 1;
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Serialize)]
struct TestRequest<'a, P> {
    jsonrpc: &'static str,
    id: i64,
    method: &'a str,
    params: &'a P,
}

#[derive(Deserialize)]
struct TestFrame {
    jsonrpc: String,
    #[serde(default)]
    id: RequestId,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    result: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<TestError>,
    #[serde(default)]
    params: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct TestError {
    code: i32,
    #[serde(default)]
    data: Option<TestErrorData>,
}

#[derive(Deserialize)]
struct TestErrorData {
    kind: String,
}

#[derive(Deserialize)]
struct InitializeView {
    protocol_version: u32,
    runtime_contract: String,
    transport: TransportView,
    capabilities: CapabilitiesView,
}

#[derive(Deserialize)]
struct TransportView {
    kind: String,
    framing: String,
}

#[derive(Deserialize)]
struct CapabilitiesView {
    turn_cancel: bool,
}

#[derive(Deserialize)]
struct AgentListView {
    agents: Vec<AgentRecord>,
}

#[derive(Deserialize)]
struct AgentGetView {
    agent: AgentRecord,
}

#[derive(Deserialize)]
struct ConversationListView {
    conversations: Vec<ConversationRecord>,
}

#[derive(Deserialize)]
struct ConversationGetView {
    conversation: ConversationRecord,
}

#[derive(Deserialize)]
struct AcceptedView {
    operation_id: Uuid7,
    state: String,
}

#[derive(Deserialize)]
struct SequencedView {
    operation_id: Uuid7,
    sequence: u64,
    #[serde(default)]
    event: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct EventView {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    ttft_ms: Option<u64>,
    #[serde(default)]
    result: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct ToolResultView {
    ok: bool,
}

#[derive(Deserialize)]
struct CompletedView {
    session_id: Uuid7,
    turn_id: Uuid7,
    latest_event_id: Uuid7,
}

#[derive(Deserialize)]
struct FailedView {
    error: FailedErrorView,
}

#[derive(Deserialize)]
struct FailedErrorView {
    kind: String,
    message: String,
}

async fn initialize(client: &mut TestClient, id: i64) {
    client
        .request(
            id,
            "initialize",
            &InitializeParams {
                protocol_version: PROTOCOL_VERSION,
                client: Some(ClientInfo {
                    name: "test".to_string(),
                    version: "1".to_string(),
                }),
            },
        )
        .await;
    let result: InitializeView = parse_raw(client.read().await.result.as_deref());
    assert_eq!(result.protocol_version, PROTOCOL_VERSION);
    assert_eq!(result.runtime_contract, "exo-agent-server-v1");
    assert_eq!(result.transport.kind, "stdio");
    assert_eq!(result.transport.framing, "jsonl");
    assert!(!result.capabilities.turn_cancel);
}

async fn start_turn(client: &mut TestClient, id: i64, conversation_ref: &str) {
    client
        .request(
            id,
            "turn/start",
            &TurnStartParams {
                agent_ref: "agent".to_string(),
                conversation_ref: conversation_ref.to_string(),
                input: vec![Message::User {
                    content: UserContent::String("hello".to_string()),
                }],
                session_id: None,
            },
        )
        .await;
}

fn parse_raw<T: for<'de> Deserialize<'de>>(raw: Option<&RawValue>) -> T {
    serde_json::from_str(raw.expect("frame payload").get()).expect("deserialize frame payload")
}

fn assert_error(frame: TestFrame, code: i32, kind: Option<&str>) {
    let error = frame.error.expect("error response");
    assert_eq!(error.code, code);
    assert_eq!(error.data.as_ref().map(|data| data.kind.as_str()), kind);
}

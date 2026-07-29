//! ACP stdio transport for one existing Exo conversation.
//!
//! Standard output belongs only to newline-framed ACP JSON-RPC. Exo continues
//! to own its conversation, durable event log, model credentials, and sandbox.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Agent, ConnectTo, ConnectionTo, Responder, Stdio};
use anyhow::Result;
use executor::{
    ExecutionCancellation, ExecutionStreamEvent, HarnessConversation, SendRequest, SessionId,
};
use lingua::Message;
use lingua::universal::UserContent;
use serde_json::Value;
use tokio_stream::StreamExt;

#[derive(Clone)]
struct SessionState {
    sessions: Arc<Mutex<HashMap<String, Option<SessionId>>>>,
    active: Arc<Mutex<HashMap<String, ExecutionCancellation>>>,
    conversation: Arc<dyn HarnessConversation>,
}

impl SessionState {
    fn new(conversation: Arc<dyn HarnessConversation>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(Mutex::new(HashMap::new())),
            conversation,
        }
    }

    fn new_session(&self) -> acp::SessionId {
        let id = acp::SessionId::new(format!("exo-acp-{}", uuid::Uuid::now_v7()));
        self.sessions
            .lock()
            .expect("ACP session map poisoned")
            .insert(id.0.to_string(), None);
        id
    }

    fn exo_session(&self, session_id: &acp::SessionId) -> Option<Option<SessionId>> {
        self.sessions
            .lock()
            .expect("ACP session map poisoned")
            .get(session_id.0.as_ref())
            .copied()
    }

    fn set_exo_session(&self, session_id: &acp::SessionId, exo_session: SessionId) {
        self.sessions
            .lock()
            .expect("ACP session map poisoned")
            .insert(session_id.0.to_string(), Some(exo_session));
    }

    fn begin_turn(&self, session_id: &acp::SessionId, cancellation: ExecutionCancellation) -> bool {
        let mut active = self.active.lock().expect("ACP active-turn map poisoned");
        match active.entry(session_id.0.to_string()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(cancellation);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    fn finish_turn(&self, session_id: &acp::SessionId) {
        self.active
            .lock()
            .expect("ACP active-turn map poisoned")
            .remove(session_id.0.as_ref());
    }

    fn cancel(&self, session_id: &acp::SessionId) {
        if let Some(cancellation) = self
            .active
            .lock()
            .expect("ACP active-turn map poisoned")
            .get(session_id.0.as_ref())
            .cloned()
        {
            cancellation.cancel();
        }
    }
}

/// Serve the conversation until the ACP stdio transport closes.
pub async fn serve(conversation: Arc<dyn HarnessConversation>) -> Result<()> {
    serve_transport(conversation, Stdio::new()).await
}

async fn serve_transport(
    conversation: Arc<dyn HarnessConversation>,
    transport: impl ConnectTo<Agent>,
) -> Result<()> {
    let state = SessionState::new(conversation);
    let new_state = state.clone();
    let prompt_state = state.clone();
    let cancel_state = state;

    Agent
        .builder()
        .name("exo-acp")
        .on_receive_request(
            async move |request: acp::InitializeRequest,
                        responder: Responder<acp::InitializeResponse>,
                        _connection: ConnectionTo<agent_client_protocol::Client>| {
                let version = if request.protocol_version >= ProtocolVersion::V1 {
                    ProtocolVersion::V1
                } else {
                    request.protocol_version
                };
                responder.respond(
                    acp::InitializeResponse::new(version)
                        .agent_capabilities(acp::AgentCapabilities::new())
                        .agent_info(
                            acp::Implementation::new("exo", env!("CARGO_PKG_VERSION")).title("Exo"),
                        ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: acp::NewSessionRequest,
                        responder: Responder<acp::NewSessionResponse>,
                        _connection: ConnectionTo<agent_client_protocol::Client>| {
                responder.respond(acp::NewSessionResponse::new(new_state.new_session()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::PromptRequest,
                        responder: Responder<acp::PromptResponse>,
                        connection: ConnectionTo<agent_client_protocol::Client>| {
                let Some(exo_session) = prompt_state.exo_session(&request.session_id) else {
                    return responder.respond_with_error(
                        agent_client_protocol::Error::invalid_params()
                            .data("unknown Exo ACP session"),
                    );
                };
                let prompt = prompt_text(&request.prompt);
                if prompt.is_empty() {
                    return responder.respond_with_error(
                        agent_client_protocol::Error::invalid_params()
                            .data("the prompt contains no text"),
                    );
                }

                let cancellation = ExecutionCancellation::new();
                if !prompt_state.begin_turn(&request.session_id, cancellation.clone()) {
                    return responder.respond_with_error(
                        agent_client_protocol::Error::invalid_request()
                            .data("this Exo ACP session already has an active turn"),
                    );
                }
                let task_state = prompt_state.clone();
                let prompt_connection = connection.clone();
                connection.spawn(async move {
                    let result = run_prompt(
                        &task_state,
                        request.session_id.clone(),
                        exo_session,
                        prompt,
                        cancellation,
                        prompt_connection,
                    )
                    .await;
                    task_state.finish_turn(&request.session_id);
                    match result {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_internal_error(error),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: acp::CancelNotification,
                        _connection: ConnectionTo<agent_client_protocol::Client>| {
                cancel_state.cancel(&notification.session_id);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await?;
    Ok(())
}

async fn run_prompt(
    state: &SessionState,
    session_id: acp::SessionId,
    exo_session: Option<SessionId>,
    prompt: String,
    cancellation: ExecutionCancellation,
    connection: ConnectionTo<agent_client_protocol::Client>,
) -> Result<acp::PromptResponse> {
    let mut stream = state
        .conversation
        .send_stream_with_cancellation(
            SendRequest {
                input: vec![Message::User {
                    content: UserContent::String(prompt),
                }],
                session_id: exo_session,
            },
            cancellation,
        )
        .await?;

    while let Some(event) = stream.next().await {
        match event? {
            ExecutionStreamEvent::FirstChunk { .. } => {}
            ExecutionStreamEvent::Chunk(chunk) => {
                let text = chunk_text(&chunk);
                if !text.is_empty() {
                    notify(
                        &connection,
                        &session_id,
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(text.into())),
                    )?;
                }
            }
            ExecutionStreamEvent::ToolCall {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                notify(
                    &connection,
                    &session_id,
                    acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(tool_call_id, tool_name.clone())
                            .status(acp::ToolCallStatus::InProgress)
                            .raw_input(Value::Object(arguments)),
                    ),
                )?;
            }
            ExecutionStreamEvent::ToolResult {
                tool_call_id,
                result,
            } => {
                notify(
                    &connection,
                    &session_id,
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        tool_call_id,
                        acp::ToolCallUpdateFields::new()
                            .status(acp::ToolCallStatus::Completed)
                            .raw_output(serde_json::to_value(result)?),
                    )),
                )?;
            }
            ExecutionStreamEvent::Completed(result) => {
                state.set_exo_session(&session_id, result.session_id);
                return Ok(
                    acp::PromptResponse::new(acp::StopReason::EndTurn).meta(turn_meta(&result))
                );
            }
            ExecutionStreamEvent::Cancelled(result) => {
                state.set_exo_session(&session_id, result.session_id);
                return Ok(
                    acp::PromptResponse::new(acp::StopReason::Cancelled).meta(turn_meta(&result))
                );
            }
        }
    }

    anyhow::bail!("Exo execution stream ended without completion")
}

fn turn_meta(result: &executor::SendResult) -> acp::Meta {
    let mut meta = acp::Meta::new();
    meta.insert(
        "exo.session_id".into(),
        Value::String(result.session_id.to_string()),
    );
    meta.insert(
        "exo.turn_id".into(),
        Value::String(result.turn_id.to_string()),
    );
    meta.insert(
        "exo.latest_event_id".into(),
        Value::String(result.latest_event_id.to_string()),
    );
    meta
}

fn notify(
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: &acp::SessionId,
    update: acp::SessionUpdate,
) -> Result<()> {
    connection.send_notification(acp::SessionNotification::new(session_id.clone(), update))?;
    Ok(())
}

fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn chunk_text(chunk: &lingua::UniversalStreamChunk) -> String {
    let mut text = String::new();
    for choice in &chunk.choices {
        if let Some(delta) = choice.delta_view()
            && let Some(content) = delta.content
        {
            text.push_str(&content);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{Channel, Client};
    use executor::{
        ConversationConfig, ConversationHandle, ConversationModelConfig, ExecutionStreamHandle,
        SendResult, Uuid7,
    };
    use exoharness::ConversationRecord;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    struct FakeConversation {
        record: ConversationRecord,
        cancel_when_requested: bool,
        turn_started: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl HarnessConversation for FakeConversation {
        fn record(&self) -> &ConversationRecord {
            &self.record
        }

        fn exoharness_handle(&self) -> Arc<dyn ConversationHandle> {
            panic!("the ACP transport does not ask for the raw conversation handle")
        }

        async fn config(&self) -> Result<ConversationConfig> {
            Ok(ConversationConfig::default())
        }

        async fn put_config(&self, _config: ConversationConfig) -> Result<()> {
            anyhow::bail!("not used")
        }

        async fn model_override(&self) -> Result<Option<ConversationModelConfig>> {
            Ok(None)
        }

        async fn put_model_override(&self, _config: Option<ConversationModelConfig>) -> Result<()> {
            anyhow::bail!("not used")
        }

        async fn messages(&self) -> Result<Vec<Message>> {
            Ok(Vec::new())
        }

        async fn close_session(&self, _session_id: SessionId) -> Result<()> {
            Ok(())
        }

        async fn send(&self, _request: SendRequest) -> Result<SendResult> {
            anyhow::bail!("ACP uses the streaming send")
        }

        async fn send_stream(&self, request: SendRequest) -> Result<ExecutionStreamHandle> {
            self.send_stream_with_cancellation(request, ExecutionCancellation::new())
                .await
        }

        async fn send_stream_with_cancellation(
            &self,
            request: SendRequest,
            cancellation: ExecutionCancellation,
        ) -> Result<ExecutionStreamHandle> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let session_id = request.session_id.unwrap_or_else(Uuid7::now);
            let turn_id = Uuid7::now();
            let latest_event_id = Uuid7::now();
            if self.cancel_when_requested {
                self.turn_started.notify_one();
                tokio::spawn(async move {
                    cancellation.cancelled().await;
                    drop(tx.send(Ok(ExecutionStreamEvent::Cancelled(SendResult {
                        session_id,
                        turn_id,
                        latest_event_id,
                    }))));
                });
                return Ok(ExecutionStreamHandle::new(UnboundedReceiverStream::new(rx)));
            }
            tx.send(Ok(ExecutionStreamEvent::Chunk(
                lingua::UniversalStreamChunk::text_delta(0, "hello"),
            )))
            .map_err(|_| anyhow::anyhow!("test stream closed"))?;
            tx.send(Ok(ExecutionStreamEvent::ToolCall {
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                arguments: serde_json::Map::from_iter([(
                    "command".into(),
                    Value::String("printf ok".into()),
                )]),
            }))
            .map_err(|_| anyhow::anyhow!("test stream closed"))?;
            tx.send(Ok(ExecutionStreamEvent::ToolResult {
                tool_call_id: "call-1".into(),
                result: serde_json::json!({"stdout": "ok"}),
            }))
            .map_err(|_| anyhow::anyhow!("test stream closed"))?;
            tx.send(Ok(ExecutionStreamEvent::Completed(SendResult {
                session_id,
                turn_id,
                latest_event_id,
            })))
            .map_err(|_| anyhow::anyhow!("test stream closed"))?;
            Ok(ExecutionStreamHandle::new(UnboundedReceiverStream::new(rx)))
        }
    }

    #[test]
    fn prompt_text_preserves_text_block_order() {
        let prompt = vec![
            acp::ContentBlock::Text(acp::TextContent::new("first")),
            acp::ContentBlock::Text(acp::TextContent::new("second")),
        ];
        assert_eq!(prompt_text(&prompt), "first\nsecond");
    }

    #[test]
    fn completion_metadata_carries_durable_exo_references() {
        let result = executor::SendResult {
            session_id: executor::Uuid7::now(),
            turn_id: executor::Uuid7::now(),
            latest_event_id: executor::Uuid7::now(),
        };
        let meta = turn_meta(&result);
        assert_eq!(
            meta.get("exo.session_id").and_then(Value::as_str),
            Some(result.session_id.to_string().as_str())
        );
        assert_eq!(
            meta.get("exo.turn_id").and_then(Value::as_str),
            Some(result.turn_id.to_string().as_str())
        );
        assert_eq!(
            meta.get("exo.latest_event_id").and_then(Value::as_str),
            Some(result.latest_event_id.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn acp_carries_text_tool_call_tool_result_and_completion() {
        let conversation: Arc<dyn HarnessConversation> = Arc::new(FakeConversation {
            record: ConversationRecord {
                id: Uuid7::now(),
                slug: "acp".into(),
                name: "ACP".into(),
                latest_event_id: None,
            },
            cancel_when_requested: false,
            turn_started: Arc::new(tokio::sync::Notify::new()),
        });
        let updates = Arc::new(Mutex::new(Vec::<acp::SessionUpdate>::new()));
        let notification_updates = Arc::clone(&updates);
        let (agent_transport, client_transport) = Channel::duplex();
        let server = tokio::spawn(serve_transport(conversation, agent_transport));

        Client
            .builder()
            .on_receive_notification(
                async move |notification: acp::SessionNotification,
                            _connection: ConnectionTo<Agent>| {
                    notification_updates
                        .lock()
                        .expect("updates")
                        .push(notification.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |connection| {
                connection
                    .send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(acp::NewSessionRequest::new(
                        std::env::current_dir().map_err(anyhow::Error::from)?,
                    ))
                    .block_task()
                    .await?;
                let response = connection
                    .send_request(acp::PromptRequest::new(
                        session.session_id,
                        vec![acp::ContentBlock::Text(acp::TextContent::new("run"))],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
                assert!(response.meta.is_some());
                Ok(())
            })
            .await
            .expect("ACP client");
        server.await.expect("server task").expect("ACP server");

        let updates = updates.lock().expect("updates");
        assert!(
            updates
                .iter()
                .any(|update| { matches!(update, acp::SessionUpdate::AgentMessageChunk(_)) })
        );
        assert!(
            updates
                .iter()
                .any(|update| { matches!(update, acp::SessionUpdate::ToolCall(_)) })
        );
        assert!(
            updates
                .iter()
                .any(|update| { matches!(update, acp::SessionUpdate::ToolCallUpdate(_)) })
        );
    }

    #[tokio::test]
    async fn acp_cancel_notification_cancels_the_active_exo_turn() {
        let turn_started = Arc::new(tokio::sync::Notify::new());
        let conversation: Arc<dyn HarnessConversation> = Arc::new(FakeConversation {
            record: ConversationRecord {
                id: Uuid7::now(),
                slug: "acp-cancel".into(),
                name: "ACP cancel".into(),
                latest_event_id: None,
            },
            cancel_when_requested: true,
            turn_started: Arc::clone(&turn_started),
        });
        let (agent_transport, client_transport) = Channel::duplex();
        let server = tokio::spawn(serve_transport(conversation, agent_transport));

        Client
            .builder()
            .connect_with(client_transport, async move |connection| {
                connection
                    .send_request(acp::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(acp::NewSessionRequest::new(
                        std::env::current_dir().map_err(anyhow::Error::from)?,
                    ))
                    .block_task()
                    .await?;
                let session_id = session.session_id;
                let prompt_connection = connection.clone();
                let prompt_session_id = session_id.clone();
                let prompt = tokio::spawn(async move {
                    prompt_connection
                        .send_request(acp::PromptRequest::new(
                            prompt_session_id,
                            vec![acp::ContentBlock::Text(acp::TextContent::new("wait"))],
                        ))
                        .block_task()
                        .await
                });
                turn_started.notified().await;
                connection.send_notification(acp::CancelNotification::new(session_id))?;
                let response = prompt.await.map_err(anyhow::Error::from)??;
                assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
                assert!(response.meta.is_some());
                Ok(())
            })
            .await
            .expect("ACP client");
        server.await.expect("server task").expect("ACP server");
    }
}

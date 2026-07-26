//! ACP stdio transport for one existing Exo conversation.
//!
//! Standard output belongs only to newline-framed ACP JSON-RPC. Exo continues
//! to own its conversation, durable event log, model credentials, and sandbox.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Agent, ConnectionTo, Responder, Stdio};
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
        .connect_to(Stdio::new())
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
}

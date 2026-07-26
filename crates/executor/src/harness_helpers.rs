use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use exoharness::{
    AddEventsRequest, AgentHandle, Binding, ConversationHandle, EventData, EventKind, EventQuery,
    EventQueryDirection, ExoHarness, Result, Secret, ToolCallId, Uuid7,
};
use lingua::Message;
use lingua::universal::{
    AssistantContent, AssistantContentPart, ToolContentPart, ToolResultContentPart, UserContent,
    UserContentPart,
};
use serde::{Deserialize, Serialize};

use crate::ConversationModelConfig;
use crate::compaction::{read_active_checkpoint, read_summary, summary_message};

pub(crate) const CONVERSATION_MODEL_CONFIG_EVENT_TYPE: &str = "conversation_model_config";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ConversationModelConfigEvent {
    Set {
        model: String,
        max_output_tokens: Option<i64>,
    },
    Clear,
}

impl ConversationModelConfigEvent {
    fn into_model_config(self) -> Option<ConversationModelConfig> {
        match self {
            Self::Set {
                model,
                max_output_tokens,
            } => Some(ConversationModelConfig {
                model,
                max_output_tokens,
            }),
            Self::Clear => None,
        }
    }
}

pub(crate) async fn resolve_agent_handle(
    exoharness: &dyn ExoHarness,
    agent_ref: &str,
) -> Result<Option<Arc<dyn AgentHandle>>> {
    if let Some(agent_id) = parse_uuid7(agent_ref)
        && let Some(agent) = exoharness.get_agent(&agent_id).await?
    {
        return Ok(Some(agent));
    }

    let agents = exoharness.list_agents().await?;
    Ok(agents
        .into_iter()
        .find(|agent| agent.record().slug == agent_ref))
}

pub(crate) async fn resolve_conversation_handle(
    agent: &dyn AgentHandle,
    conversation_ref: &str,
) -> Result<Option<Arc<dyn ConversationHandle>>> {
    if let Some(conversation_id) = parse_uuid7(conversation_ref)
        && let Some(conversation) = agent.get_conversation(&conversation_id).await?
    {
        return Ok(Some(conversation));
    }

    let conversations = agent
        .list_conversations(exoharness::ListConversationsRequest::default())
        .await?
        .conversations;
    Ok(conversations
        .into_iter()
        .find(|conversation| conversation.record().slug == conversation_ref))
}

/// Conversation history as prompt messages, honouring any compaction
/// checkpoint. Used by executors that assemble history directly rather than
/// through `BasicExecutor`'s incremental cache.
pub(crate) async fn materialize_conversation_messages(
    conversation: &dyn ConversationHandle,
) -> Result<Vec<Message>> {
    // A checkpoint whose artifact has vanished falls back to full history:
    // a large prompt beats one with a hole where the summary should be.
    let summary = match read_active_checkpoint(conversation).await? {
        Some((_, checkpoint)) => read_summary(conversation, &checkpoint)
            .await?
            .map(|text| (text, checkpoint.up_to_event_id)),
        None => None,
    };

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: summary.as_ref().map(|(_, cursor)| *cursor),
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await?
        .events;

    let mut messages = Vec::new();
    if let Some((summary, _)) = &summary {
        messages.push(summary_message(summary));
    }
    let mut tool_call_names = HashMap::<ToolCallId, String>::new();

    for event in events {
        match event.data {
            EventData::Messages {
                messages: event_messages,
                ..
            } => messages.extend(event_messages),
            EventData::ToolRequested {
                tool_call_id,
                request,
                ..
            } => {
                tool_call_names.insert(tool_call_id, request.function_name);
            }
            EventData::ToolResult {
                tool_call_id,
                result,
            } => {
                let Some(tool_name) = tool_call_names.get(&tool_call_id) else {
                    continue;
                };
                messages.push(Message::Tool {
                    content: vec![ToolContentPart::ToolResult(ToolResultContentPart {
                        tool_call_id,
                        tool_name: tool_name.clone(),
                        output: to_lingua_value(result),
                        provider_options: None,
                    })],
                });
            }
            _ => {}
        }
    }

    Ok(messages)
}

pub(crate) async fn get_conversation_model_override(
    conversation: &dyn ConversationHandle,
) -> Result<Option<ConversationModelConfig>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(1),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::custom(
                CONVERSATION_MODEL_CONFIG_EVENT_TYPE,
            )]),
        }))
        .await?
        .events;

    let Some(event) = events.into_iter().next() else {
        return Ok(None);
    };

    let EventData::Custom { payload, .. } = event.data else {
        return Ok(None);
    };
    let config_event: ConversationModelConfigEvent = serde_json::from_value(payload)?;
    Ok(config_event.into_model_config())
}

pub(crate) async fn put_conversation_model_override(
    conversation: &dyn ConversationHandle,
    config: Option<ConversationModelConfig>,
) -> Result<()> {
    let payload = match config {
        Some(ConversationModelConfig {
            model,
            max_output_tokens,
        }) => serde_json::to_value(ConversationModelConfigEvent::Set {
            model,
            max_output_tokens,
        })?,
        None => serde_json::to_value(ConversationModelConfigEvent::Clear)?,
    };

    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::Custom {
                event_type: CONVERSATION_MODEL_CONFIG_EVENT_TYPE.to_string(),
                payload,
            }],
        })
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedModelBinding {
    pub(crate) model: String,
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: Option<String>,
}

pub(crate) async fn resolve_model_binding(
    conversation: &dyn ConversationHandle,
    name: &str,
) -> Result<ResolvedModelBinding> {
    let binding_record = conversation
        .list_bindings()
        .await?
        .into_iter()
        .find(|binding| binding.name == name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "model is not registered: {name}; run `exo model register {name} --secret <secret>`"
            )
        })?;
    let Binding::Llm {
        model,
        base_url,
        secret_id,
        ..
    } = binding_record.binding
    else {
        return Err(anyhow::anyhow!("binding is not a model: {name}"));
    };
    let api_key = match secret_id {
        Some(secret_id) => {
            let secret = conversation
                .get_secret(&secret_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("model secret does not exist for {name}"))?;
            match secret {
                Secret::Key { value } => Some(value),
                Secret::Oauth { .. } => {
                    return Err(anyhow::anyhow!(
                        "model secret must be a key secret, got oauth for {name}"
                    ));
                }
            }
        }
        None => None,
    };
    Ok(ResolvedModelBinding {
        model,
        api_key,
        base_url,
    })
}

pub(crate) fn to_lingua_value(value: serde_json::Value) -> lingua::serde_json::Value {
    match value {
        serde_json::Value::Null => lingua::serde_json::Value::Null,
        serde_json::Value::Bool(value) => lingua::serde_json::Value::Bool(value),
        serde_json::Value::Number(value) => lingua::serde_json::Value::Number(
            lingua::serde_json::Number::from_string_unchecked(value.to_string()),
        ),
        serde_json::Value::String(value) => lingua::serde_json::Value::String(value),
        serde_json::Value::Array(values) => {
            lingua::serde_json::Value::Array(values.into_iter().map(to_lingua_value).collect())
        }
        serde_json::Value::Object(values) => lingua::serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, to_lingua_value(value)))
                .collect(),
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HistoryMessage {
    pub index: usize,
    pub role: String,
    pub content: String,
}

pub(crate) fn messages_to_history_messages(messages: &[Message]) -> Vec<HistoryMessage> {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| history_message_from_message(index, message))
        .collect()
}

pub(crate) fn messages_to_transcript(messages: &[Message]) -> String {
    messages_to_history_messages(messages)
        .into_iter()
        .map(|message| format!("{}:\n{}", message.role.to_uppercase(), message.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn assistant_messages_text(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { content, .. } => Some(render_assistant_content(content)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn system_message(text: &str) -> Message {
    Message::System {
        content: UserContent::String(text.to_string()),
    }
}

pub(crate) fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

pub(crate) fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        content: AssistantContent::String(text.to_string()),
        id: None,
    }
}

fn history_message_from_message(index: usize, message: &Message) -> HistoryMessage {
    match message {
        Message::User { content } => HistoryMessage {
            index,
            role: "user".to_string(),
            content: render_user_content(content),
        },
        Message::Assistant { content, .. } => HistoryMessage {
            index,
            role: "assistant".to_string(),
            content: render_assistant_content(content),
        },
        Message::Tool { content } => HistoryMessage {
            index,
            role: "tool".to_string(),
            content: content
                .iter()
                .map(|part| {
                    let ToolContentPart::ToolResult(result) = part;
                    format!("{} => {}", result.tool_name, result.output)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        Message::System { content } => HistoryMessage {
            index,
            role: "system".to_string(),
            content: render_user_content(content),
        },
        Message::Developer { content } => HistoryMessage {
            index,
            role: "developer".to_string(),
            content: render_user_content(content),
        },
    }
}

fn parse_uuid7(raw: &str) -> Option<Uuid7> {
    Uuid7::from_str(raw).ok()
}

fn render_user_content(content: &UserContent) -> String {
    match content {
        UserContent::String(text) => text.clone(),
        UserContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                UserContentPart::Text(text) => text.text.clone(),
                _ => "[non-text user content]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn render_assistant_content(content: &AssistantContent) -> String {
    match content {
        AssistantContent::String(text) => text.clone(),
        AssistantContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                AssistantContentPart::Text(text) => text.text.clone(),
                AssistantContentPart::Reasoning { text, .. } => format!("[reasoning] {text}"),
                AssistantContentPart::ToolCall {
                    tool_name,
                    arguments,
                    ..
                } => format!("[tool_call {tool_name}] {arguments}"),
                AssistantContentPart::ToolResult {
                    tool_name, output, ..
                } => format!("[tool_result {tool_name}] {output}"),
                AssistantContentPart::File { .. } => "[file]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::to_lingua_value;

    #[test]
    fn converts_std_json_to_lingua_json_structurally() {
        let value = json!({
            "null": null,
            "bool": true,
            "number": 123.5,
            "string": "hello",
            "array": [1, false, {"nested": "value"}],
        });

        let encoded = serde_json::to_string(&value).expect("test json should serialize");
        let expected: lingua::serde_json::Value =
            lingua::serde_json::from_str(&encoded).expect("test json should parse as lingua json");

        assert_eq!(to_lingua_value(value), expected);
    }
}

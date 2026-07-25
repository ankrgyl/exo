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

use crate::{ConversationModelConfig, ModelRequestAuth};

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

pub(crate) async fn materialize_conversation_messages(
    conversation: &dyn ConversationHandle,
) -> Result<Vec<Message>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await?
        .events;

    let mut messages = Vec::new();
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
    pub(crate) provider: Option<String>,
    pub(crate) auth: Option<ModelRequestAuth>,
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
        provider,
        base_url,
        secret_id,
        ..
    } = binding_record.binding
    else {
        return Err(anyhow::anyhow!("binding is not a model: {name}"));
    };
    let effective_provider = model_provider(provider.as_deref(), &model, base_url.as_deref());
    let auth = match secret_id {
        Some(secret_id) => {
            let secret = conversation
                .get_secret(&secret_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("model secret does not exist for {name}"))?;
            Some(request_auth_for_secret(effective_provider, secret)?)
        }
        None => None,
    };
    Ok(ResolvedModelBinding {
        model,
        provider,
        auth,
        base_url,
    })
}

pub(crate) fn model_provider<'a>(
    provider: Option<&'a str>,
    model: &str,
    base_url: Option<&str>,
) -> &'a str {
    if is_openrouter_binding(provider, base_url) {
        "openrouter"
    } else if is_anthropic_binding(provider, model) {
        "anthropic"
    } else {
        provider.unwrap_or("openai")
    }
}

/// OpenRouter is an OpenAI-compatible aggregator selected by its base URL (it
/// has no Responses API, so it can't be detected by model name the way native
/// Anthropic is). A binding pointed at `openrouter.ai` routes through the
/// OpenAI provider in Chat Completions mode.
fn is_openrouter_binding(provider: Option<&str>, base_url: Option<&str>) -> bool {
    provider == Some("openrouter")
        || (provider.is_none() && base_url.is_some_and(|url| url.contains("openrouter.ai")))
}

/// Anthropic model bindings route to the native Messages API. We detect them by
/// model name (`claude*`). Bedrock/Vertex Anthropic ids carry provider prefixes
/// (e.g. `us.anthropic.claude-...`) so they do not match here and keep falling
/// through to the OpenAI-compatible path.
fn is_anthropic_binding(provider: Option<&str>, model: &str) -> bool {
    provider == Some("anthropic")
        || (provider.is_none() && model.to_ascii_lowercase().starts_with("claude"))
}

fn request_auth_for_secret(provider: &str, secret: Secret) -> Result<ModelRequestAuth> {
    match secret {
        Secret::Key { .. } if provider == exoharness::OPENAI_CHATGPT_PROVIDER_ID => Err(
            anyhow::anyhow!("model provider `{provider}` requires an OAuth credential"),
        ),
        Secret::Key { value } if provider == "anthropic" => Ok(ModelRequestAuth {
            authorization: None,
            headers: HashMap::from([("x-api-key".to_string(), value)]),
        }),
        Secret::Key { value } => Ok(ModelRequestAuth {
            authorization: Some(format!("Bearer {value}")),
            headers: HashMap::new(),
        }),
        Secret::Oauth {
            provider: credential_provider,
            account_id,
            access_token,
            ..
        } => {
            let credential_provider = credential_provider
                .ok_or_else(|| anyhow::anyhow!("OAuth credential has no provider; log in again"))?;
            let access_token = access_token.ok_or_else(|| {
                anyhow::anyhow!(
                    "OAuth credential for provider `{credential_provider}` is logged out"
                )
            })?;
            if credential_provider != provider {
                return Err(anyhow::anyhow!(
                    "model provider `{provider}` cannot use an OAuth credential for `{credential_provider}`"
                ));
            }
            if provider != exoharness::OPENAI_CHATGPT_PROVIDER_ID {
                return Err(anyhow::anyhow!(
                    "OAuth request authentication is not implemented for provider `{provider}`"
                ));
            }
            let account_id = account_id.ok_or_else(|| {
                anyhow::anyhow!("OpenAI ChatGPT credential has no account id; log in again")
            })?;
            Ok(ModelRequestAuth {
                authorization: Some(format!("Bearer {access_token}")),
                headers: HashMap::from([
                    ("chatgpt-account-id".to_string(), account_id),
                    (
                        "OpenAI-Beta".to_string(),
                        "responses=experimental".to_string(),
                    ),
                ]),
            })
        }
    }
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

pub(crate) fn render_user_content(content: &UserContent) -> String {
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
    use exoharness::Secret;
    use serde_json::json;

    use super::{request_auth_for_secret, to_lingua_value};

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

    #[test]
    fn chatgpt_rejects_api_keys() {
        let error = request_auth_for_secret(
            exoharness::OPENAI_CHATGPT_PROVIDER_ID,
            Secret::Key {
                value: "api-key".to_string(),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("requires an OAuth credential"));
    }
}

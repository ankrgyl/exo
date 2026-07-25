use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use crate::harness_helpers::{model_provider, render_user_content, to_lingua_value};
use crate::{
    ModelClient, ModelRequest, ModelResponse, ModelResponseStream, PendingToolCall, ToolDefinition,
};
use async_trait::async_trait;
use braintrust_llm_router::{
    AuthConfig, ClientHeaders, ModelCatalog, ModelFlavor, ModelSpec, Router, create_provider,
};
use bytes::Bytes;
use exoharness::{Result, Uuid7};
use futures::{Stream, StreamExt};
use lingua::processing::adapter_for_format;
use lingua::providers::openai::generated::InputItem;
use lingua::providers::openai::universal_to_responses_input;
use lingua::serde_json as lingua_json;
use lingua::universal::{
    AssistantContent, AssistantContentPart, TextContentPart, TokenBudget, ToolCallArguments,
    ToolChoiceConfig, ToolChoiceMode, UniversalParams, UniversalRequest, UniversalResponse,
    UniversalStreamChunk, UniversalTool,
};
use lingua::{Message, ProviderFormat};
use reqwest::Url;
use serde::Serialize;

type RawChunkStream = Pin<
    Box<
        dyn Stream<
                Item = std::result::Result<
                    braintrust_llm_router::StreamChunk,
                    braintrust_llm_router::Error,
                >,
            > + Send,
    >,
>;

#[derive(Debug, Default, Clone)]
pub struct RouterModelClient {
    env: Arc<HashMap<String, String>>,
}

impl RouterModelClient {
    pub fn new(env: HashMap<String, String>) -> Self {
        Self { env: Arc::new(env) }
    }
}

#[async_trait]
impl ModelClient for RouterModelClient {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        if is_openai_chatgpt_request(&request) {
            let mut stream = self.complete_stream(request).await?;
            while stream.next_chunk().await?.is_some() {}
            return stream.finish().await;
        }
        let config = resolve_runtime_config(&request, self.env.as_ref())?;
        let format = config.format;
        let router = build_router(&request, format, &config)?;
        let universal_request = build_universal_request(&request, false)?;
        let payload = serialize_request(format, &universal_request)?;
        let route = resolve_provider_route(&router, &request.model, format)?;
        let (prepared, _router_metadata) = router.create_request(payload, format, &route).await?;
        let body = router.complete(prepared, &config.client_headers).await?;
        let provider_cost_usd = extract_provider_cost(&body);
        let response = lingua::response_to_universal(body)?;
        let mut model_response = normalize_model_response(response)?;
        model_response.provider_cost_usd = provider_cost_usd;
        Ok(model_response)
    }

    async fn complete_stream(&self, request: ModelRequest) -> Result<Box<dyn ModelResponseStream>> {
        let config = resolve_runtime_config(&request, self.env.as_ref())?;
        let format = config.format;
        let router = build_router(&request, format, &config)?;
        let universal_request = build_universal_request(&request, true)?;
        let payload = if is_openai_chatgpt_request(&request) {
            build_chatgpt_codex_payload(&universal_request, request.session_id.as_deref())?
        } else {
            serialize_request(format, &universal_request)?
        };
        let route = resolve_provider_route(&router, &request.model, format)?;
        let (prepared, _router_metadata) = router
            .create_stream_request(payload, format, &route)
            .await?;
        let raw_stream = router
            .complete_stream(prepared, &config.client_headers, None)
            .await?;
        Ok(Box::new(RouterModelResponseStream {
            raw: Box::pin(raw_stream),
            format,
            accumulator: UniversalResponseAccumulator::default(),
            provider_cost_usd: None,
        }))
    }
}

fn is_openai_chatgpt_request(request: &ModelRequest) -> bool {
    request.provider.as_deref() == Some(exoharness::OPENAI_CHATGPT_PROVIDER_ID)
}

struct RouterModelResponseStream {
    raw: RawChunkStream,
    format: ProviderFormat,
    accumulator: UniversalResponseAccumulator,
    provider_cost_usd: Option<f64>,
}

#[async_trait]
impl ModelResponseStream for RouterModelResponseStream {
    async fn next_chunk(&mut self) -> Result<Option<UniversalStreamChunk>> {
        // Map raw provider chunks to universal chunks inline (rather than via a
        // pre-mapped stream) so we can also read the provider-reported cost off
        // the raw bytes — it rides in `usage.cost` on the final chunk and isn't
        // preserved by lingua's UniversalUsage.
        loop {
            let Some(raw) = self.raw.next().await.transpose()? else {
                return Ok(None);
            };
            if let Some(cost) = extract_provider_cost(&raw.data) {
                self.provider_cost_usd = Some(cost);
            }
            match lingua::parse_stream_event(raw.data, self.format, self.format) {
                Ok(parsed) => {
                    if let Some(chunk) = parsed.universal {
                        self.accumulator.push(&chunk);
                        return Ok(Some(chunk));
                    }
                    // Non-content event (e.g. a usage-only final chunk): keep reading.
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn finish(self: Box<Self>) -> Result<ModelResponse> {
        let mut response = normalize_model_response(self.accumulator.finalize())?;
        response.provider_cost_usd = self.provider_cost_usd;
        Ok(response)
    }
}

#[derive(Debug, Clone)]
struct ResolvedRuntimeConfig {
    provider_alias: String,
    provider_kind: String,
    format: ProviderFormat,
    endpoint: Option<Url>,
    endpoint_template: Option<String>,
    metadata: HashMap<String, lingua_json::Value>,
    auth: AuthConfig,
    client_headers: ClientHeaders,
}

fn resolve_runtime_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    let provider = model_provider(
        request.provider.as_deref(),
        &request.model,
        request.base_url.as_deref(),
    );
    match provider {
        "anthropic" => resolve_anthropic_config(request, env),
        "openrouter" => resolve_openrouter_config(request, env),
        "openai" => resolve_openai_config(request, env),
        exoharness::OPENAI_CHATGPT_PROVIDER_ID => resolve_openai_chatgpt_config(request),
        provider => Err(anyhow::anyhow!("unsupported model provider `{provider}`")),
    }
}

fn resolve_openrouter_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    let key = request_auth_value(request, "authorization", Some("Bearer"))
        .or_else(|| optional_env(env, "OPENROUTER_API_KEY"))
        .ok_or_else(|| anyhow::anyhow!("model request is missing authorization"))?;
    let endpoint = request
        .base_url
        .clone()
        .map(|raw| Url::parse(&raw))
        .transpose()?;
    Ok(ResolvedRuntimeConfig {
        provider_alias: "openrouter".to_string(),
        // OpenRouter speaks the OpenAI Chat Completions wire format, so reuse
        // the OpenAI provider but force Chat Completions (not Responses).
        provider_kind: "openai".to_string(),
        format: ProviderFormat::ChatCompletions,
        endpoint,
        endpoint_template: None,
        metadata: HashMap::new(),
        auth: AuthConfig::ApiKey {
            key,
            header: Some("authorization".to_string()),
            prefix: Some("Bearer".to_string()),
        },
        client_headers: request_headers(request, Some("authorization"))?,
    })
}

fn resolve_anthropic_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    let key = request_auth_value(request, "x-api-key", None)
        .or_else(|| optional_env(env, "ANTHROPIC_API_KEY"))
        .ok_or_else(|| anyhow::anyhow!("model request is missing authorization"))?;
    // `None` lets the provider use its built-in default
    // (`https://api.anthropic.com/v1/`).
    let endpoint = request
        .base_url
        .clone()
        .or_else(|| optional_env(env, "ANTHROPIC_BASE_URL"))
        .map(|raw| Url::parse(&raw))
        .transpose()?;
    Ok(ResolvedRuntimeConfig {
        provider_alias: "anthropic".to_string(),
        provider_kind: "anthropic".to_string(),
        format: ProviderFormat::Anthropic,
        endpoint,
        endpoint_template: None,
        metadata: HashMap::new(),
        auth: AuthConfig::ApiKey {
            key,
            header: Some("x-api-key".to_string()),
            prefix: None,
        },
        client_headers: request_headers(request, Some("x-api-key"))?,
    })
}

fn resolve_openai_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    let key = request_auth_value(request, "authorization", Some("Bearer"))
        .or_else(|| optional_env(env, "OPENAI_API_KEY"))
        .ok_or_else(|| anyhow::anyhow!("model request is missing authorization"))?;
    let endpoint = request
        .base_url
        .clone()
        .or_else(|| optional_env(env, "OPENAI_BASE_URL"))
        .map(|raw| Url::parse(&raw))
        .transpose()?;
    let mut metadata = HashMap::new();
    if let Some(organization_id) = optional_env(env, "OPENAI_ORG_ID") {
        metadata.insert(
            "organization_id".to_string(),
            lingua_json::Value::String(organization_id),
        );
    }
    if let Some(project) = optional_env(env, "OPENAI_PROJECT") {
        metadata.insert("project".to_string(), lingua_json::Value::String(project));
    }
    Ok(ResolvedRuntimeConfig {
        provider_alias: "openai".to_string(),
        provider_kind: "openai".to_string(),
        format: ProviderFormat::Responses,
        endpoint,
        endpoint_template: None,
        metadata,
        auth: AuthConfig::ApiKey {
            key,
            header: Some("authorization".to_string()),
            prefix: Some("Bearer".to_string()),
        },
        client_headers: request_headers(request, Some("authorization"))?,
    })
}

fn resolve_openai_chatgpt_config(request: &ModelRequest) -> Result<ResolvedRuntimeConfig> {
    let key = request_auth_value(request, "authorization", Some("Bearer"))
        .ok_or_else(|| anyhow::anyhow!("OpenAI ChatGPT credential is logged out"))?;
    let endpoint = Some(Url::parse(
        request
            .base_url
            .as_deref()
            .unwrap_or("https://chatgpt.com/backend-api/codex"),
    )?);
    let mut client_headers = request_headers(request, Some("authorization"))?;
    for (name, value) in chatgpt_codex_headers(request) {
        client_headers.insert_user_configured(name, value)?;
    }
    Ok(ResolvedRuntimeConfig {
        provider_alias: exoharness::OPENAI_CHATGPT_PROVIDER_ID.to_string(),
        provider_kind: "openai".to_string(),
        format: ProviderFormat::Responses,
        endpoint,
        endpoint_template: None,
        metadata: HashMap::new(),
        auth: AuthConfig::ApiKey {
            key,
            header: Some("authorization".to_string()),
            prefix: Some("Bearer".to_string()),
        },
        client_headers,
    })
}

fn request_auth_value(
    request: &ModelRequest,
    header: &str,
    prefix: Option<&str>,
) -> Option<String> {
    let auth = request.auth.as_ref()?;
    if header.eq_ignore_ascii_case("authorization") {
        let authorization = auth.authorization.as_deref()?;
        return match prefix {
            Some(prefix) => {
                let (scheme, value) = authorization.split_once(char::is_whitespace)?;
                (scheme.eq_ignore_ascii_case(prefix) && !value.trim().is_empty())
                    .then(|| value.trim().to_string())
            }
            None => Some(authorization.to_string()),
        };
    }
    auth.headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(header))
        .map(|(_, value)| value.clone())
}

fn request_headers(request: &ModelRequest, auth_header: Option<&str>) -> Result<ClientHeaders> {
    let mut headers = ClientHeaders::new();
    if let Some(auth) = &request.auth {
        for (name, value) in &auth.headers {
            if auth_header.is_some_and(|auth_header| name.eq_ignore_ascii_case(auth_header)) {
                continue;
            }
            headers.insert_user_configured(name.clone(), value.clone())?;
        }
    }
    Ok(headers)
}

fn optional_env(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).cloned().or_else(|| std::env::var(key).ok())
}

fn build_router(
    request: &ModelRequest,
    format: ProviderFormat,
    config: &ResolvedRuntimeConfig,
) -> Result<Router> {
    let provider = create_provider(
        &config.provider_kind,
        config.endpoint.as_ref(),
        config.endpoint_template.as_deref(),
        None,
        &config.metadata,
        None,
    )?;

    let mut catalog = ModelCatalog::empty();
    catalog.insert(
        request.model.clone(),
        ModelSpec {
            model: request.model.clone(),
            format,
            flavor: match format {
                ProviderFormat::Responses => ModelFlavor::Responses,
                _ => ModelFlavor::Chat,
            },
            display_name: None,
            parent: None,
            input_cost_per_mil_tokens: None,
            output_cost_per_mil_tokens: None,
            input_cache_read_cost_per_mil_tokens: None,
            multimodal: None,
            reasoning: None,
            max_input_tokens: None,
            max_output_tokens: request.max_output_tokens.map(|tokens| tokens as u32),
            supports_streaming: true,
            extra: Default::default(),
            available_providers: vec![config.provider_alias.clone()],
        },
    );

    Router::builder()
        .with_catalog(std::sync::Arc::new(catalog))
        .add_provider_arc(
            config.provider_alias.clone(),
            provider,
            config.auth.clone(),
            vec![format],
        )
        .build()
        .map_err(Into::into)
}

fn resolve_provider_route(
    router: &Router,
    model: &str,
    format: ProviderFormat,
) -> Result<braintrust_llm_router::ProviderRoute> {
    router
        .resolve_provider_routes(model, format, &[])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no provider route resolved for model {model}"))
}

fn build_universal_request(request: &ModelRequest, stream: bool) -> Result<UniversalRequest> {
    let tools = build_universal_tools(&request.tools)?;
    let has_tools = !tools.is_empty();
    let params = UniversalParams {
        token_budget: request.max_output_tokens.map(TokenBudget::OutputTokens),
        tools: if tools.is_empty() { None } else { Some(tools) },
        tool_choice: has_tools.then_some(ToolChoiceConfig {
            mode: Some(ToolChoiceMode::Auto),
            tool_name: None,
        }),
        parallel_tool_calls: has_tools.then_some(true),
        stream: Some(stream),
        ..Default::default()
    };

    Ok(UniversalRequest {
        model: Some(request.model.clone()),
        messages: request.messages.clone(),
        params,
    })
}

#[derive(Serialize)]
struct ChatGptCodexBody {
    model: String,
    input: Vec<ChatGptInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<lingua_json::Value>>,
    tool_choice: &'static str,
    parallel_tool_calls: bool,
    reasoning: ChatGptReasoning,
    include: [&'static str; 1],
    prompt_cache_key: String,
    store: bool,
    stream: bool,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ChatGptInputItem {
    AdditionalTools(ChatGptAdditionalTools),
    Input(Box<InputItem>),
}

#[derive(Serialize)]
struct ChatGptAdditionalTools {
    #[serde(rename = "type")]
    item_type: &'static str,
    role: &'static str,
    tools: Vec<lingua_json::Value>,
}

#[derive(Serialize)]
struct ChatGptReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'static str>,
}

fn build_chatgpt_codex_payload(
    request: &UniversalRequest,
    session_id: Option<&str>,
) -> Result<Bytes> {
    let model = request
        .model
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OpenAI ChatGPT request is missing a model"))?;
    let responses_lite = chatgpt_codex_uses_responses_lite(model);
    let leading_instruction_count = request
        .messages
        .iter()
        .take_while(|message| matches!(message, Message::System { .. } | Message::Developer { .. }))
        .count();
    let leading_instructions = request
        .messages
        .iter()
        .take(leading_instruction_count)
        .map(|message| match message {
            Message::System { content } | Message::Developer { content } => {
                render_user_content(content)
            }
            _ => unreachable!("take_while only retains instruction messages"),
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    let input_messages = if responses_lite {
        request.messages.as_slice()
    } else {
        &request.messages[leading_instruction_count..]
    };
    let tools = request
        .params
        .tools
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(UniversalTool::to_responses_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut input = universal_to_responses_input(input_messages)?
        .into_iter()
        .map(|item| ChatGptInputItem::Input(Box::new(item)))
        .collect::<Vec<_>>();
    let parallel_tool_calls = !responses_lite && !tools.is_empty();
    let body_tools = if responses_lite {
        if !tools.is_empty() {
            input.insert(
                0,
                ChatGptInputItem::AdditionalTools(ChatGptAdditionalTools {
                    item_type: "additional_tools",
                    role: "developer",
                    tools,
                }),
            );
        }
        None
    } else {
        Some(tools)
    };
    let body = ChatGptCodexBody {
        model: model.to_string(),
        input,
        instructions: (!responses_lite && !leading_instructions.is_empty())
            .then(|| leading_instructions.join("\n\n")),
        tools: body_tools,
        tool_choice: "auto",
        parallel_tool_calls,
        reasoning: ChatGptReasoning {
            context: responses_lite.then_some("all_turns"),
        },
        include: ["reasoning.encrypted_content"],
        prompt_cache_key: session_id.unwrap_or("exo").to_string(),
        store: false,
        stream: true,
    };
    Ok(Bytes::from(lingua_json::to_vec(&body)?))
}

fn chatgpt_codex_headers(request: &ModelRequest) -> HashMap<String, String> {
    let session_id = request
        .session_id
        .clone()
        .unwrap_or_else(|| "exo".to_string());
    let mut headers = HashMap::from([
        ("session-id".to_string(), session_id.clone()),
        ("x-client-request-id".to_string(), session_id),
        ("accept".to_string(), "text/event-stream".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
    ]);
    if chatgpt_codex_uses_responses_lite(&request.model) {
        headers.insert(
            "x-openai-internal-codex-responses-lite".to_string(),
            "true".to_string(),
        );
    }
    headers
}

fn chatgpt_codex_uses_responses_lite(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower == "gpt-5.6" || lower.starts_with("gpt-5.6-")
}

fn build_universal_tools(tools: &[ToolDefinition]) -> Result<Vec<UniversalTool>> {
    tools.iter().map(tool_definition_to_universal).collect()
}

fn tool_definition_to_universal(tool: &ToolDefinition) -> Result<UniversalTool> {
    Ok(UniversalTool::function(
        tool.name.clone(),
        Some(tool.description.clone()),
        Some(to_lingua_value(tool.parameters.clone())),
        Some(true),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct SerializedChatStreamRequest {
        stream: Option<bool>,
        stream_options: Option<SerializedChatStreamOptions>,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedChatStreamOptions {
        include_usage: Option<bool>,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedResponsesStreamRequest {
        stream: Option<bool>,
        store: Option<bool>,
        stream_options: Option<SerializedResponsesStreamOptions>,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedResponsesStreamOptions {}

    #[derive(Debug, Deserialize)]
    struct SerializedChatGptBody {
        input: Vec<SerializedChatGptInput>,
        instructions: Option<String>,
        tools: Option<Vec<SerializedChatGptTool>>,
        parallel_tool_calls: bool,
        reasoning: SerializedChatGptReasoning,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedChatGptInput {
        #[serde(rename = "type")]
        item_type: Option<String>,
        role: Option<String>,
        tools: Option<Vec<SerializedChatGptTool>>,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedChatGptTool {
        name: String,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedChatGptReasoning {
        context: Option<String>,
    }

    fn model_request() -> ModelRequest {
        ModelRequest {
            model: "gpt-5.4".to_string(),
            provider: Some("openai".to_string()),
            auth: None,
            base_url: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: None,
            session_id: None,
        }
    }

    #[test]
    fn responses_stream_request_does_not_include_chat_usage_stream_option() {
        let request = build_universal_request(&model_request(), true).unwrap();
        let serialized = serialize_request(ProviderFormat::Responses, &request).unwrap();
        let payload: SerializedResponsesStreamRequest =
            lingua_json::from_slice(&serialized).unwrap();

        assert_eq!(payload.stream, Some(true));
        assert_eq!(payload.store, None);
        assert!(payload.stream_options.is_none());
    }

    #[test]
    fn openai_chatgpt_uses_subscription_endpoint() {
        let mut request = model_request();
        request.provider = Some(exoharness::OPENAI_CHATGPT_PROVIDER_ID.to_string());
        request.auth = Some(crate::ModelRequestAuth {
            authorization: Some("Bearer subscription-token".to_string()),
            headers: HashMap::new(),
        });
        let config = resolve_runtime_config(&request, &HashMap::new()).unwrap();
        assert_eq!(
            config.endpoint.unwrap().as_str(),
            "https://chatgpt.com/backend-api/codex"
        );
    }

    #[test]
    fn chatgpt_gpt_5_6_uses_responses_lite_contract() {
        let mut request = model_request();
        request.model = "gpt-5.6-terra".to_string();
        request.provider = Some(exoharness::OPENAI_CHATGPT_PROVIDER_ID.to_string());
        request.messages = vec![Message::Developer {
            content: lingua::universal::UserContent::String("Be concise.".to_string()),
        }];
        request.tools = vec![ToolDefinition {
            name: "weather".to_string(),
            description: "Get the weather".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        request.max_output_tokens = Some(100);

        let universal = build_universal_request(&request, true).unwrap();
        let serialized =
            build_chatgpt_codex_payload(&universal, request.session_id.as_deref()).unwrap();
        let payload: SerializedChatGptBody = lingua_json::from_slice(&serialized).unwrap();

        assert!(payload.instructions.is_none());
        assert!(payload.tools.is_none());
        assert!(!payload.parallel_tool_calls);
        assert_eq!(payload.reasoning.context.as_deref(), Some("all_turns"));
        assert_eq!(
            payload.input[0].item_type.as_deref(),
            Some("additional_tools")
        );
        assert_eq!(payload.input[0].role.as_deref(), Some("developer"));
        assert_eq!(payload.input[0].tools.as_ref().unwrap()[0].name, "weather");
        assert_eq!(
            chatgpt_codex_headers(&request)["x-openai-internal-codex-responses-lite"],
            "true"
        );
        assert!(
            !serialized
                .windows(b"max_output_tokens".len())
                .any(|window| { window == b"max_output_tokens" })
        );
    }

    #[test]
    fn pre_5_6_chatgpt_moves_leading_instructions_out_of_input() {
        let mut request = model_request();
        request.provider = Some(exoharness::OPENAI_CHATGPT_PROVIDER_ID.to_string());
        request.messages = vec![
            Message::Developer {
                content: lingua::universal::UserContent::String("Be concise.".to_string()),
            },
            Message::User {
                content: lingua::universal::UserContent::String("Hello".to_string()),
            },
        ];

        let universal = build_universal_request(&request, true).unwrap();
        let serialized =
            build_chatgpt_codex_payload(&universal, request.session_id.as_deref()).unwrap();
        let payload: SerializedChatGptBody = lingua_json::from_slice(&serialized).unwrap();

        assert_eq!(payload.instructions.as_deref(), Some("Be concise."));
        assert!(payload.tools.is_some());
        assert!(!payload.parallel_tool_calls);
        assert!(payload.reasoning.context.is_none());
        assert_eq!(payload.input.len(), 1);
        assert_eq!(payload.input[0].role.as_deref(), Some("user"));
    }

    #[test]
    fn anthropic_models_route_to_the_native_messages_api() {
        let mut request = model_request();
        request.model = "claude-sonnet-4-6".to_string();
        request.provider = None;
        request.auth = Some(crate::ModelRequestAuth {
            authorization: None,
            headers: HashMap::from([("x-api-key".to_string(), "sk-ant-test".to_string())]),
        });

        let config = resolve_runtime_config(&request, &HashMap::new()).unwrap();

        assert_eq!(config.provider_kind, "anthropic");
        assert_eq!(config.format, ProviderFormat::Anthropic);
        assert!(matches!(
            config.auth,
            AuthConfig::ApiKey { ref header, ref prefix, .. }
                if header.as_deref() == Some("x-api-key") && prefix.is_none()
        ));
    }

    #[test]
    fn non_anthropic_models_keep_the_openai_responses_route() {
        let mut request = model_request();
        request.auth = Some(crate::ModelRequestAuth {
            authorization: Some("Bearer sk-test".to_string()),
            headers: HashMap::new(),
        });

        let config = resolve_runtime_config(&request, &HashMap::new()).unwrap();

        assert_eq!(config.provider_kind, "openai");
        assert_eq!(config.format, ProviderFormat::Responses);
    }

    #[test]
    fn openrouter_bindings_use_openai_chat_completions() {
        let mut request = model_request();
        request.model = "openai/gpt-4o-mini".to_string();
        request.provider = None;
        request.auth = Some(crate::ModelRequestAuth {
            authorization: Some("Bearer sk-or-test".to_string()),
            headers: HashMap::new(),
        });
        request.base_url = Some("https://openrouter.ai/api/v1".to_string());

        let config = resolve_runtime_config(&request, &HashMap::new()).unwrap();

        assert_eq!(config.provider_alias, "openrouter");
        assert_eq!(config.provider_kind, "openai");
        assert_eq!(config.format, ProviderFormat::ChatCompletions);
        assert!(matches!(
            config.auth,
            AuthConfig::ApiKey { ref header, ref prefix, .. }
                if header.as_deref() == Some("authorization")
                    && prefix.as_deref() == Some("Bearer")
        ));
    }

    #[test]
    fn extracts_provider_reported_cost_from_usage() {
        let body = br#"{"usage":{"prompt_tokens":16,"completion_tokens":6,"cost":0.000006}}"#;
        assert_eq!(extract_provider_cost(body), Some(0.000006));

        // No cost field (OpenAI/Anthropic) -> None, and cheaply skipped.
        let no_cost = br#"{"usage":{"prompt_tokens":16,"completion_tokens":6}}"#;
        assert_eq!(extract_provider_cost(no_cost), None);
    }

    #[test]
    fn chat_completions_stream_request_includes_lingua_usage_stream_option() {
        let request = build_universal_request(&model_request(), true).unwrap();
        let serialized = serialize_request(ProviderFormat::ChatCompletions, &request).unwrap();
        let payload: SerializedChatStreamRequest = lingua_json::from_slice(&serialized).unwrap();

        assert_eq!(payload.stream, Some(true));
        assert_eq!(
            payload
                .stream_options
                .and_then(|options| options.include_usage),
            Some(true)
        );
    }

    #[test]
    fn stream_accumulator_does_not_treat_chunk_id_as_assistant_message_id() {
        let mut accumulator = UniversalResponseAccumulator::default();
        accumulator.push(&UniversalStreamChunk::new(
            Some("resp_123".to_string()),
            Some("gpt-5.4".to_string()),
            vec![lingua::UniversalStreamChoice::text_delta(0, "hello")],
            None,
            None,
        ));

        let response = accumulator.finalize();

        assert!(matches!(
            response.messages.as_slice(),
            [Message::Assistant { id: None, .. }]
        ));
    }

    #[test]
    fn stream_accumulator_drops_tool_call_slots_without_id_and_name() {
        let mut accumulator = UniversalResponseAccumulator::default();
        accumulator.push(&UniversalStreamChunk::new(
            Some("resp_123".to_string()),
            Some("gpt-5.4".to_string()),
            vec![lingua::UniversalStreamChoice {
                index: 0,
                delta: Some(lingua_json::json!({
                    "role": "assistant",
                    "tool_calls": [
                        { "index": 0 },
                        {
                            "index": 1,
                            "id": "call_real",
                            "function": { "name": "shell", "arguments": "{}" }
                        }
                    ]
                })),
                finish_reason: None,
            }],
            None,
            None,
        ));

        let response = accumulator.finalize();

        let Some(Message::Assistant {
            content: AssistantContent::Array(parts),
            ..
        }) = response.messages.first()
        else {
            panic!("expected a single Assistant message with array content");
        };
        let tool_calls: Vec<&AssistantContentPart> = parts
            .iter()
            .filter(|part| matches!(part, AssistantContentPart::ToolCall { .. }))
            .collect();
        assert_eq!(tool_calls.len(), 1);
        let AssistantContentPart::ToolCall {
            tool_call_id,
            tool_name,
            ..
        } = tool_calls[0]
        else {
            unreachable!();
        };
        assert_eq!(tool_call_id, "call_real");
        assert_eq!(tool_name, "shell");
    }
}

fn serialize_request(format: ProviderFormat, request: &UniversalRequest) -> Result<Bytes> {
    let adapter = adapter_for_format(format)
        .ok_or_else(|| anyhow::anyhow!("unsupported provider format for request: {format}"))?;
    let payload = adapter.request_from_universal(request)?;
    Ok(Bytes::from(lingua_json::to_vec(&payload)?))
}

/// Some providers (e.g. OpenRouter) report the authoritative dollar cost of a
/// request in `usage.cost`. lingua's `UniversalUsage` doesn't carry that field,
/// so we read it straight off the raw response/stream JSON. Returns `None` when
/// absent (the common case — OpenAI and Anthropic don't send it).
fn extract_provider_cost(data: &[u8]) -> Option<f64> {
    // Cheap guard so we don't JSON-parse every streamed content chunk; only the
    // final usage chunk carries a cost field.
    if !data.windows(6).any(|window| window == b"\"cost\"") {
        return None;
    }
    let value: lingua_json::Value = lingua_json::from_slice(data).ok()?;
    value.get("usage")?.get("cost")?.as_f64()
}

fn normalize_model_response(response: UniversalResponse) -> Result<ModelResponse> {
    let response_id = Some(Uuid7::now());
    let tool_calls = extract_tool_calls(&response.messages)?;

    Ok(ModelResponse {
        provider_cost_usd: None,
        response_id,
        messages: response.messages,
        tool_calls,
        usage: response.usage,
        model: response.model,
        ttft: None,
        duration: None,
    })
}

fn extract_tool_calls(messages: &[Message]) -> Result<Vec<PendingToolCall>> {
    let mut tool_calls = Vec::new();

    for message in messages {
        let Message::Assistant { content, .. } = message else {
            continue;
        };
        let AssistantContent::Array(parts) = content else {
            continue;
        };

        for part in parts {
            if let AssistantContentPart::ToolCall {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } = part
            {
                let ToolCallArguments::Valid(arguments) = arguments else {
                    continue;
                };
                tool_calls.push(PendingToolCall {
                    tool_call_id: tool_call_id.clone(),
                    request: exoharness::ToolRequest {
                        function_name: tool_name.clone(),
                        arguments: to_exoharness_arguments(arguments)?,
                    },
                });
            }
        }
    }

    Ok(tool_calls)
}

fn to_exoharness_arguments(
    arguments: &lingua::serde_json::Map<String, lingua::serde_json::Value>,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let serialized = lingua_json::to_string(arguments)?;
    Ok(serde_json::from_str(&serialized)?)
}

#[derive(Default)]
struct UniversalResponseAccumulator {
    model: Option<String>,
    usage: Option<lingua::UniversalUsage>,
    text: String,
    reasoning: Vec<String>,
    tool_calls: Vec<lingua::UniversalToolCallDelta>,
}

impl UniversalResponseAccumulator {
    fn push(&mut self, chunk: &UniversalStreamChunk) {
        if chunk.is_keep_alive() {
            return;
        }

        if self.model.is_none() {
            self.model = chunk.model.clone();
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.clone());
        }

        for choice in &chunk.choices {
            let Some(delta) = choice.delta_view() else {
                continue;
            };

            if let Some(content) = delta.content {
                self.text.push_str(&content);
            }

            for reasoning in delta.reasoning {
                if let Some(content) = reasoning.content {
                    self.reasoning.push(content);
                }
            }

            for (fallback_index, tool_call) in delta.tool_calls.into_iter().enumerate() {
                let index = tool_call
                    .index
                    .map(|value| value as usize)
                    .unwrap_or(fallback_index);
                if self.tool_calls.len() <= index {
                    self.tool_calls
                        .resize_with(index + 1, lingua::UniversalToolCallDelta::default);
                }
                let accumulator = &mut self.tool_calls[index];
                if accumulator.id.is_none() {
                    accumulator.id = tool_call.id;
                }
                if accumulator.call_type.is_none() {
                    accumulator.call_type = tool_call.call_type;
                }
                let accumulator_function = accumulator
                    .function
                    .get_or_insert_with(lingua::UniversalToolFunctionDelta::default);
                if let Some(function) = tool_call.function {
                    if accumulator_function.name.is_none() {
                        accumulator_function.name = function.name;
                    }
                    if let Some(arguments_delta) = function.arguments {
                        let current = accumulator_function.arguments.take().unwrap_or_default();
                        accumulator_function.arguments = Some(current + &arguments_delta);
                    }
                }
            }
        }
    }

    fn finalize(&self) -> UniversalResponse {
        let mut content_parts = Vec::new();

        if !self.text.is_empty() {
            content_parts.push(AssistantContentPart::Text(TextContentPart {
                text: self.text.clone(),
                encrypted_content: None,
                provider_options: None,
                cache_control: None,
            }));
        }

        for reasoning in &self.reasoning {
            content_parts.push(AssistantContentPart::Reasoning {
                text: reasoning.clone(),
                encrypted_content: None,
            });
        }

        for tool_call in &self.tool_calls {
            let Some(id) = tool_call.id.clone() else {
                continue;
            };
            let function = tool_call.function.clone().unwrap_or_default();
            let Some(name) = function.name else {
                continue;
            };
            content_parts.push(AssistantContentPart::ToolCall {
                tool_call_id: id,
                tool_name: name,
                arguments: ToolCallArguments::from(function.arguments.unwrap_or_default()),
                encrypted_content: None,
                provider_options: None,
                provider_executed: None,
            });
        }

        let assistant_message = if content_parts.is_empty() {
            None
        } else if content_parts.len() == 1 {
            match &content_parts[0] {
                AssistantContentPart::Text(text) => Some(Message::Assistant {
                    content: AssistantContent::String(text.text.clone()),
                    id: None,
                }),
                _ => Some(Message::Assistant {
                    content: AssistantContent::Array(content_parts),
                    id: None,
                }),
            }
        } else {
            Some(Message::Assistant {
                content: AssistantContent::Array(content_parts),
                id: None,
            })
        };

        UniversalResponse {
            id: None,
            id_format: None,
            model: self.model.clone(),
            messages: assistant_message.into_iter().collect(),
            usage: self.usage.clone(),
            finish_reason: None,
        }
    }
}

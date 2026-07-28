use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use crate::harness_helpers::to_lingua_value;
use crate::{
    ModelClient, ModelRequest, ModelResponse, ModelResponseStream, PendingToolCall, ToolDefinition,
};
use async_trait::async_trait;
use braintrust_llm_router::{
    AuthConfig, ClientHeaders, ModelCatalog, ModelFlavor, ModelSpec, Router, create_provider,
};
use bytes::Bytes;
use exoharness::{AuthScheme, Result, Uuid7, WireFormat};
use futures::{Stream, StreamExt};
use lingua::processing::adapter_for_format;
use lingua::serde_json as lingua_json;
use lingua::universal::{
    AssistantContent, AssistantContentPart, TextContentPart, TokenBudget, ToolCallArguments,
    ToolChoiceConfig, ToolChoiceMode, UniversalParams, UniversalRequest, UniversalResponse,
    UniversalStreamChunk, UniversalTool,
};
use lingua::{Message, ProviderFormat};
use reqwest::Url;

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
        let config = resolve_runtime_config(&request, self.env.as_ref())?;
        let format = config.format;
        let router = build_router(&request, format, &config)?;
        let universal_request = build_universal_request(&request, false)?;
        let payload = serialize_request(format, &universal_request)?;
        let route = resolve_provider_route(&router, &request.model, format)?;
        let (prepared, _router_metadata) = router.create_request(payload, format, &route).await?;
        let body = router.complete(prepared, &ClientHeaders::new()).await?;
        let cost_usage_path = request
            .provider
            .as_ref()
            .and_then(|provider| provider.cost_usage_path.as_deref());
        let provider_cost_usd = extract_provider_cost(&body, cost_usage_path);
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
        let payload = serialize_request(format, &universal_request)?;
        let route = resolve_provider_route(&router, &request.model, format)?;
        let (prepared, _router_metadata) = router
            .create_stream_request(payload, format, &route)
            .await?;
        let raw_stream = router
            .complete_stream(prepared, &ClientHeaders::new(), None)
            .await?;
        Ok(Box::new(RouterModelResponseStream {
            raw: Box::pin(raw_stream),
            format,
            accumulator: UniversalResponseAccumulator::default(),
            provider_cost_usd: None,
            cost_usage_path: request
                .provider
                .as_ref()
                .and_then(|provider| provider.cost_usage_path.clone()),
        }))
    }
}

struct RouterModelResponseStream {
    raw: RawChunkStream,
    format: ProviderFormat,
    accumulator: UniversalResponseAccumulator,
    provider_cost_usd: Option<f64>,
    cost_usage_path: Option<Vec<String>>,
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
            if let Some(cost) = extract_provider_cost(&raw.data, self.cost_usage_path.as_deref()) {
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
}

fn resolve_runtime_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    if let Some(provider) = &request.provider {
        resolve_declared_config(provider, request)
    } else if is_anthropic_model(&request.model) {
        resolve_anthropic_config(request, env)
    } else if is_openrouter_request(request) {
        resolve_openrouter_config(request, env)
    } else {
        resolve_openai_config(request, env)
    }
}

/// A registered provider record (`Binding::Provider`) declares its wire
/// format, auth scheme, and endpoint as data, so no name/base-URL detection
/// heuristics apply. The credential comes from the provider's secret via the
/// binding resolution — there is no env-var fallback for registered providers.
fn resolve_declared_config(
    provider: &crate::ModelRequestProvider,
    request: &ModelRequest,
) -> Result<ResolvedRuntimeConfig> {
    let endpoint = request
        .base_url
        .clone()
        .map(|raw| {
            // Base-URL contract: provider records store the provider root
            // (e.g. https://api.opper.ai/v3/compat) and each client appends
            // its own path — the Anthropic TS SDK adds `/v1/messages` itself,
            // while the Rust provider joins `messages` onto the endpoint. So
            // extend the root with `v1/` here (the trailing slash makes
            // `Url::join` append rather than replace the last segment).
            // Registration rejects `/v1`-suffixed roots for this format.
            let raw = if matches!(provider.format, WireFormat::Anthropic) {
                format!("{}/v1/", raw.trim_end_matches('/'))
            } else {
                raw
            };
            Url::parse(&raw)
        })
        .transpose()?;
    let (provider_kind, format) = match provider.format {
        WireFormat::ChatCompletions => ("openai", ProviderFormat::ChatCompletions),
        WireFormat::Responses => ("openai", ProviderFormat::Responses),
        WireFormat::Anthropic => ("anthropic", ProviderFormat::Anthropic),
    };
    // Auth scheme and wire format are independent properties: a gateway can
    // speak the Anthropic format but authenticate with `Bearer`. Absent an
    // explicit scheme, derive the format's native one.
    let scheme = provider.auth.unwrap_or(match provider.format {
        WireFormat::Anthropic => AuthScheme::XApiKey,
        _ => AuthScheme::Bearer,
    });
    let auth = match scheme {
        AuthScheme::None => AuthConfig::Custom {
            headers: HashMap::new(),
        },
        AuthScheme::Bearer | AuthScheme::XApiKey => {
            let key = request.api_key.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "model request is missing an API key (provider {} has no secret)",
                    provider.name
                )
            })?;
            let (header, prefix) = match scheme {
                AuthScheme::Bearer => ("authorization", Some("Bearer".to_string())),
                _ => ("x-api-key", None),
            };
            AuthConfig::ApiKey {
                key,
                header: Some(header.to_string()),
                prefix,
            }
        }
    };
    Ok(ResolvedRuntimeConfig {
        provider_alias: provider.name.clone(),
        provider_kind: provider_kind.to_string(),
        format,
        endpoint,
        endpoint_template: None,
        metadata: HashMap::new(),
        auth,
    })
}

/// OpenRouter is an OpenAI-compatible aggregator selected by its base URL (it
/// has no Responses API, so it can't be detected by model name the way native
/// Anthropic is). A binding pointed at `openrouter.ai` routes through the
/// OpenAI provider in Chat Completions mode.
fn is_openrouter_request(request: &ModelRequest) -> bool {
    request
        .base_url
        .as_deref()
        .is_some_and(|url| url.contains("openrouter.ai"))
}

fn resolve_openrouter_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    let key = request
        .api_key
        .clone()
        .or_else(|| optional_env(env, "OPENROUTER_API_KEY"))
        .ok_or_else(|| anyhow::anyhow!("model request is missing an API key"))?;
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
    })
}

/// Anthropic model bindings route to the native Messages API. We detect them by
/// model name (`claude*`). Bedrock/Vertex Anthropic ids carry provider prefixes
/// (e.g. `us.anthropic.claude-...`) so they do not match here and keep falling
/// through to the OpenAI-compatible path.
fn is_anthropic_model(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("claude")
}

fn resolve_anthropic_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    let key = request
        .api_key
        .clone()
        .or_else(|| optional_env(env, "ANTHROPIC_API_KEY"))
        .ok_or_else(|| anyhow::anyhow!("model request is missing an API key"))?;
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
    })
}

fn resolve_openai_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    let key = request
        .api_key
        .clone()
        .or_else(|| optional_env(env, "OPENAI_API_KEY"))
        .ok_or_else(|| anyhow::anyhow!("model request is missing an API key"))?;
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
    })
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
        stream_options: Option<SerializedResponsesStreamOptions>,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedResponsesStreamOptions {}

    fn model_request() -> ModelRequest {
        ModelRequest {
            model: "gpt-5.4".to_string(),
            api_key: None,
            base_url: None,
            provider: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: None,
        }
    }

    #[test]
    fn responses_stream_request_does_not_include_chat_usage_stream_option() {
        let request = build_universal_request(&model_request(), true).unwrap();
        let serialized = serialize_request(ProviderFormat::Responses, &request).unwrap();
        let payload: SerializedResponsesStreamRequest =
            lingua_json::from_slice(&serialized).unwrap();

        assert_eq!(payload.stream, Some(true));
        assert!(payload.stream_options.is_none());
    }

    #[test]
    fn anthropic_models_route_to_the_native_messages_api() {
        let mut request = model_request();
        request.model = "claude-sonnet-4-6".to_string();
        request.api_key = Some("sk-ant-test".to_string());

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
        request.api_key = Some("sk-test".to_string());

        let config = resolve_runtime_config(&request, &HashMap::new()).unwrap();

        assert_eq!(config.provider_kind, "openai");
        assert_eq!(config.format, ProviderFormat::Responses);
    }

    #[test]
    fn openrouter_bindings_use_openai_chat_completions() {
        let mut request = model_request();
        request.model = "openai/gpt-4o-mini".to_string();
        request.api_key = Some("sk-or-test".to_string());
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
    fn declared_provider_format_overrides_detection_heuristics() {
        let mut request = model_request();
        // A model name that would otherwise route to the native Anthropic path.
        request.model = "claude-sonnet-4-6".to_string();
        request.api_key = Some("op-test".to_string());
        request.base_url = Some("https://api.opper.ai/v3/compat".to_string());
        request.provider = Some(crate::ModelRequestProvider {
            name: "opper".to_string(),
            format: WireFormat::ChatCompletions,
            auth: None,
            cost_usage_path: None,
        });

        let config = resolve_runtime_config(&request, &HashMap::new()).unwrap();

        assert_eq!(config.provider_alias, "opper");
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
    fn declared_anthropic_format_uses_native_auth() {
        let mut request = model_request();
        request.api_key = Some("sk-ant-test".to_string());
        request.base_url = Some("https://anthropic.example/v1".to_string());
        request.provider = Some(crate::ModelRequestProvider {
            name: "anthropic-proxy".to_string(),
            format: WireFormat::Anthropic,
            auth: None,
            cost_usage_path: None,
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
    fn declared_auth_scheme_is_independent_of_wire_format() {
        // Anthropic wire format with Bearer auth — e.g. Opper's
        // Anthropic-compatible endpoint, which is Bearer-only.
        let mut request = model_request();
        request.api_key = Some("op-test".to_string());
        request.base_url = Some("https://api.opper.ai/v3/compat".to_string());
        request.provider = Some(crate::ModelRequestProvider {
            name: "opper".to_string(),
            format: WireFormat::Anthropic,
            auth: Some(AuthScheme::Bearer),
            cost_usage_path: None,
        });

        let config = resolve_runtime_config(&request, &HashMap::new()).unwrap();

        assert_eq!(config.provider_kind, "anthropic");
        assert_eq!(config.format, ProviderFormat::Anthropic);
        assert!(matches!(
            config.auth,
            AuthConfig::ApiKey { ref header, ref prefix, .. }
                if header.as_deref() == Some("authorization")
                    && prefix.as_deref() == Some("Bearer")
        ));
    }

    #[test]
    fn declared_anthropic_endpoint_extends_the_provider_root_with_v1() {
        // The record stores the provider root (registration rejects a
        // /v1-suffixed value); trailing slash or not must not matter.
        for base in [
            "https://api.opper.ai/v3/compat",
            "https://api.opper.ai/v3/compat/",
        ] {
            let mut request = model_request();
            request.api_key = Some("op-test".to_string());
            request.base_url = Some(base.to_string());
            request.provider = Some(crate::ModelRequestProvider {
                name: "opper".to_string(),
                format: WireFormat::Anthropic,
                auth: Some(AuthScheme::Bearer),
                cost_usage_path: None,
            });

            let config = resolve_runtime_config(&request, &HashMap::new()).unwrap();

            let endpoint = config.endpoint.expect("endpoint should be set");
            assert_eq!(
                endpoint.join("messages").unwrap().as_str(),
                "https://api.opper.ai/v3/compat/v1/messages",
                "base {base}"
            );
        }
    }

    #[test]
    fn declared_auth_none_sends_no_credential_and_needs_no_key() {
        let mut request = model_request();
        request.base_url = Some("http://localhost:11434/v1".to_string());
        request.provider = Some(crate::ModelRequestProvider {
            name: "local".to_string(),
            format: WireFormat::ChatCompletions,
            auth: Some(AuthScheme::None),
            cost_usage_path: None,
        });

        let config = resolve_runtime_config(&request, &HashMap::new()).unwrap();

        assert!(matches!(
            config.auth,
            AuthConfig::Custom { ref headers } if headers.is_empty()
        ));
    }

    #[test]
    fn declared_provider_without_key_is_an_error() {
        let mut request = model_request();
        request.provider = Some(crate::ModelRequestProvider {
            name: "gateway".to_string(),
            format: WireFormat::ChatCompletions,
            auth: None,
            cost_usage_path: None,
        });

        assert!(resolve_runtime_config(&request, &HashMap::new()).is_err());
    }

    /// Live end-to-end check for the declared-provider path against a real
    /// OpenAI-compatible gateway (Opper). Ignored by default; run with:
    ///   OPPER_API_KEY=... cargo test -p executor --lib provider_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "live: requires OPPER_API_KEY and network access"]
    async fn provider_live_declared_chat_completions_end_to_end() {
        let key = std::env::var("OPPER_API_KEY")
            .expect("set OPPER_API_KEY to run the live provider test");
        let client = RouterModelClient::new(HashMap::new());
        let request = ModelRequest {
            model: "openai/gpt-4.1-mini".to_string(),
            api_key: Some(key),
            base_url: Some("https://api.opper.ai/v3/compat".to_string()),
            provider: Some(crate::ModelRequestProvider {
                name: "opper".to_string(),
                format: WireFormat::ChatCompletions,
                auth: None,
                cost_usage_path: None,
            }),
            messages: vec![Message::User {
                content: lingua::universal::UserContent::String(
                    "Reply with exactly: provider-binding-ok".to_string(),
                ),
            }],
            tools: Vec::new(),
            max_output_tokens: Some(30),
        };

        let response = client
            .complete(request)
            .await
            .expect("live completion through the declared provider should succeed");

        let text = response
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant { content, .. } => Some(match content {
                    AssistantContent::String(text) => text.clone(),
                    AssistantContent::Array(parts) => parts
                        .iter()
                        .filter_map(|part| match part {
                            AssistantContentPart::Text(text) => Some(text.text.clone()),
                            _ => None,
                        })
                        .collect::<String>(),
                }),
                _ => None,
            })
            .collect::<String>();

        assert!(
            text.contains("provider-binding-ok"),
            "unexpected assistant content: {text:?}"
        );
    }

    #[test]
    fn extracts_provider_reported_cost_from_usage() {
        let body = br#"{"usage":{"prompt_tokens":16,"completion_tokens":6,"cost":0.000006}}"#;
        assert_eq!(extract_provider_cost(body, None), Some(0.000006));

        // A provider-declared path reads nested vendor extensions (Opper).
        let opper = br#"{"usage":{"prompt_tokens":14,"opper":{"cost":{"total":0.0000035}}}}"#;
        let path = vec!["opper".to_string(), "cost".to_string(), "total".to_string()];
        assert_eq!(extract_provider_cost(opper, Some(&path)), Some(0.0000035));
        // The default path does not read the nested shape, and vice versa.
        assert_eq!(extract_provider_cost(opper, None), None);
        assert_eq!(extract_provider_cost(body, Some(&path)), None);

        // No cost field (OpenAI/Anthropic) -> None, and cheaply skipped.
        let no_cost = br#"{"usage":{"prompt_tokens":16,"completion_tokens":6}}"#;
        assert_eq!(extract_provider_cost(no_cost, None), None);
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
/// Reads the provider-reported spend from a response body. Cost is not part
/// of the standard chat completions `usage` schema, so providers extend it in
/// different places; `path` (from the provider record's `cost_usage_path`)
/// selects the location under `usage`, defaulting to OpenRouter-style
/// top-level `usage.cost`.
fn extract_provider_cost(data: &[u8], path: Option<&[String]>) -> Option<f64> {
    // Cheap guard so we don't JSON-parse every streamed content chunk; only
    // the final usage chunk carries the cost field.
    let last = path
        .and_then(|path| path.last().map(String::as_str))
        .unwrap_or("cost");
    let needle = format!("\"{last}\"");
    if !data
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
    {
        return None;
    }
    let value: lingua_json::Value = lingua_json::from_slice(data).ok()?;
    let mut node = value.get("usage")?;
    match path {
        Some(path) => {
            for segment in path {
                node = node.get(segment)?;
            }
        }
        None => node = node.get("cost")?,
    }
    node.as_f64()
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
                        namespace: None,
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

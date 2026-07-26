use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use exoharness::{
    AgentHandle, ConversationHandle, DurableFileSystem, EventId, FileSystemMount, ResponseId,
    Result, SandboxProvider, SessionId, ToolArguments, ToolCallId, ToolRequest, ToolResult,
    TurnHandle, TurnId,
};
use lingua::{Message, UniversalStreamChunk, UniversalUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::OwnedMutexGuard;
use tokio_stream::{Stream, wrappers::UnboundedReceiverStream};

use crate::braintrust::BraintrustTracingConfig;

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct AgentConfig {
    pub instructions: Vec<Message>,
    #[serde(default)]
    pub harness: AgentHarnessKind,
    #[serde(default)]
    pub typescript: Option<TypeScriptHarnessConfig>,
    #[serde(default = "default_enable_agent_tool_creation")]
    pub enable_agent_tool_creation: bool,
    pub sandbox: AgentSandboxConfig,
    pub model: String,
    pub max_output_tokens: Option<i64>,
    pub max_tool_round_trips: Option<u32>,
    pub braintrust: Option<BraintrustTracingConfig>,
}

/// Agent-level defaults for conversation sandboxes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSandboxConfig {
    /// Default scope for conversation that don't specify a `sandbox_scope`.
    #[serde(default)]
    pub scope: SandboxScope,
    #[serde(default)]
    pub image: Option<String>,
    pub provider: SandboxProvider,
    /// Mounts for the agent-scoped sandbox. These apply to every conversation
    /// that uses the shared agent sandbox.
    #[serde(default)]
    pub mounts: Vec<FileSystemMount>,
    #[serde(default)]
    pub enable_networking: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHarnessKind {
    #[default]
    Basic,
    Rlm,
    #[serde(rename = "typescript")]
    TypeScript,
    Exo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeScriptHarnessConfig {
    pub module_path: String,
    #[serde(default)]
    pub tool_module_paths: Vec<String>,
}

pub fn default_enable_agent_tool_creation() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ConversationConfig {
    #[serde(default)]
    pub sandbox_image: Option<String>,
    #[serde(default)]
    pub sandbox_provider: Option<SandboxProvider>,
    pub shell_program: Option<String>,
    #[serde(default)]
    pub mounts: Vec<FileSystemMount>,
    #[serde(default)]
    pub durable_file_systems: Vec<DurableFileSystem>,
    #[serde(default)]
    pub sandbox_scope: Option<SandboxScope>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxScope {
    Agent,
    #[default]
    Conversation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationModelConfig {
    pub model: String,
    pub max_output_tokens: Option<i64>,
}

impl fmt::Display for ConversationModelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "model={}, max_output_tokens={}",
            self.model,
            self.max_output_tokens
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string())
        )
    }
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            sandbox_image: None,
            sandbox_provider: None,
            shell_program: Some("/bin/bash".to_string()),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            sandbox_scope: None,
        }
    }
}

pub fn effective_sandbox_scope(
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
) -> SandboxScope {
    conversation_config
        .sandbox_scope
        .unwrap_or(agent_config.sandbox.scope)
}

impl ConversationConfig {
    pub fn effective_sandbox_image<'a>(&'a self, agent_config: &'a AgentConfig) -> Option<&'a str> {
        self.sandbox_image
            .as_deref()
            .or(agent_config.sandbox.image.as_deref())
    }

    pub fn effective_sandbox_provider(&self, agent_config: &AgentConfig) -> SandboxProvider {
        self.sandbox_provider
            .unwrap_or(agent_config.sandbox.provider)
    }
}

#[async_trait]
pub trait ModelClient: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse>;
    async fn complete_stream(&self, request: ModelRequest) -> Result<Box<dyn ModelResponseStream>>;
}

#[async_trait]
pub trait ModelResponseStream: Send {
    async fn next_chunk(&mut self) -> Result<Option<UniversalStreamChunk>>;
    async fn finish(self: Box<Self>) -> Result<ModelResponse>;
}

#[async_trait]
pub trait ToolRuntime: Send + Sync {
    async fn prepare_conversation(
        &self,
        _agent: &dyn AgentHandle,
        _conversation: &dyn ConversationHandle,
        _agent_config: &AgentConfig,
        _config: &ConversationConfig,
    ) -> Result<()> {
        Ok(())
    }

    async fn execute(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Option<&dyn TurnHandle>,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
        request: &ToolRequest,
    ) -> Result<ToolResult>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub response_id: Option<ResponseId>,
    pub messages: Vec<Message>,
    pub tool_calls: Vec<PendingToolCall>,
    pub usage: Option<UniversalUsage>,
    /// Model identifier echoed back by the provider, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Time to first token (streaming path only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft: Option<Duration>,
    /// Wall-clock duration from request start to end of response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<Duration>,
    /// Authoritative cost in USD reported by the provider (e.g. OpenRouter's
    /// `usage.cost`), if any. Preferred over the price-table estimate when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolCall {
    pub tool_call_id: ToolCallId,
    pub request: ToolRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendRequest {
    pub input: Vec<Message>,
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone)]
pub struct SendResult {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub latest_event_id: EventId,
}

pub struct ExecutionStreamHandle {
    event_stream: UnboundedReceiverStream<Result<ExecutionStreamEvent>>,
    _send_guard: Option<OwnedMutexGuard<()>>,
}

impl ExecutionStreamHandle {
    pub fn new(event_stream: UnboundedReceiverStream<Result<ExecutionStreamEvent>>) -> Self {
        Self {
            event_stream,
            _send_guard: None,
        }
    }

    pub(crate) fn with_send_guard(mut self, send_guard: OwnedMutexGuard<()>) -> Self {
        self._send_guard = Some(send_guard);
        self
    }
}

#[derive(Debug, Clone)]
pub enum ExecutionStreamEvent {
    FirstChunk {
        ttft: Duration,
    },
    Chunk(UniversalStreamChunk),
    ToolCall {
        tool_call_id: ToolCallId,
        tool_name: String,
        arguments: ToolArguments,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        result: ToolResult,
    },
    Completed(SendResult),
}

impl Stream for ExecutionStreamHandle {
    type Item = Result<ExecutionStreamEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.event_stream).poll_next(cx)
    }
}

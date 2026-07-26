use std::sync::Arc;

use async_trait::async_trait;
use exoharness::{
    AgentHandle, AgentRecord, ConversationHandle, ConversationRecord, ExoHarness, Result,
    SandboxProvider, SessionId,
};
use lingua::Message;

use crate::{
    AgentConfig, AgentHarnessKind, BraintrustTracingConfig, ConversationConfig,
    ConversationModelConfig, ExecutionStreamHandle, SandboxScope, SendRequest, SendResult,
    TypeScriptHarnessConfig,
};

#[async_trait]
pub trait Harness: Send + Sync {
    fn exoharness_handle(&self) -> Arc<dyn ExoHarness>;

    async fn list_agents(&self) -> Result<Vec<AgentRecord>>;
    async fn get_agent(&self, agent_ref: &str) -> Result<Option<Arc<dyn HarnessAgent>>>;
    async fn create_agent(&self, request: CreateAgentRequest) -> Result<Arc<dyn HarnessAgent>>;
    async fn delete_agent(&self, agent_ref: &str) -> Result<bool>;
    async fn flush_tracing(&self) -> Result<()>;
}

#[async_trait]
pub trait HarnessAgent: Send + Sync {
    fn record(&self) -> &AgentRecord;
    fn exoharness_handle(&self) -> Arc<dyn AgentHandle>;

    async fn config(&self) -> Result<AgentConfig>;
    async fn put_config(&self, config: AgentConfig) -> Result<()>;
    async fn list_conversations(&self) -> Result<Vec<ConversationRecord>>;
    async fn get_conversation(
        &self,
        conversation_ref: &str,
    ) -> Result<Option<Arc<dyn HarnessConversation>>>;
    async fn create_conversation(
        &self,
        request: CreateConversationRequest,
    ) -> Result<Arc<dyn HarnessConversation>>;
    async fn delete_conversation(&self, conversation_ref: &str) -> Result<bool>;
}

#[async_trait]
pub trait HarnessConversation: Send + Sync {
    fn record(&self) -> &ConversationRecord;
    fn exoharness_handle(&self) -> Arc<dyn ConversationHandle>;

    async fn config(&self) -> Result<ConversationConfig>;
    async fn put_config(&self, config: ConversationConfig) -> Result<()>;
    async fn model_override(&self) -> Result<Option<ConversationModelConfig>>;
    async fn put_model_override(&self, config: Option<ConversationModelConfig>) -> Result<()>;
    async fn messages(&self) -> Result<Vec<Message>>;
    async fn close_session(&self, session_id: SessionId) -> Result<()>;
    async fn send(&self, request: SendRequest) -> Result<SendResult>;
    async fn send_stream(&self, request: SendRequest) -> Result<ExecutionStreamHandle>;
}

#[derive(Debug, Clone)]
pub struct CreateAgentRequest {
    pub slug: String,
    pub name: Option<String>,
    pub harness: AgentHarnessKind,
    pub typescript: Option<TypeScriptHarnessConfig>,
    pub enable_agent_tool_creation: bool,
    pub sandbox_image: Option<String>,
    pub sandbox_provider: SandboxProvider,
    pub sandbox_scope: Option<SandboxScope>,
    pub enable_networking: bool,
    pub model: String,
    pub max_output_tokens: Option<i64>,
    pub max_tool_round_trips: Option<u32>,
    pub braintrust: Option<BraintrustTracingConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct CreateConversationRequest {
    pub slug: Option<String>,
    pub name: Option<String>,
    pub sandbox_image: Option<String>,
    pub sandbox_provider: Option<SandboxProvider>,
    pub shell_program: Option<String>,
}

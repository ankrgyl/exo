use std::sync::Arc;

use crate::{AgentConfig, ConversationConfig, ExecutionStreamHandle, SendRequest, SendResult};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, AgentRecord, ConversationHandle, ConversationRecord, ExoHarness, NewAgentRequest,
    NewConversationRequest, Result, SessionId,
};
use lingua::Message;

use crate::conversation_wakeup::conversation_send_lock;
use crate::harness_helpers::{
    get_conversation_model_override, materialize_conversation_messages,
    put_conversation_model_override, resolve_agent_handle, resolve_conversation_handle,
};
use crate::harness_types::{
    CreateAgentRequest, CreateConversationRequest, Harness, HarnessAgent, HarnessConversation,
};

#[async_trait]
pub(crate) trait HarnessRuntime: Send + Sync + Clone + 'static {
    async fn get_agent_config(&self, agent: &dyn AgentHandle) -> Result<AgentConfig>;
    async fn put_agent_config(&self, agent: &dyn AgentHandle, config: AgentConfig) -> Result<()>;
    async fn get_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
    ) -> Result<ConversationConfig>;
    async fn put_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
        config: ConversationConfig,
    ) -> Result<()>;
    async fn send(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        request: SendRequest,
    ) -> Result<SendResult>;
    async fn send_stream(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        request: SendRequest,
    ) -> Result<ExecutionStreamHandle>;
    async fn flush_tracing(&self) -> Result<()>;
}

pub(crate) trait SharedHarnessBacked: Send + Sync {
    type Runtime: HarnessRuntime;

    fn shared_harness(&self) -> &SharedHarness<Self::Runtime>;
}

#[async_trait]
impl<T> Harness for T
where
    T: SharedHarnessBacked,
{
    fn exoharness_handle(&self) -> Arc<dyn ExoHarness> {
        self.shared_harness().exoharness_handle()
    }

    async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        self.shared_harness().list_agents().await
    }

    async fn get_agent(&self, agent_ref: &str) -> Result<Option<Arc<dyn HarnessAgent>>> {
        self.shared_harness().get_agent(agent_ref).await
    }

    async fn create_agent(&self, request: CreateAgentRequest) -> Result<Arc<dyn HarnessAgent>> {
        self.shared_harness().create_agent(request).await
    }

    async fn delete_agent(&self, agent_ref: &str) -> Result<bool> {
        self.shared_harness().delete_agent(agent_ref).await
    }

    async fn flush_tracing(&self) -> Result<()> {
        self.shared_harness().flush_tracing().await
    }
}

pub(crate) struct SharedHarness<R> {
    exoharness: Arc<dyn ExoHarness>,
    runtime: R,
}

impl<R> SharedHarness<R>
where
    R: HarnessRuntime,
{
    pub(crate) fn new(exoharness: Arc<dyn ExoHarness>, runtime: R) -> Self {
        Self {
            exoharness,
            runtime,
        }
    }

    pub(crate) fn exoharness_handle(&self) -> Arc<dyn ExoHarness> {
        Arc::clone(&self.exoharness)
    }

    pub(crate) async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        let agents = self.exoharness.list_agents().await?;
        Ok(agents
            .into_iter()
            .map(|agent| agent.record().clone())
            .collect())
    }

    pub(crate) async fn get_agent(&self, agent_ref: &str) -> Result<Option<Arc<dyn HarnessAgent>>> {
        let Some(agent) = resolve_agent_handle(self.exoharness.as_ref(), agent_ref).await? else {
            return Ok(None);
        };
        Ok(Some(self.wrap_agent(agent)))
    }

    pub(crate) async fn create_agent(
        &self,
        request: CreateAgentRequest,
    ) -> Result<Arc<dyn HarnessAgent>> {
        let name = request.name.clone().unwrap_or_else(|| request.slug.clone());
        let config = AgentConfig {
            instructions: Vec::new(),
            harness: request.harness,
            typescript: request.typescript,
            enable_agent_tool_creation: request.enable_agent_tool_creation,
            sandbox: crate::AgentSandboxConfig {
                image: request.sandbox_image,
                provider: request.sandbox_provider,
                mounts: Vec::new(),
                enable_networking: request.enable_networking,
                scope: request.sandbox_scope.unwrap_or_default(),
            },
            model: request.model,
            max_output_tokens: request.max_output_tokens,
            max_tool_round_trips: request.max_tool_round_trips,
            braintrust: request.braintrust,
        };
        let agent = self
            .exoharness
            .new_agent(NewAgentRequest {
                slug: request.slug,
                name,
            })
            .await?;
        self.runtime
            .put_agent_config(agent.as_ref(), config)
            .await?;
        Ok(self.wrap_agent(agent))
    }

    pub(crate) async fn delete_agent(&self, agent_ref: &str) -> Result<bool> {
        let Some(agent) = resolve_agent_handle(self.exoharness.as_ref(), agent_ref).await? else {
            return Ok(false);
        };
        let deleted = self.exoharness.delete_agent(&agent.record().id).await?;
        Ok(deleted)
    }

    pub(crate) async fn flush_tracing(&self) -> Result<()> {
        self.runtime.flush_tracing().await
    }

    fn wrap_agent(&self, agent: Arc<dyn AgentHandle>) -> Arc<dyn HarnessAgent> {
        Arc::new(SharedHarnessAgent {
            agent,
            runtime: self.runtime.clone(),
        })
    }
}

struct SharedHarnessAgent<R> {
    agent: Arc<dyn AgentHandle>,
    runtime: R,
}

#[async_trait]
impl<R> HarnessAgent for SharedHarnessAgent<R>
where
    R: HarnessRuntime,
{
    fn record(&self) -> &AgentRecord {
        self.agent.record()
    }

    fn exoharness_handle(&self) -> Arc<dyn AgentHandle> {
        Arc::clone(&self.agent)
    }

    async fn config(&self) -> Result<AgentConfig> {
        self.runtime.get_agent_config(self.agent.as_ref()).await
    }

    async fn put_config(&self, config: AgentConfig) -> Result<()> {
        self.runtime
            .put_agent_config(self.agent.as_ref(), config)
            .await
    }

    async fn list_conversations(&self) -> Result<Vec<ConversationRecord>> {
        let conversations = self
            .agent
            .list_conversations(exoharness::ListConversationsRequest::default())
            .await?
            .conversations;
        Ok(conversations
            .into_iter()
            .map(|conversation| conversation.record().clone())
            .collect())
    }

    async fn get_conversation(
        &self,
        conversation_ref: &str,
    ) -> Result<Option<Arc<dyn HarnessConversation>>> {
        let Some(conversation) =
            resolve_conversation_handle(self.agent.as_ref(), conversation_ref).await?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(SharedHarnessConversation {
            agent: Arc::clone(&self.agent),
            conversation,
            runtime: self.runtime.clone(),
        })))
    }

    async fn create_conversation(
        &self,
        request: CreateConversationRequest,
    ) -> Result<Arc<dyn HarnessConversation>> {
        let agent_config = self.config().await?;
        let conversation = self
            .agent
            .new_conversation(NewConversationRequest {
                slug: request.slug,
                name: request.name,
            })
            .await?;
        let default_conversation_config = ConversationConfig::default();
        let conversation_config = ConversationConfig {
            sandbox_image: request.sandbox_image.or(agent_config.sandbox.image),
            sandbox_provider: Some(
                request
                    .sandbox_provider
                    .unwrap_or(agent_config.sandbox.provider),
            ),
            shell_program: request
                .shell_program
                .or(default_conversation_config.shell_program),
            mounts: default_conversation_config.mounts,
            durable_file_systems: default_conversation_config.durable_file_systems,
            sandbox_scope: default_conversation_config.sandbox_scope,
        };
        self.runtime
            .put_conversation_config(conversation.as_ref(), conversation_config)
            .await?;
        Ok(Arc::new(SharedHarnessConversation {
            agent: Arc::clone(&self.agent),
            conversation,
            runtime: self.runtime.clone(),
        }))
    }

    async fn delete_conversation(&self, conversation_ref: &str) -> Result<bool> {
        let Some(conversation) =
            resolve_conversation_handle(self.agent.as_ref(), conversation_ref).await?
        else {
            return Ok(false);
        };
        let deleted = self
            .agent
            .delete_conversation(&conversation.record().id)
            .await?;
        Ok(deleted)
    }
}

struct SharedHarnessConversation<R> {
    agent: Arc<dyn AgentHandle>,
    conversation: Arc<dyn ConversationHandle>,
    runtime: R,
}

#[async_trait]
impl<R> HarnessConversation for SharedHarnessConversation<R>
where
    R: HarnessRuntime,
{
    fn record(&self) -> &ConversationRecord {
        self.conversation.record()
    }

    fn exoharness_handle(&self) -> Arc<dyn ConversationHandle> {
        Arc::clone(&self.conversation)
    }

    async fn config(&self) -> Result<ConversationConfig> {
        self.runtime
            .get_conversation_config(self.conversation.as_ref())
            .await
    }

    async fn put_config(&self, config: ConversationConfig) -> Result<()> {
        self.runtime
            .put_conversation_config(self.conversation.as_ref(), config)
            .await
    }

    async fn model_override(&self) -> Result<Option<crate::ConversationModelConfig>> {
        get_conversation_model_override(self.conversation.as_ref()).await
    }

    async fn put_model_override(
        &self,
        config: Option<crate::ConversationModelConfig>,
    ) -> Result<()> {
        put_conversation_model_override(self.conversation.as_ref(), config).await
    }

    async fn messages(&self) -> Result<Vec<Message>> {
        materialize_conversation_messages(self.conversation.as_ref()).await
    }

    async fn close_session(&self, session_id: SessionId) -> Result<()> {
        self.conversation.end_session(session_id).await
    }

    async fn send(&self, request: SendRequest) -> Result<SendResult> {
        let send_lock = conversation_send_lock(&self.conversation.record().id.to_string());
        let _guard = send_lock.lock().await;
        self.runtime
            .send(
                Arc::clone(&self.agent),
                Arc::clone(&self.conversation),
                request,
            )
            .await
    }

    async fn send_stream(&self, request: SendRequest) -> Result<ExecutionStreamHandle> {
        let send_lock = conversation_send_lock(&self.conversation.record().id.to_string());
        let send_guard = send_lock.lock_owned().await;
        let stream = self
            .runtime
            .send_stream(
                Arc::clone(&self.agent),
                Arc::clone(&self.conversation),
                request,
            )
            .await?;
        Ok(stream.with_send_guard(send_guard))
    }
}

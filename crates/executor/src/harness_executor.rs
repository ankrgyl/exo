use std::sync::Arc;

use async_trait::async_trait;
use exoharness::{AgentHandle, BeginTurnRequest, ConversationHandle, Result, TurnHandle};
use tokio::sync::mpsc;

use crate::braintrust::{BraintrustRuntimeConfig, BraintrustTracer};
use crate::execution_tracing::{ExecutionTracer, TurnExecutionTrace};
use crate::harness_config::{
    load_agent_config, load_conversation_config, store_agent_config, store_conversation_config,
};
use crate::harness_facade::HarnessRuntime;
use crate::harness_helpers::get_conversation_model_override;
use crate::shared::{execute_prepared_turn, spawn_prepared_turn_stream};
use crate::{
    AgentConfig, ConversationConfig, ConversationModelConfig, ExecutionStreamEvent,
    ExecutionStreamHandle, SendRequest, SendResult,
};

#[derive(Clone, Copy)]
pub(crate) enum ExecutorStreamMode<'a> {
    Disabled,
    Enabled(&'a mpsc::UnboundedSender<Result<ExecutionStreamEvent>>),
}

#[async_trait]
pub(crate) trait HarnessExecutor: Send + Sync + Clone + 'static {
    type Prepared: Send + Sync + 'static;

    async fn prepare_conversation(
        &self,
        _agent: &dyn AgentHandle,
        _conversation: &dyn ConversationHandle,
        _agent_config: &AgentConfig,
        _conversation_config: &ConversationConfig,
    ) -> Result<()> {
        Ok(())
    }

    fn prepare_request(&self, request: &SendRequest) -> Result<Self::Prepared>;

    async fn execute_turn(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        prepared: &Self::Prepared,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()>;
}

pub(crate) struct ExecutorHarnessRuntime<E> {
    executor: E,
    tracer: Arc<dyn ExecutionTracer>,
}

impl<E> ExecutorHarnessRuntime<E> {
    pub(crate) fn new(executor: E, runtime_config: Option<BraintrustRuntimeConfig>) -> Self {
        Self {
            executor,
            tracer: Arc::new(BraintrustTracer::new(runtime_config)),
        }
    }
}

impl<E> Clone for ExecutorHarnessRuntime<E>
where
    E: Clone,
{
    fn clone(&self) -> Self {
        Self {
            executor: self.executor.clone(),
            tracer: Arc::clone(&self.tracer),
        }
    }
}

fn apply_conversation_model_override(
    agent_config: &mut AgentConfig,
    model_override: Option<ConversationModelConfig>,
) {
    let Some(model_override) = model_override else {
        return;
    };
    agent_config.model = model_override.model;
    agent_config.max_output_tokens = model_override.max_output_tokens;
}

#[async_trait]
impl<E> HarnessRuntime for ExecutorHarnessRuntime<E>
where
    E: HarnessExecutor,
{
    async fn get_agent_config(&self, agent: &dyn AgentHandle) -> Result<AgentConfig> {
        // Read the latest config from the agent's artifact store every turn, so an
        // external `exo agent update` (or the agent rewriting its own config) takes
        // effect on the next turn without restarting the runner. No cache on
        // purpose: the config is a tiny artifact and a turn is dominated by the
        // model call, so a fresh read costs less than keeping a cache correct
        // across outside writes (the stale-cache bug this replaces).
        load_agent_config(agent).await
    }

    async fn put_agent_config(&self, agent: &dyn AgentHandle, config: AgentConfig) -> Result<()> {
        store_agent_config(agent, &config).await
    }

    async fn get_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
    ) -> Result<ConversationConfig> {
        // Read fresh every turn, for the same reasons as get_agent_config.
        load_conversation_config(conversation).await
    }

    async fn put_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
        config: ConversationConfig,
    ) -> Result<()> {
        store_conversation_config(conversation, &config).await
    }

    async fn send(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        request: SendRequest,
    ) -> Result<SendResult> {
        let (mut agent_config, conversation_config, model_override) = tokio::try_join!(
            self.get_agent_config(agent.as_ref()),
            self.get_conversation_config(conversation.as_ref()),
            get_conversation_model_override(conversation.as_ref()),
        )?;
        apply_conversation_model_override(&mut agent_config, model_override);
        self.executor
            .prepare_conversation(
                agent.as_ref(),
                conversation.as_ref(),
                &agent_config,
                &conversation_config,
            )
            .await?;
        let prepared = self.executor.prepare_request(&request)?;
        let turn = conversation
            .begin_turn(BeginTurnRequest {
                session_id: request.session_id,
                input: request.input,
            })
            .await?;
        let trace_agent_config = agent_config.clone();
        let executor = self.executor.clone();
        let run_turn = Arc::clone(&turn);
        let run_conversation = Arc::clone(&conversation);
        let run_agent = Arc::clone(&agent);

        execute_prepared_turn(
            self.tracer.as_ref(),
            agent.as_ref(),
            conversation.as_ref(),
            turn.as_ref(),
            &trace_agent_config,
            |turn_trace| {
                Box::pin(async move {
                    executor
                        .execute_turn(
                            run_agent.as_ref(),
                            run_conversation.as_ref(),
                            Arc::clone(&run_turn),
                            &agent_config,
                            &conversation_config,
                            &prepared,
                            ExecutorStreamMode::Disabled,
                            turn_trace,
                        )
                        .await
                })
            },
        )
        .await
    }

    async fn send_stream(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        request: SendRequest,
    ) -> Result<ExecutionStreamHandle> {
        let (mut agent_config, conversation_config, model_override) = tokio::try_join!(
            self.get_agent_config(agent.as_ref()),
            self.get_conversation_config(conversation.as_ref()),
            get_conversation_model_override(conversation.as_ref()),
        )?;
        apply_conversation_model_override(&mut agent_config, model_override);
        self.executor
            .prepare_conversation(
                agent.as_ref(),
                conversation.as_ref(),
                &agent_config,
                &conversation_config,
            )
            .await?;
        let prepared = self.executor.prepare_request(&request)?;
        let turn = conversation
            .begin_turn(BeginTurnRequest {
                session_id: request.session_id,
                input: request.input,
            })
            .await?;
        let trace_agent_config = agent_config.clone();
        let executor = self.executor.clone();
        let run_turn = Arc::clone(&turn);
        let run_conversation = Arc::clone(&conversation);
        let run_agent = Arc::clone(&agent);

        Ok(spawn_prepared_turn_stream(
            Arc::clone(&self.tracer),
            agent,
            conversation,
            turn,
            trace_agent_config,
            move |turn_trace, event_tx| {
                Box::pin(async move {
                    executor
                        .execute_turn(
                            run_agent.as_ref(),
                            run_conversation.as_ref(),
                            Arc::clone(&run_turn),
                            &agent_config,
                            &conversation_config,
                            &prepared,
                            ExecutorStreamMode::Enabled(event_tx),
                            turn_trace,
                        )
                        .await
                })
            },
        ))
    }

    async fn flush_tracing(&self) -> Result<()> {
        self.tracer.flush().await
    }
}

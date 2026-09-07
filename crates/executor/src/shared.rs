use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use anyhow::Error;
use exoharness::{
    AgentHandle, AgentId, ConversationHandle, ConversationId, EventId, Result, TurnHandle,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::execution_tracing::{ExecutionTracer, TurnExecutionTrace};
use crate::{AgentConfig, ExecutionStreamEvent, ExecutionStreamHandle, SendResult};

pub(crate) type TurnFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

pub(crate) fn cache_insert<K, V>(cache: &RwLock<HashMap<K, V>>, key: K, value: V, name: &str)
where
    K: Eq + Hash,
{
    cache.write().expect(name).insert(key, value);
}

pub(crate) async fn get_or_load_cached<K, V, Load, LoadFuture>(
    cache: &RwLock<HashMap<K, V>>,
    key: K,
    name: &str,
    load: Load,
) -> Result<V>
where
    K: Eq + Hash + Clone,
    V: Clone,
    Load: FnOnce() -> LoadFuture,
    LoadFuture: Future<Output = Result<V>>,
{
    {
        let cache = cache.read().expect(name);
        if let Some(value) = cache.get(&key) {
            return Ok(value.clone());
        }
    }

    let value = load().await?;
    cache_insert(cache, key, value.clone(), name);
    Ok(value)
}

pub(crate) async fn execute_prepared_turn<Run>(
    tracer: &dyn ExecutionTracer,
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    turn: &dyn TurnHandle,
    agent_config: &AgentConfig,
    run: Run,
) -> Result<SendResult>
where
    Run: for<'a> FnOnce(Option<&'a dyn TurnExecutionTrace>) -> TurnFuture<'a>,
{
    let session_id = turn.record().session_id;
    let turn_id = turn.record().id;
    let turn_trace = tracer
        .start_turn(
            agent_config.braintrust.as_ref(),
            agent.record(),
            conversation.record(),
            agent_config,
            session_id,
            turn_id,
            false,
        )
        .await;
    let latest_event_id = finalize_turn(turn, run(turn_trace.as_deref()).await).await;

    finish_turn_trace(turn_trace, &latest_event_id).await;

    Ok(SendResult {
        session_id,
        turn_id,
        latest_event_id: latest_event_id?,
    })
}

pub(crate) fn spawn_prepared_turn_stream<Run>(
    tracer: Arc<dyn ExecutionTracer>,
    agent: Arc<dyn AgentHandle>,
    conversation: Arc<dyn ConversationHandle>,
    turn: Arc<dyn TurnHandle>,
    agent_config: AgentConfig,
    run: Run,
) -> ExecutionStreamHandle
where
    Run: for<'a> FnOnce(
            Option<&'a dyn TurnExecutionTrace>,
            &'a mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
        ) -> TurnFuture<'a>
        + Send
        + 'static,
{
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();

    let task = tokio::spawn(async move {
        let session_id = turn.record().session_id;
        let turn_id = turn.record().id;
        let turn_trace = tokio::select! {
            turn_trace = tracer.start_turn(
                agent_config.braintrust.as_ref(),
                agent.record(),
                conversation.record(),
                &agent_config,
                session_id,
                turn_id,
                true,
            ) => turn_trace,
            () = task_cancellation.cancelled() => {
                complete_stream_turn(
                    turn.as_ref(),
                    session_id,
                    turn_id,
                    None,
                    Err(Error::msg("execution cancelled")),
                    &event_tx,
                ).await;
                return;
            }
        };
        let run_result = tokio::select! {
            result = run(turn_trace.as_deref(), &event_tx) => result,
            () = task_cancellation.cancelled() => Err(Error::msg("execution cancelled")),
        };
        complete_stream_turn(
            turn.as_ref(),
            session_id,
            turn_id,
            turn_trace,
            run_result,
            &event_tx,
        )
        .await;
    });

    ExecutionStreamHandle::with_task(UnboundedReceiverStream::new(event_rx), cancellation, task)
}

async fn complete_stream_turn(
    turn: &dyn TurnHandle,
    session_id: exoharness::SessionId,
    turn_id: exoharness::TurnId,
    turn_trace: Option<Box<dyn TurnExecutionTrace>>,
    run_result: Result<()>,
    event_tx: &mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
) {
    let send_result = finalize_turn(turn, run_result)
        .await
        .map(|latest_event_id| SendResult {
            session_id,
            turn_id,
            latest_event_id,
        });

    if let Some(turn_trace) = turn_trace {
        match &send_result {
            Ok(result) => {
                turn_trace
                    .finish_success(Some(result.latest_event_id))
                    .await
            }
            Err(error) => turn_trace.finish_error(error).await,
        }
    }

    if let Err(error) = &send_result {
        try_send_stream_error(event_tx, error);
    } else if let Ok(result) = &send_result {
        try_send_stream_event(event_tx, ExecutionStreamEvent::Completed(result.clone()));
    }
}

async fn finish_turn_trace(
    turn_trace: Option<Box<dyn TurnExecutionTrace>>,
    latest_event_id: &Result<EventId>,
) {
    if let Some(turn_trace) = turn_trace {
        match latest_event_id {
            Ok(event_id) => turn_trace.finish_success(Some(*event_id)).await,
            Err(error) => turn_trace.finish_error(error).await,
        }
    }
}

pub(crate) async fn finalize_turn(turn: &dyn TurnHandle, result: Result<()>) -> Result<EventId> {
    match result {
        Ok(()) => turn.finish().await,
        Err(error) => match turn.finish().await {
            Ok(_) => Err(error),
            Err(finish_error) => {
                Err(error.context(format!("also failed to finish turn: {finish_error}")))
            }
        },
    }
}

pub(crate) fn try_send_stream_event(
    event_tx: &mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
    event: ExecutionStreamEvent,
) {
    if event_tx.send(Ok(event)).is_err() {}
}

pub(crate) fn try_send_stream_error(
    event_tx: &mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
    error: &Error,
) {
    if event_tx.send(Err(Error::msg(error.to_string()))).is_err() {}
}

pub(crate) const AGENT_CONFIG_CACHE_NAME: &str = "agent config cache poisoned";
pub(crate) const CONVERSATION_CONFIG_CACHE_NAME: &str = "conversation config cache poisoned";
pub(crate) const HISTORY_CACHE_NAME: &str = "history cache poisoned";

pub(crate) fn cache_agent_config(
    cache: &RwLock<HashMap<AgentId, AgentConfig>>,
    agent_id: AgentId,
    config: AgentConfig,
) {
    cache_insert(cache, agent_id, config, AGENT_CONFIG_CACHE_NAME);
}

pub(crate) fn cache_conversation_config(
    cache: &RwLock<HashMap<ConversationId, crate::ConversationConfig>>,
    conversation_id: ConversationId,
    config: crate::ConversationConfig,
) {
    cache_insert(
        cache,
        conversation_id,
        config,
        CONVERSATION_CONFIG_CACHE_NAME,
    );
}

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Error;
use exoharness::{AgentHandle, ConversationHandle, EventId, Result, TurnHandle};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::execution_tracing::{ExecutionTracer, TurnExecutionTrace};
use crate::{AgentConfig, ExecutionStreamEvent, ExecutionStreamHandle, SendResult};

pub(crate) type TurnFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

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

    tokio::spawn(async move {
        let session_id = turn.record().session_id;
        let turn_id = turn.record().id;
        let turn_trace = tracer
            .start_turn(
                agent_config.braintrust.as_ref(),
                agent.record(),
                conversation.record(),
                &agent_config,
                session_id,
                turn_id,
                true,
            )
            .await;
        let send_result = finalize_turn(turn.as_ref(), run(turn_trace.as_deref(), &event_tx).await)
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
            try_send_stream_error(&event_tx, error);
        } else if let Ok(result) = &send_result {
            try_send_stream_event(&event_tx, ExecutionStreamEvent::Completed(result.clone()));
        }
    });

    ExecutionStreamHandle::new(UnboundedReceiverStream::new(event_rx))
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

pub(crate) const HISTORY_CACHE_NAME: &str = "history cache poisoned";

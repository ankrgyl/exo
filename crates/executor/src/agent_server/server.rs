use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use exoharness::{AgentId, ConversationId, Uuid7};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::Mutex;
use tokio::task::{JoinError, JoinSet};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::{
    AgentHarnessKind, CreateConversationRequest, ExecutionStreamEvent, ExecutionStreamHandle,
    Harness, HarnessConversation, SendRequest,
};

use super::AGENT_SERVER_TRACING_TARGET;
use super::protocol::{
    AgentGetParams, AgentGetResult, AgentListResult, ConversationCreateParams,
    ConversationCreateResult, ConversationGetParams, ConversationGetResult, ConversationListParams,
    ConversationListResult, EmptyParams, InitializeParams, InitializeResult, OperationError,
    PROTOCOL_VERSION, RequestId, ResponseResult, RpcError, RpcFailure, RpcNotification, RpcRequest,
    RpcResponse, RpcSuccess, TurnCancelParams, TurnCompletedParams, TurnEvent, TurnEventParams,
    TurnFailedParams, TurnStartParams, TurnStartResult, TurnStartedParams,
};

type SharedWriter<W> = Arc<Mutex<BufWriter<W>>>;
type ActiveConversations = Arc<Mutex<HashSet<ConversationId>>>;
pub(super) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const CLIENT_EXECUTOR_FAILURE_MESSAGE: &str = "executor turn failed; see server logs";

pub(crate) struct AgentServer {
    harness: Arc<dyn Harness>,
    harness_kind: AgentHarnessKind,
    initialized: bool,
    active_conversations: ActiveConversations,
}

impl AgentServer {
    pub(crate) fn new(harness: Arc<dyn Harness>, harness_kind: AgentHarnessKind) -> Self {
        Self {
            harness,
            harness_kind,
            initialized: false,
            active_conversations: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub(crate) async fn serve<R, W>(mut self, reader: R, writer: W) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        tracing::info!(target: AGENT_SERVER_TRACING_TARGET, "agent server started");
        let mut frames = FrameReader::new(reader);
        let writer = Arc::new(Mutex::new(BufWriter::new(writer)));
        let mut operations = JoinSet::new();
        let shutdown = CancellationToken::new();
        let mut server_error = None;

        loop {
            match next_event(&mut frames, &mut operations).await {
                ServeEvent::Input(Ok(Some(frame))) => {
                    let dispatch = match frame {
                        InputFrame::Bytes(frame) => self.dispatch_frame(&frame).await,
                        InputFrame::TooLarge => {
                            Dispatch::failure(RequestId::Null(()), RpcError::parse())
                        }
                    };
                    if let Some(response) = dispatch.response
                        && let Err(error) = write_frame(&writer, &response).await
                    {
                        if let Some(operation) = dispatch.operation {
                            operation.release().await;
                        }
                        server_error = Some(error);
                        break;
                    }
                    if let Some(operation) = dispatch.operation {
                        operations
                            .spawn(operation.run(Arc::clone(&writer), shutdown.child_token()));
                    }
                }
                ServeEvent::Input(Ok(None)) => break,
                ServeEvent::Input(Err(error)) => {
                    server_error = Some(error.into());
                    break;
                }
                ServeEvent::Operation(result) => {
                    if let Err(error) = operation_result(result) {
                        server_error = Some(error);
                        break;
                    }
                }
            }
        }

        shutdown.cancel();
        while let Some(result) = operations.join_next().await {
            if let Err(error) = operation_result(result)
                && server_error.is_none()
            {
                server_error = Some(error);
            }
        }
        writer.lock().await.flush().await?;
        tracing::info!(target: AGENT_SERVER_TRACING_TARGET, "agent server stopped");

        match server_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn dispatch_frame(&mut self, frame: &[u8]) -> Dispatch {
        let raw = match serde_json::from_slice::<Box<RawValue>>(frame) {
            Ok(raw) => raw,
            Err(_) => return Dispatch::failure(RequestId::Null(()), RpcError::parse()),
        };
        let request = match serde_json::from_str::<RpcRequest>(raw.get()) {
            Ok(request) => request,
            Err(_) => {
                return Dispatch::failure(RequestId::Null(()), RpcError::invalid_request());
            }
        };
        if request.jsonrpc != "2.0" {
            return Dispatch::failure(request.id, RpcError::invalid_request());
        }

        tracing::debug!(
            target: AGENT_SERVER_TRACING_TARGET,
            method = %request.method,
            "agent server request"
        );
        let id = request.id.clone();
        match self.dispatch_request(request).await {
            Ok((result, operation)) => Dispatch {
                response: (!id.is_missing())
                    .then(|| RpcResponse::Success(RpcSuccess::new(id, result))),
                operation,
            },
            Err(_) if id.is_missing() => Dispatch::empty(),
            Err(error) => Dispatch::failure(id, error),
        }
    }

    async fn dispatch_request(
        &mut self,
        request: RpcRequest,
    ) -> std::result::Result<(ResponseResult, Option<TurnOperation>), RpcError> {
        let params = request.params.as_deref();
        match request.method.as_str() {
            "initialize" => self.initialize(params),
            "agent/list" => {
                self.require_initialized()?;
                let _: EmptyParams = parse_params(params)?;
                let agents = self
                    .harness
                    .list_agents()
                    .await
                    .map_err(RpcError::executor)?;
                Ok((ResponseResult::AgentList(AgentListResult { agents }), None))
            }
            "agent/get" => {
                self.require_initialized()?;
                let params: AgentGetParams = parse_params(params)?;
                let agent = self
                    .harness
                    .get_agent(&params.agent_ref)
                    .await
                    .map_err(RpcError::executor)?
                    .ok_or_else(|| agent_not_found(&params.agent_ref))?;
                Ok((
                    ResponseResult::AgentGet(AgentGetResult {
                        agent: agent.record().clone(),
                    }),
                    None,
                ))
            }
            "conversation/list" => {
                self.require_initialized()?;
                let params: ConversationListParams = parse_params(params)?;
                let agent = self.resolve_agent(&params.agent_ref).await?;
                let conversations = agent
                    .list_conversations()
                    .await
                    .map_err(RpcError::executor)?;
                Ok((
                    ResponseResult::ConversationList(ConversationListResult { conversations }),
                    None,
                ))
            }
            "conversation/get" => {
                self.require_initialized()?;
                let params: ConversationGetParams = parse_params(params)?;
                let agent = self.resolve_agent(&params.agent_ref).await?;
                let conversation = agent
                    .get_conversation(&params.conversation_ref)
                    .await
                    .map_err(RpcError::executor)?
                    .ok_or_else(|| conversation_not_found(&params.conversation_ref))?;
                Ok((
                    ResponseResult::ConversationGet(ConversationGetResult {
                        conversation: conversation.record().clone(),
                    }),
                    None,
                ))
            }
            "conversation/create" => {
                self.require_initialized()?;
                let params: ConversationCreateParams = parse_params(params)?;
                let agent = self.resolve_agent(&params.agent_ref).await?;
                let conversation = agent
                    .create_conversation(CreateConversationRequest {
                        slug: params.slug,
                        name: params.name,
                        sandbox_image: params.sandbox_image,
                        sandbox_provider: params.sandbox_provider,
                        shell_program: params.shell_program,
                    })
                    .await
                    .map_err(RpcError::executor)?;
                Ok((
                    ResponseResult::ConversationCreate(ConversationCreateResult {
                        conversation: conversation.record().clone(),
                    }),
                    None,
                ))
            }
            "turn/start" => {
                self.require_initialized()?;
                let params: TurnStartParams = parse_params(params)?;
                self.start_turn(params).await
            }
            "turn/cancel" => {
                self.require_initialized()?;
                let params: TurnCancelParams = parse_params(params)?;
                tracing::debug!(
                    target: AGENT_SERVER_TRACING_TARGET,
                    operation_id = %params.operation_id,
                    "unsupported turn cancellation requested"
                );
                Err(RpcError::cancellation_unsupported())
            }
            _ => Err(RpcError::method_not_found()),
        }
    }

    fn initialize(
        &mut self,
        params: Option<&RawValue>,
    ) -> std::result::Result<(ResponseResult, Option<TurnOperation>), RpcError> {
        let params: InitializeParams = parse_params(params)?;
        if params.protocol_version != PROTOCOL_VERSION {
            return Err(RpcError::unsupported_protocol_version());
        }
        if let Some(client) = params.client {
            tracing::info!(
                target: AGENT_SERVER_TRACING_TARGET,
                client_name = %client.name,
                client_version = %client.version,
                "agent server initialized"
            );
        } else {
            tracing::info!(target: AGENT_SERVER_TRACING_TARGET, "agent server initialized");
        }
        self.initialized = true;
        Ok((
            ResponseResult::Initialize(InitializeResult::default()),
            None,
        ))
    }

    fn require_initialized(&self) -> std::result::Result<(), RpcError> {
        if self.initialized {
            Ok(())
        } else {
            Err(RpcError::not_initialized())
        }
    }

    async fn resolve_agent(
        &self,
        agent_ref: &str,
    ) -> std::result::Result<Arc<dyn crate::HarnessAgent>, RpcError> {
        self.harness
            .get_agent(agent_ref)
            .await
            .map_err(RpcError::executor)?
            .ok_or_else(|| agent_not_found(agent_ref))
    }

    async fn start_turn(
        &self,
        params: TurnStartParams,
    ) -> std::result::Result<(ResponseResult, Option<TurnOperation>), RpcError> {
        let agent = self.resolve_agent(&params.agent_ref).await?;
        let agent_harness = agent.config().await.map_err(RpcError::executor)?.harness;
        if agent_harness != self.harness_kind {
            return Err(RpcError::incompatible_harness(format!(
                "agent uses the {} harness but the agent server uses {}; restart with --harness {}",
                harness_kind_name(agent_harness),
                harness_kind_name(self.harness_kind),
                harness_kind_name(agent_harness),
            )));
        }
        let conversation = agent
            .get_conversation(&params.conversation_ref)
            .await
            .map_err(RpcError::executor)?
            .ok_or_else(|| conversation_not_found(&params.conversation_ref))?;
        let agent_id = agent.record().id;
        let conversation_id = conversation.record().id;
        let operation_id = Uuid7::now();

        let mut active = self.active_conversations.lock().await;
        if !active.insert(conversation_id) {
            return Err(RpcError::turn_already_active());
        }
        drop(active);

        tracing::info!(
            target: AGENT_SERVER_TRACING_TARGET,
            %operation_id,
            %agent_id,
            %conversation_id,
            "agent server turn accepted"
        );
        Ok((
            ResponseResult::TurnStart(TurnStartResult {
                operation_id,
                state: "accepted",
                agent_id,
                conversation_id,
            }),
            Some(TurnOperation {
                operation_id,
                agent_id,
                conversation_id,
                conversation,
                request: SendRequest {
                    input: params.input,
                    session_id: params.session_id,
                },
                active_conversations: Arc::clone(&self.active_conversations),
            }),
        ))
    }
}

struct Dispatch {
    response: Option<RpcResponse>,
    operation: Option<TurnOperation>,
}

impl Dispatch {
    fn failure(id: RequestId, error: RpcError) -> Self {
        Self {
            response: Some(RpcResponse::Failure(RpcFailure::new(id, error))),
            operation: None,
        }
    }

    fn empty() -> Self {
        Self {
            response: None,
            operation: None,
        }
    }
}

struct TurnOperation {
    operation_id: Uuid7,
    agent_id: AgentId,
    conversation_id: ConversationId,
    conversation: Arc<dyn HarnessConversation>,
    request: SendRequest,
    active_conversations: ActiveConversations,
}

impl TurnOperation {
    async fn run<W>(self, writer: SharedWriter<W>, shutdown: CancellationToken) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let result = self.run_stream(&writer, &shutdown).await;
        self.remove_active().await;
        result
    }

    async fn run_stream<W>(
        &self,
        writer: &SharedWriter<W>,
        shutdown: &CancellationToken,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let mut stream = match tokio::select! {
            biased;
            result = self.conversation.send_stream(self.request.clone()) => result,
            () = shutdown.cancelled() => {
                self.emit_failed(writer, 0, "cancelled", "agent server input closed").await?;
                return Ok(());
            },
        } {
            Ok(stream) => stream,
            Err(error) => {
                self.emit_executor_failed(writer, 0, error).await?;
                return Ok(());
            }
        };
        write_stream_frame(
            writer,
            &RpcNotification::new(
                "turn/started",
                TurnStartedParams {
                    operation_id: self.operation_id,
                    sequence: 0,
                    agent_id: self.agent_id,
                    conversation_id: self.conversation_id,
                    session_id: None,
                    turn_id: None,
                },
            ),
            &mut stream,
        )
        .await?;

        let mut sequence = 0;
        loop {
            let event = tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    let (cancellation, completed) = cancel_for_shutdown(&mut stream).await;
                    if let Some(result) = completed {
                        self.emit_completed(writer, sequence + 1, result).await?;
                        return Ok(());
                    }
                    if let Err(error) = cancellation {
                        self.emit_executor_failed(writer, sequence + 1, error).await?;
                        return Ok(());
                    }
                    self.emit_failed(
                        writer,
                        sequence + 1,
                        "cancelled",
                        "agent server input closed",
                    )
                    .await?;
                    tracing::info!(
                        target: AGENT_SERVER_TRACING_TARGET,
                        operation_id = %self.operation_id,
                        "agent server turn cancelled during shutdown"
                    );
                    return Ok(());
                },
                event = stream.next() => event,
            };
            let Some(event) = event else {
                let message = match stream.join().await {
                    Ok(()) => anyhow!("executor stream ended without a terminal event"),
                    Err(error) => error,
                };
                self.emit_executor_failed(writer, sequence + 1, message)
                    .await?;
                return Ok(());
            };
            sequence += 1;
            match event {
                Ok(ExecutionStreamEvent::Completed(result)) => {
                    if let Err(error) = stream.join().await {
                        tracing::warn!(
                            target: AGENT_SERVER_TRACING_TARGET,
                            operation_id = %self.operation_id,
                            %error,
                            "executor task failed after reporting completion"
                        );
                    }
                    self.emit_completed(writer, sequence, result).await?;
                    tracing::info!(
                        target: AGENT_SERVER_TRACING_TARGET,
                        operation_id = %self.operation_id,
                        "agent server turn completed"
                    );
                    return Ok(());
                }
                Ok(event) => {
                    let event = wire_event(event)?;
                    write_stream_frame(
                        writer,
                        &RpcNotification::new(
                            "turn/event",
                            TurnEventParams {
                                operation_id: self.operation_id,
                                sequence,
                                event,
                            },
                        ),
                        &mut stream,
                    )
                    .await?;
                }
                Err(error) => {
                    let message = match stream.join().await {
                        Ok(()) => error,
                        Err(join_error) => error
                            .context(format!("also failed to join executor task: {join_error}")),
                    };
                    self.emit_executor_failed(writer, sequence, message).await?;
                    return Ok(());
                }
            }
        }
    }

    async fn emit_completed<W>(
        &self,
        writer: &SharedWriter<W>,
        sequence: u64,
        result: crate::SendResult,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        self.remove_active().await;
        write_frame(
            writer,
            &RpcNotification::new(
                "turn/completed",
                TurnCompletedParams {
                    operation_id: self.operation_id,
                    sequence,
                    agent_id: self.agent_id,
                    conversation_id: self.conversation_id,
                    session_id: result.session_id,
                    turn_id: result.turn_id,
                    latest_event_id: result.latest_event_id,
                },
            ),
        )
        .await
    }

    async fn emit_executor_failed<W>(
        &self,
        writer: &SharedWriter<W>,
        sequence: u64,
        error: anyhow::Error,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        tracing::error!(
            target: AGENT_SERVER_TRACING_TARGET,
            operation_id = %self.operation_id,
            error = ?error,
            "agent server executor failure"
        );
        self.emit_failed(
            writer,
            sequence,
            "executor_error",
            CLIENT_EXECUTOR_FAILURE_MESSAGE,
        )
        .await
    }

    async fn emit_failed<W>(
        &self,
        writer: &SharedWriter<W>,
        sequence: u64,
        kind: &'static str,
        message: impl Into<String>,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        self.remove_active().await;
        write_frame(
            writer,
            &RpcNotification::new(
                "turn/failed",
                TurnFailedParams {
                    operation_id: self.operation_id,
                    sequence,
                    agent_id: self.agent_id,
                    conversation_id: self.conversation_id,
                    session_id: None,
                    turn_id: None,
                    latest_event_id: None,
                    error: OperationError {
                        kind,
                        message: message.into(),
                    },
                },
            ),
        )
        .await?;
        tracing::info!(
            target: AGENT_SERVER_TRACING_TARGET,
            operation_id = %self.operation_id,
            "agent server turn failed"
        );
        Ok(())
    }

    async fn release(self) {
        self.remove_active().await;
    }

    async fn remove_active(&self) {
        self.active_conversations
            .lock()
            .await
            .remove(&self.conversation_id);
    }
}

pub(super) async fn cancel_for_shutdown(
    stream: &mut ExecutionStreamHandle,
) -> (Result<()>, Option<crate::SendResult>) {
    let finite = stream.supports_cancellation();
    let cancellation = stream.cancel().await;
    if !finite {
        return (cancellation, None);
    }
    while let Some(event) = stream.next().await {
        if let Ok(ExecutionStreamEvent::Completed(result)) = event {
            return (cancellation, Some(result));
        }
    }
    (cancellation, None)
}

fn wire_event(event: ExecutionStreamEvent) -> Result<TurnEvent> {
    match event {
        ExecutionStreamEvent::FirstChunk { ttft } => Ok(TurnEvent::FirstChunk {
            ttft_ms: u64::try_from(ttft.as_millis()).unwrap_or(u64::MAX),
        }),
        ExecutionStreamEvent::Chunk(chunk) => Ok(TurnEvent::Chunk { chunk }),
        ExecutionStreamEvent::ToolCall {
            tool_call_id,
            tool_name,
            arguments,
        } => Ok(TurnEvent::ToolCall {
            tool_call_id,
            tool_name,
            arguments,
        }),
        ExecutionStreamEvent::ToolResult {
            tool_call_id,
            result,
        } => Ok(TurnEvent::ToolResult {
            tool_call_id,
            result,
        }),
        ExecutionStreamEvent::Completed(_) => {
            Err(anyhow!("completed event mapped as non-terminal"))
        }
    }
}

fn parse_params<T: DeserializeOwned>(
    params: Option<&RawValue>,
) -> std::result::Result<T, RpcError> {
    serde_json::from_str(params.map_or("{}", RawValue::get)).map_err(|_| RpcError::invalid_params())
}

fn agent_not_found(agent_ref: &str) -> RpcError {
    RpcError::not_found(format!("agent not found: {agent_ref}"), "agent_not_found")
}

fn conversation_not_found(conversation_ref: &str) -> RpcError {
    RpcError::not_found(
        format!("conversation not found: {conversation_ref}"),
        "conversation_not_found",
    )
}

fn harness_kind_name(kind: AgentHarnessKind) -> &'static str {
    match kind {
        AgentHarnessKind::Basic => "basic",
        AgentHarnessKind::Rlm => "rlm",
        AgentHarnessKind::TypeScript => "typescript",
        AgentHarnessKind::Exo => "exo",
    }
}

async fn write_frame<W, T>(writer: &SharedWriter<W>, frame: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let encoded = serde_json::to_vec(frame).context("serialize agent server frame")?;
    let mut writer = writer.lock().await;
    writer
        .write_all(&encoded)
        .await
        .context("write agent server frame")?;
    writer
        .write_all(b"\n")
        .await
        .context("write agent server frame delimiter")?;
    writer.flush().await.context("flush agent server frame")
}

async fn write_stream_frame<W, T>(
    writer: &SharedWriter<W>,
    frame: &T,
    stream: &mut ExecutionStreamHandle,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    if let Err(error) = write_frame(writer, frame).await {
        return match stream.cancel().await {
            Ok(()) => Err(error),
            Err(cancel_error) => Err(error.context(format!(
                "also failed to cancel executor stream: {cancel_error}"
            ))),
        };
    }
    Ok(())
}

enum ServeEvent {
    Input(std::io::Result<Option<InputFrame>>),
    Operation(std::result::Result<Result<()>, JoinError>),
}

async fn next_event<R>(
    frames: &mut FrameReader<R>,
    operations: &mut JoinSet<Result<()>>,
) -> ServeEvent
where
    R: AsyncRead + Unpin,
{
    if operations.is_empty() {
        return ServeEvent::Input(frames.next_frame().await);
    }
    tokio::select! {
        frame = frames.next_frame() => ServeEvent::Input(frame),
        Some(result) = operations.join_next() => ServeEvent::Operation(result),
    }
}

fn operation_result(result: std::result::Result<Result<()>, JoinError>) -> Result<()> {
    result.context("agent server turn task failed")?
}

enum InputFrame {
    Bytes(Vec<u8>),
    TooLarge,
}

struct FrameReader<R> {
    reader: BufReader<R>,
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
        }
    }

    async fn next_frame(&mut self) -> std::io::Result<Option<InputFrame>> {
        let mut frame = Vec::new();
        let mut too_large = false;

        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(if too_large {
                    Some(InputFrame::TooLarge)
                } else if frame.is_empty() {
                    None
                } else {
                    Some(InputFrame::Bytes(frame))
                });
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            let payload_len = newline.unwrap_or(consumed);
            if !too_large {
                if frame.len() + payload_len > MAX_FRAME_BYTES {
                    frame.clear();
                    too_large = true;
                } else {
                    frame.extend_from_slice(&available[..payload_len]);
                }
            }
            self.reader.consume(consumed);

            if newline.is_some() {
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                return Ok(Some(if too_large {
                    InputFrame::TooLarge
                } else {
                    InputFrame::Bytes(frame)
                }));
            }
        }
    }
}

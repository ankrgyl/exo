use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

use crate::{
    FirecrackerConfig, FirecrackerSandboxBackend, ManagedSandboxBackend, ManagedSandboxHandle,
    SandboxCommand, SandboxCommandOutput, SandboxProcessParts, SandboxRequest,
};

const MAX_BRIDGE_FRAME_BYTES: usize = 16 * 1024 * 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum FirecrackerBridgeRequest {
    Acquire {
        config: FirecrackerConfig,
        request: SandboxRequest,
    },
    Exec {
        config: FirecrackerConfig,
        request: SandboxRequest,
        command: SandboxCommand,
    },
    StartProcess {
        config: FirecrackerConfig,
        request: SandboxRequest,
        command: SandboxCommand,
    },
    ConnectTcp {
        config: FirecrackerConfig,
        request: SandboxRequest,
        port: u16,
    },
    Stop {
        config: FirecrackerConfig,
        request: SandboxRequest,
    },
    Fork {
        config: FirecrackerConfig,
        source: SandboxRequest,
        target: SandboxRequest,
    },
    Terminate {
        config: FirecrackerConfig,
        request: SandboxRequest,
    },
}

impl FirecrackerBridgeRequest {
    fn is_stream(&self) -> bool {
        matches!(self, Self::StartProcess { .. } | Self::ConnectTcp { .. })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum FirecrackerBridgeResponse {
    Acquired {
        id: String,
        provider_state: Option<Value>,
        effective_image: Option<String>,
    },
    Exec {
        output: SandboxCommandOutput,
    },
    Stopped,
    Forked {
        id: String,
        provider_state: Option<Value>,
        effective_image: Option<String>,
    },
    Terminated,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum FirecrackerBridgeClientFrame {
    Request {
        id: u64,
        request: Box<FirecrackerBridgeRequest>,
    },
    StreamInput {
        id: u64,
        data: String,
    },
    StreamInputClosed {
        id: u64,
    },
    StreamCancel {
        id: u64,
    },
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirecrackerBridgeStreamChannel {
    Stdout,
    Stderr,
    Tcp,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum FirecrackerBridgeServerFrame {
    Response {
        id: u64,
        response: Option<FirecrackerBridgeResponse>,
        error: Option<String>,
    },
    StreamOpened {
        id: u64,
    },
    StreamData {
        id: u64,
        channel: FirecrackerBridgeStreamChannel,
        data: String,
    },
    StreamClosed {
        id: u64,
        channel: FirecrackerBridgeStreamChannel,
    },
    ProcessExited {
        id: u64,
        exit_code: i32,
    },
    StreamError {
        id: u64,
        message: String,
    },
}

enum BridgeStreamInput {
    Data(Vec<u8>),
    Closed,
    Cancel,
}

struct BridgeBackendCache {
    backends: Mutex<HashMap<String, Arc<FirecrackerSandboxBackend>>>,
}

impl BridgeBackendCache {
    fn new() -> Self {
        Self {
            backends: Mutex::new(HashMap::new()),
        }
    }

    async fn backend(&self, config: FirecrackerConfig) -> Result<Arc<FirecrackerSandboxBackend>> {
        let key = serde_json::to_string(&config)?;
        let mut backends = self.backends.lock().await;
        if let Some(backend) = backends.get(&key) {
            return Ok(Arc::clone(backend));
        }
        let backend = Arc::new(FirecrackerSandboxBackend::new(config)?);
        backends.insert(key, Arc::clone(&backend));
        Ok(backend)
    }

    async fn acquire(
        &self,
        config: FirecrackerConfig,
        request: SandboxRequest,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        self.backend(config).await?.acquire(request).await
    }
}

type BridgeWriter = Arc<Mutex<tokio::io::Stdout>>;
type BridgeStreams = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<BridgeStreamInput>>>>;

pub async fn run_firecracker_bridge() -> Result<Option<i32>> {
    let writer = Arc::new(Mutex::new(tokio::io::stdout()));
    let streams = Arc::new(Mutex::new(HashMap::new()));
    let backends = Arc::new(BridgeBackendCache::new());
    let mut reader = tokio::io::stdin();

    loop {
        let frame = match read_frame::<FirecrackerBridgeClientFrame>(&mut reader).await {
            Ok(frame) => frame,
            Err(error) if is_unexpected_eof(&error) => break,
            Err(error) => return Err(error),
        };
        match frame {
            FirecrackerBridgeClientFrame::Request { id, request } if request.is_stream() => {
                let writer = Arc::clone(&writer);
                let streams = Arc::clone(&streams);
                let backends = Arc::clone(&backends);
                tokio::spawn(async move {
                    let result = open_stream(id, *request, &backends, &streams, &writer).await;
                    if let Err(error) = result {
                        streams.lock().await.remove(&id);
                        if let Err(send_error) = send_server_frame(
                            &writer,
                            &FirecrackerBridgeServerFrame::StreamError {
                                id,
                                message: format!("{error:#}"),
                            },
                        )
                        .await
                        {
                            tracing::debug!(%send_error, id, "failed to send Firecracker bridge stream error");
                        }
                    }
                });
            }
            FirecrackerBridgeClientFrame::Request { id, request } => {
                let writer = Arc::clone(&writer);
                let backends = Arc::clone(&backends);
                tokio::spawn(async move {
                    let result = handle_request(*request, &backends).await;
                    let frame = match result {
                        Ok(response) => FirecrackerBridgeServerFrame::Response {
                            id,
                            response: Some(response),
                            error: None,
                        },
                        Err(error) => FirecrackerBridgeServerFrame::Response {
                            id,
                            response: None,
                            error: Some(format!("{error:#}")),
                        },
                    };
                    if let Err(error) = send_server_frame(&writer, &frame).await {
                        tracing::debug!(%error, id, "failed to send Firecracker bridge response");
                    }
                });
            }
            FirecrackerBridgeClientFrame::StreamInput { id, data } => {
                send_stream_input(&streams, id, BridgeStreamInput::Data(BASE64.decode(data)?))
                    .await;
            }
            FirecrackerBridgeClientFrame::StreamInputClosed { id } => {
                send_stream_input(&streams, id, BridgeStreamInput::Closed).await;
            }
            FirecrackerBridgeClientFrame::StreamCancel { id } => {
                send_stream_input(&streams, id, BridgeStreamInput::Cancel).await;
            }
            FirecrackerBridgeClientFrame::Shutdown => break,
        }
    }

    let streams = std::mem::take(&mut *streams.lock().await);
    for (_, sender) in streams {
        if sender.send(BridgeStreamInput::Cancel).is_err() {
            tracing::debug!("Firecracker bridge stream already closed during shutdown");
        }
    }
    Ok(None)
}

async fn send_stream_input(streams: &BridgeStreams, id: u64, input: BridgeStreamInput) {
    let sender = streams.lock().await.get(&id).cloned();
    if let Some(sender) = sender
        && sender.send(input).is_err()
    {
        streams.lock().await.remove(&id);
    }
}

async fn handle_request(
    request: FirecrackerBridgeRequest,
    backends: &BridgeBackendCache,
) -> Result<FirecrackerBridgeResponse> {
    match request {
        FirecrackerBridgeRequest::Acquire { config, request } => {
            let handle = backends.acquire(config, request).await?;
            Ok(FirecrackerBridgeResponse::Acquired {
                id: handle.id().to_string(),
                provider_state: handle.provider_state(),
                effective_image: handle.effective_image(),
            })
        }
        FirecrackerBridgeRequest::Exec {
            config,
            request,
            command,
        } => Ok(FirecrackerBridgeResponse::Exec {
            output: backends
                .acquire(config, request)
                .await?
                .exec(&command)
                .await?,
        }),
        FirecrackerBridgeRequest::Stop { config, request } => {
            backends.acquire(config, request).await?.stop().await?;
            Ok(FirecrackerBridgeResponse::Stopped)
        }
        FirecrackerBridgeRequest::Fork {
            config,
            source,
            target,
        } => {
            let handle = backends
                .backend(config)
                .await?
                .fork_sandbox(source, target)
                .await?
                .context("Firecracker backend did not fork the sandbox")?;
            Ok(FirecrackerBridgeResponse::Forked {
                id: handle.id().to_string(),
                provider_state: handle.provider_state(),
                effective_image: handle.effective_image(),
            })
        }
        FirecrackerBridgeRequest::Terminate { config, request } => {
            backends.backend(config).await?.terminate(request).await?;
            Ok(FirecrackerBridgeResponse::Terminated)
        }
        FirecrackerBridgeRequest::StartProcess { .. }
        | FirecrackerBridgeRequest::ConnectTcp { .. } => {
            bail!("streaming Firecracker request reached the RPC handler")
        }
    }
}

async fn open_stream(
    id: u64,
    request: FirecrackerBridgeRequest,
    backends: &BridgeBackendCache,
    streams: &BridgeStreams,
    writer: &BridgeWriter,
) -> Result<()> {
    let (input_sender, input_receiver) = mpsc::unbounded_channel();
    if streams.lock().await.insert(id, input_sender).is_some() {
        bail!("duplicate Firecracker bridge stream id {id}");
    }
    match request {
        FirecrackerBridgeRequest::StartProcess {
            config,
            request,
            command,
        } => {
            let parts = backends
                .acquire(config, request)
                .await?
                .start_process(&command)
                .await?;
            send_server_frame(writer, &FirecrackerBridgeServerFrame::StreamOpened { id }).await?;
            proxy_process(id, parts, input_receiver, writer).await?;
        }
        FirecrackerBridgeRequest::ConnectTcp {
            config,
            request,
            port,
        } => {
            let stream = backends
                .acquire(config, request)
                .await?
                .connect_tcp(port)
                .await?
                .context("Firecracker sandbox does not support TCP connections")?;
            send_server_frame(writer, &FirecrackerBridgeServerFrame::StreamOpened { id }).await?;
            proxy_tcp(id, stream, input_receiver, writer).await?;
        }
        _ => bail!("non-streaming Firecracker request reached the stream handler"),
    }
    streams.lock().await.remove(&id);
    Ok(())
}

async fn proxy_process(
    id: u64,
    parts: SandboxProcessParts,
    mut input_receiver: mpsc::UnboundedReceiver<BridgeStreamInput>,
    writer: &BridgeWriter,
) -> Result<()> {
    let SandboxProcessParts {
        stdout,
        stderr,
        stdin,
        mut wait,
    } = parts;
    let stdout_task = tokio::spawn(copy_output(
        id,
        FirecrackerBridgeStreamChannel::Stdout,
        stdout.compat(),
        Arc::clone(writer),
    ));
    let stderr_task = tokio::spawn(copy_output(
        id,
        FirecrackerBridgeStreamChannel::Stderr,
        stderr.compat(),
        Arc::clone(writer),
    ));
    let mut stdin = stdin.compat_write();
    let exit_code = loop {
        tokio::select! {
            result = wait.as_mut() => break result?,
            input = input_receiver.recv() => match input {
                Some(BridgeStreamInput::Data(data)) => {
                    stdin.write_all(&data).await?;
                    stdin.flush().await?;
                }
                Some(BridgeStreamInput::Closed) => stdin.shutdown().await?,
                Some(BridgeStreamInput::Cancel) | None => {
                    stdout_task.abort();
                    stderr_task.abort();
                    return Ok(());
                }
            }
        }
    };
    match tokio::time::timeout(OUTPUT_DRAIN_GRACE, async {
        stdout_task
            .await
            .context("joining Firecracker stdout bridge")??;
        stderr_task
            .await
            .context("joining Firecracker stderr bridge")??;
        Result::<()>::Ok(())
    })
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            // A background guest child may retain inherited descriptors after the
            // requested process exits. The caller already has the terminal status.
        }
    }
    send_server_frame(
        writer,
        &FirecrackerBridgeServerFrame::ProcessExited { id, exit_code },
    )
    .await
}

async fn copy_output(
    id: u64,
    channel: FirecrackerBridgeStreamChannel,
    mut source: impl tokio::io::AsyncRead + Unpin,
    writer: BridgeWriter,
) -> Result<()> {
    let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
    loop {
        let read = source.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        send_server_frame(
            &writer,
            &FirecrackerBridgeServerFrame::StreamData {
                id,
                channel,
                data: BASE64.encode(&buffer[..read]),
            },
        )
        .await?;
    }
    send_server_frame(
        &writer,
        &FirecrackerBridgeServerFrame::StreamClosed { id, channel },
    )
    .await
}

async fn proxy_tcp(
    id: u64,
    stream: crate::BoxSandboxTcpStream,
    mut input_receiver: mpsc::UnboundedReceiver<BridgeStreamInput>,
    writer: &BridgeWriter,
) -> Result<()> {
    let (mut reader, mut writer_half) = tokio::io::split(stream);
    let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
    loop {
        tokio::select! {
            read = reader.read(&mut buffer) => {
                let read = read?;
                if read == 0 {
                    send_server_frame(
                        writer,
                        &FirecrackerBridgeServerFrame::StreamClosed {
                            id,
                            channel: FirecrackerBridgeStreamChannel::Tcp,
                        },
                    )
                    .await?;
                    return Ok(());
                }
                send_server_frame(
                    writer,
                    &FirecrackerBridgeServerFrame::StreamData {
                        id,
                        channel: FirecrackerBridgeStreamChannel::Tcp,
                        data: BASE64.encode(&buffer[..read]),
                    },
                )
                .await?;
            }
            input = input_receiver.recv() => match input {
                Some(BridgeStreamInput::Data(data)) => {
                    writer_half.write_all(&data).await?;
                    writer_half.flush().await?;
                }
                Some(BridgeStreamInput::Closed) => writer_half.shutdown().await?,
                Some(BridgeStreamInput::Cancel) | None => return Ok(()),
            }
        }
    }
}

async fn send_server_frame(
    writer: &BridgeWriter,
    frame: &FirecrackerBridgeServerFrame,
) -> Result<()> {
    write_frame(&mut *writer.lock().await, frame).await
}

fn is_unexpected_eof(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == ErrorKind::UnexpectedEof)
}

pub async fn write_frame<T: Serialize>(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &T,
) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_BRIDGE_FRAME_BYTES {
        bail!("Firecracker bridge request exceeds {MAX_BRIDGE_FRAME_BYTES} bytes");
    }
    writer.write_u64(payload.len() as u64).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<T: DeserializeOwned>(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<T> {
    let length = reader.read_u64().await?;
    let length = usize::try_from(length).context("Firecracker bridge frame length overflows")?;
    if length > MAX_BRIDGE_FRAME_BYTES {
        bail!("Firecracker bridge response exceeds {MAX_BRIDGE_FRAME_BYTES} bytes");
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).map_err(|error| anyhow!(error))
}

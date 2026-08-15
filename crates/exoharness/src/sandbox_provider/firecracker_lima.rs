//! macOS transport for the Linux Firecracker backend.
//!
//! Firecracker only runs on Linux/KVM. On Apple silicon that supports nested
//! virtualization, Lima can provide the Linux/KVM boundary while the Exo caller
//! remains a native macOS process:
//! https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration/isnestedvirtualizationsupported
//! https://github.com/lima-vm/lima/blob/master/templates/default.yaml

use std::collections::HashMap;
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};

use anyhow::{Context as AnyhowContext, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, ReadBuf};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::PollSender;

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxRequest, SnapshotPayload,
};
use crate::{SandboxAttachment, SandboxProcessParts};

use super::firecracker::FirecrackerConfig;
use super::firecracker_bridge::{
    FirecrackerBridgeClientFrame, FirecrackerBridgeRequest, FirecrackerBridgeResponse,
    FirecrackerBridgeServerFrame, FirecrackerBridgeStreamChannel, read_frame, write_frame,
};

const DEFAULT_LIMA_INSTANCE: &str = "exo-firecracker";
const DEFAULT_LIMA_TARGET_DIR: &str = "/var/tmp/exo-firecracker-bridge-target";
const BRIDGE_FRAME_QUEUE_DEPTH: usize = 16;
const BRIDGE_STREAM_QUEUE_DEPTH: usize = 16;
const BRIDGE_STREAM_CHUNK_BYTES: usize = 64 * 1024;

pub struct LimaFirecrackerSandboxBackend {
    config: FirecrackerConfig,
    limactl: PathBuf,
    instance: String,
    bridge_binary: PathBuf,
    bridge: Arc<LimaBridgeManager>,
}

impl LimaFirecrackerSandboxBackend {
    pub async fn from_env(mut config: FirecrackerConfig) -> Result<Self> {
        if env::var_os("EXO_FIRECRACKER_MEMORY_MIB").is_none() {
            config.memory_mib = 1024;
        }
        let limactl = env_path("EXO_FIRECRACKER_LIMACTL", "limactl");
        let instance = env::var("EXO_FIRECRACKER_LIMA_INSTANCE")
            .unwrap_or_else(|_| DEFAULT_LIMA_INSTANCE.to_string());
        let target_dir = env_path("EXO_FIRECRACKER_LIMA_TARGET_DIR", DEFAULT_LIMA_TARGET_DIR);
        let bridge_binary = env::var_os("EXO_FIRECRACKER_LIMA_EXO_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| target_dir.join("debug/exo"));
        let bridge = Arc::new(LimaBridgeManager::new(
            limactl.clone(),
            instance.clone(),
            bridge_binary.clone(),
        ));
        let backend = Self {
            config,
            limactl,
            instance,
            bridge_binary,
            bridge,
        };
        backend.prepare_bridge(&target_dir).await?;
        backend.bridge.connection().await?;
        Ok(backend)
    }

    async fn prepare_bridge(&self, target_dir: &Path) -> Result<()> {
        self.run_checked(
            Command::new(&self.limactl).arg("start").arg(&self.instance),
            "starting the Firecracker Lima VM",
        )
        .await?;
        if env::var_os("EXO_FIRECRACKER_LIMA_EXO_BINARY").is_some() {
            return Ok(());
        }
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("resolving the Exo source root")?;
        let mut command = Command::new(&self.limactl);
        // Keep the one-time Linux build viable in the 4 GiB development VM:
        // GNU ld retained multiple GiB while linking Exo, whereas LLVM lld is
        // designed for lower-memory parallel linking. Debug info and incremental
        // state are not useful for this executable-only bridge cache.
        // https://lld.llvm.org/
        command
            .arg("shell")
            .arg(&self.instance)
            .arg("--")
            .arg("env")
            .arg(format!("CARGO_TARGET_DIR={}", target_dir.display()))
            .arg("CARGO_BUILD_JOBS=1")
            .arg("CARGO_INCREMENTAL=0")
            .arg("CARGO_PROFILE_DEV_DEBUG=0")
            .arg("RUSTFLAGS=-C linker=clang -C link-arg=-fuse-ld=lld")
            .arg("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(source_root.join("Cargo.toml"))
            .arg("--package")
            .arg("exo")
            .arg("--features")
            .arg("firecracker")
            .arg("--bin")
            .arg("exo");
        self.run_checked(&mut command, "building the Exo Firecracker bridge in Lima")
            .await
    }

    async fn run_checked(&self, command: &mut Command, description: &str) -> Result<()> {
        let output = command.output().await?;
        if output.status.success() {
            return Ok(());
        }
        bail!(
            "{description} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }

    async fn request(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<FirecrackerBridgeResponse> {
        self.bridge.request(request).await
    }

    fn client(&self) -> Arc<Self> {
        Arc::new(Self {
            config: self.config.clone(),
            limactl: self.limactl.clone(),
            instance: self.instance.clone(),
            bridge_binary: self.bridge_binary.clone(),
            bridge: Arc::clone(&self.bridge),
        })
    }
}

#[async_trait]
impl ManagedSandboxBackend for LimaFirecrackerSandboxBackend {
    fn is_local(&self) -> bool {
        true
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let response = self
            .request(FirecrackerBridgeRequest::Acquire {
                config: self.config.clone(),
                request: request.clone(),
            })
            .await?;
        let FirecrackerBridgeResponse::Acquired {
            id,
            provider_state,
            effective_image,
        } = response
        else {
            bail!("Firecracker Lima bridge returned the wrong response to acquire");
        };
        Ok(Arc::new(LimaFirecrackerSandboxHandle {
            id,
            provider_state,
            effective_image,
            request,
            backend: self.client(),
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Firecracker sandboxes do not support external attachments")
    }

    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        match self
            .request(FirecrackerBridgeRequest::Terminate {
                config: self.config.clone(),
                request,
            })
            .await?
        {
            FirecrackerBridgeResponse::Terminated => Ok(()),
            _ => bail!("Firecracker Lima bridge returned the wrong response to terminate"),
        }
    }

    async fn fork_sandbox(
        &self,
        source: SandboxRequest,
        target: SandboxRequest,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let response = self
            .request(FirecrackerBridgeRequest::Fork {
                config: self.config.clone(),
                source,
                target: target.clone(),
            })
            .await?;
        let FirecrackerBridgeResponse::Forked {
            id,
            provider_state,
            effective_image,
        } = response
        else {
            bail!("Firecracker Lima bridge returned the wrong response to fork");
        };
        Ok(Arc::new(LimaFirecrackerSandboxHandle {
            id,
            provider_state,
            effective_image,
            request: target,
            backend: self.client(),
        }))
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Firecracker sandboxes do not support restoring Exo snapshots")
    }
}

struct LimaFirecrackerSandboxHandle {
    id: String,
    provider_state: Option<serde_json::Value>,
    effective_image: Option<String>,
    request: SandboxRequest,
    backend: Arc<LimaFirecrackerSandboxBackend>,
}

#[async_trait]
impl ManagedSandboxHandle for LimaFirecrackerSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn provider_state(&self) -> Option<serde_json::Value> {
        self.provider_state.clone()
    }

    fn effective_image(&self) -> Option<String> {
        self.effective_image.clone()
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let response = self
            .backend
            .request(FirecrackerBridgeRequest::Exec {
                config: self.backend.config.clone(),
                request: self.request.clone(),
                command: command.clone(),
            })
            .await?;
        let FirecrackerBridgeResponse::Exec { output } = response else {
            bail!("Firecracker Lima bridge returned the wrong response to exec");
        };
        Ok(output)
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        self.backend
            .bridge
            .start_process(FirecrackerBridgeRequest::StartProcess {
                config: self.backend.config.clone(),
                request: self.request.clone(),
                command: command.clone(),
            })
            .await
    }

    async fn stop(&self) -> Result<()> {
        match self
            .backend
            .request(FirecrackerBridgeRequest::Stop {
                config: self.backend.config.clone(),
                request: self.request.clone(),
            })
            .await?
        {
            FirecrackerBridgeResponse::Stopped => Ok(()),
            _ => bail!("Firecracker Lima bridge returned the wrong response to stop"),
        }
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("Firecracker sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!("Firecracker sandbox snapshots are not implemented")
    }
}

struct LimaBridgeManager {
    limactl: PathBuf,
    instance: String,
    bridge_binary: PathBuf,
    connection: Mutex<Option<Arc<LimaBridgeConnection>>>,
}

impl LimaBridgeManager {
    fn new(limactl: PathBuf, instance: String, bridge_binary: PathBuf) -> Self {
        Self {
            limactl,
            instance,
            bridge_binary,
            connection: Mutex::new(None),
        }
    }

    fn bridge_command(&self) -> Command {
        let mut command = Command::new(&self.limactl);
        // The direct backend deliberately uses Firecracker's jailer and host
        // network setup, both of which require root inside the Lima VM. `-n`
        // prevents an invisible password prompt from hanging the host process.
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md
        command
            .arg("shell")
            .arg(&self.instance)
            .arg("--")
            .arg("sudo")
            .arg("-n")
            .arg(&self.bridge_binary)
            .arg("firecracker-bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    async fn connection(&self) -> Result<Arc<LimaBridgeConnection>> {
        let mut connection = self.connection.lock().await;
        if let Some(active) = connection.as_ref()
            && !active.is_closed()
        {
            return Ok(Arc::clone(active));
        }
        let active = Arc::new(LimaBridgeConnection::spawn(self.bridge_command()).await?);
        *connection = Some(Arc::clone(&active));
        Ok(active)
    }

    async fn invalidate(&self, failed: &Arc<LimaBridgeConnection>) {
        let mut connection = self.connection.lock().await;
        if connection
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, failed))
        {
            *connection = None;
        }
    }

    async fn request(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<FirecrackerBridgeResponse> {
        for attempt in 0..2 {
            let connection = self.connection().await?;
            match connection.request(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) if connection.is_closed() && attempt == 0 => {
                    self.invalidate(&connection).await;
                    tracing::debug!(%error, "restarting closed Firecracker Lima bridge");
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("Firecracker bridge request retry loop must return")
    }

    async fn start_process(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<SandboxProcessParts> {
        for attempt in 0..2 {
            let connection = self.connection().await?;
            match connection.start_process(request.clone()).await {
                Ok(parts) => return Ok(parts),
                Err(error) if connection.is_closed() && attempt == 0 => {
                    self.invalidate(&connection).await;
                    tracing::debug!(%error, "restarting closed Firecracker Lima bridge");
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("Firecracker bridge process retry loop must return")
    }
}

impl Drop for LimaBridgeManager {
    fn drop(&mut self) {
        if let Ok(connection) = self.connection.try_lock()
            && let Some(connection) = connection.as_ref()
        {
            connection.shutdown();
        }
    }
}

type RpcResult = std::result::Result<FirecrackerBridgeResponse, String>;
type OpenResult = std::result::Result<(), String>;
type ExitResult = std::result::Result<i32, String>;

struct LimaBridgeConnection {
    outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>,
    state: Arc<LimaBridgeClientState>,
    next_id: AtomicU64,
}

struct LimaBridgeClientState {
    requests: StdMutex<HashMap<u64, oneshot::Sender<RpcResult>>>,
    streams: StdMutex<HashMap<u64, ClientStreamRoutes>>,
    closed: AtomicBool,
    close_reason: StdMutex<Option<String>>,
}

struct ClientStreamRoutes {
    opened: Option<oneshot::Sender<OpenResult>>,
    stdout: Option<mpsc::Sender<BridgeReadEvent>>,
    stderr: Option<mpsc::Sender<BridgeReadEvent>>,
    exited: Option<oneshot::Sender<ExitResult>>,
}

enum BridgeReadEvent {
    Data(Vec<u8>),
    Closed,
    Error(String),
}

impl LimaBridgeConnection {
    async fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .context("opening Firecracker bridge stdin")?;
        let mut stdout = child
            .stdout
            .take()
            .context("opening Firecracker bridge stdout")?;
        let mut stderr = child
            .stderr
            .take()
            .context("opening Firecracker bridge stderr")?;
        let state = Arc::new(LimaBridgeClientState::new());
        let (outgoing, mut outgoing_receiver) = mpsc::channel(BRIDGE_FRAME_QUEUE_DEPTH);

        let writer_state = Arc::clone(&state);
        tokio::spawn(async move {
            while let Some(frame) = outgoing_receiver.recv().await {
                if let Err(error) = write_frame(&mut stdin, &frame).await {
                    writer_state.fail(format!("writing Firecracker Lima bridge: {error:#}"));
                    return;
                }
            }
        });

        let reader_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                match read_frame::<FirecrackerBridgeServerFrame>(&mut stdout).await {
                    Ok(frame) => {
                        if let Err(error) = reader_state.handle_frame(frame).await {
                            reader_state
                                .fail(format!("decoding Firecracker Lima bridge: {error:#}"));
                            return;
                        }
                    }
                    Err(error) => {
                        reader_state.fail(format!("reading Firecracker Lima bridge: {error:#}"));
                        return;
                    }
                }
            }
        });

        tokio::spawn(async move {
            let mut lines = BufReader::new(&mut stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(message)) => tracing::info!(
                        target: "exo_firecracker_lima_bridge",
                        %message,
                        "Firecracker Lima bridge"
                    ),
                    Ok(None) => return,
                    Err(error) => {
                        tracing::debug!(%error, "failed to drain Firecracker Lima bridge stderr");
                        return;
                    }
                }
            }
        });

        let wait_state = Arc::clone(&state);
        tokio::spawn(async move {
            let reason = match child.wait().await {
                Ok(status) => format!("Firecracker Lima bridge exited with {status}"),
                Err(error) => format!("waiting for Firecracker Lima bridge: {error}"),
            };
            wait_state.fail(reason);
        });

        Ok(Self {
            outgoing,
            state,
            next_id: AtomicU64::new(1),
        })
    }

    fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send(&self, frame: FirecrackerBridgeClientFrame) -> Result<()> {
        self.outgoing.send(frame).await.map_err(|_| {
            anyhow!(
                "Firecracker Lima bridge is closed: {}",
                self.state.close_reason()
            )
        })
    }

    async fn request(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<FirecrackerBridgeResponse> {
        let id = self.next_id();
        let (sender, receiver) = oneshot::channel();
        self.state
            .requests
            .lock()
            .map_err(|_| anyhow!("Firecracker bridge request lock is poisoned"))?
            .insert(id, sender);
        if let Err(error) = self
            .send(FirecrackerBridgeClientFrame::Request {
                id,
                request: Box::new(request),
            })
            .await
        {
            self.state.remove_request(id);
            return Err(error);
        }
        receiver
            .await
            .map_err(|_| anyhow!("Firecracker Lima bridge dropped request {id}"))?
            .map_err(|message| anyhow!(message))
    }

    async fn start_process(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<SandboxProcessParts> {
        let id = self.next_id();
        let (opened_sender, opened_receiver) = oneshot::channel();
        let (stdout_sender, stdout_receiver) = mpsc::channel(BRIDGE_STREAM_QUEUE_DEPTH);
        let (stderr_sender, stderr_receiver) = mpsc::channel(BRIDGE_STREAM_QUEUE_DEPTH);
        let (exit_sender, exit_receiver) = oneshot::channel();
        self.state.insert_stream(
            id,
            ClientStreamRoutes {
                opened: Some(opened_sender),
                stdout: Some(stdout_sender),
                stderr: Some(stderr_sender),
                exited: Some(exit_sender),
            },
        )?;
        if let Err(error) = self
            .send(FirecrackerBridgeClientFrame::Request {
                id,
                request: Box::new(request),
            })
            .await
        {
            self.state.remove_stream(id);
            return Err(error);
        }
        opened_receiver
            .await
            .map_err(|_| anyhow!("Firecracker Lima bridge dropped process open {id}"))?
            .map_err(|message| anyhow!(message))?;

        let cancel = BridgeStreamCancel::new(id, self.outgoing.clone(), Arc::clone(&self.state));
        let wait = Box::pin(async move {
            let mut cancel = cancel;
            let result = exit_receiver
                .await
                .map_err(|_| anyhow!("Firecracker Lima bridge dropped process exit {id}"))?
                .map_err(|message| anyhow!(message));
            cancel.disarm();
            result
        });
        Ok(SandboxProcessParts {
            stdout: Box::pin(BridgeReadStream::new(stdout_receiver).compat()),
            stderr: Box::pin(BridgeReadStream::new(stderr_receiver).compat()),
            stdin: Box::pin(BridgeWriteStream::new(id, self.outgoing.clone()).compat_write()),
            wait,
        })
    }

    fn shutdown(&self) {
        if self
            .outgoing
            .try_send(FirecrackerBridgeClientFrame::Shutdown)
            .is_err()
        {
            tracing::debug!("Firecracker Lima bridge was already closed during shutdown");
        }
    }
}

impl LimaBridgeClientState {
    fn new() -> Self {
        Self {
            requests: StdMutex::new(HashMap::new()),
            streams: StdMutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            close_reason: StdMutex::new(None),
        }
    }

    fn close_reason(&self) -> String {
        self.close_reason
            .lock()
            .ok()
            .and_then(|reason| reason.clone())
            .unwrap_or_else(|| "connection closed".to_string())
    }

    fn fail(&self, reason: String) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut close_reason) = self.close_reason.lock() {
            *close_reason = Some(reason.clone());
        }
        if let Ok(mut requests) = self.requests.lock() {
            for (_, sender) in requests.drain() {
                if sender.send(Err(reason.clone())).is_err() {
                    tracing::debug!("Firecracker bridge request receiver already closed");
                }
            }
        }
        if let Ok(mut streams) = self.streams.lock() {
            for (_, routes) in streams.drain() {
                routes.fail(reason.clone());
            }
        }
    }

    async fn handle_frame(&self, frame: FirecrackerBridgeServerFrame) -> Result<()> {
        match frame {
            FirecrackerBridgeServerFrame::Response {
                id,
                response,
                error,
            } => {
                let sender = self
                    .requests
                    .lock()
                    .map_err(|_| anyhow!("Firecracker bridge request lock is poisoned"))?
                    .remove(&id)
                    .context("Firecracker bridge response has an unknown request id")?;
                let result = match (response, error) {
                    (Some(response), None) => Ok(response),
                    (None, Some(error)) => Err(error),
                    _ => bail!("Firecracker bridge response must contain one result"),
                };
                if sender.send(result).is_err() {
                    tracing::debug!(
                        id,
                        "Firecracker bridge request receiver closed before response"
                    );
                }
            }
            FirecrackerBridgeServerFrame::StreamOpened { id } => {
                let mut streams = self
                    .streams
                    .lock()
                    .map_err(|_| anyhow!("Firecracker bridge stream lock is poisoned"))?;
                let Some(routes) = streams.get_mut(&id) else {
                    return Ok(());
                };
                let sender = routes
                    .opened
                    .take()
                    .context("Firecracker bridge opened a stream twice")?;
                if sender.send(Ok(())).is_err() {
                    tracing::debug!(id, "Firecracker bridge stream opener was dropped");
                }
            }
            FirecrackerBridgeServerFrame::StreamData { id, channel, data } => {
                let bytes = BASE64.decode(data)?;
                self.send_stream_event(id, channel, BridgeReadEvent::Data(bytes))
                    .await?;
            }
            FirecrackerBridgeServerFrame::StreamClosed { id, channel } => {
                self.send_stream_event(id, channel, BridgeReadEvent::Closed)
                    .await?;
            }
            FirecrackerBridgeServerFrame::ProcessExited { id, exit_code } => {
                let Some(mut routes) = self.remove_stream(id) else {
                    return Ok(());
                };
                routes.close_outputs();
                if let Some(sender) = routes.exited.take()
                    && sender.send(Ok(exit_code)).is_err()
                {
                    tracing::debug!(id, "Firecracker bridge process waiter was dropped");
                }
            }
            FirecrackerBridgeServerFrame::StreamError { id, message } => {
                if let Some(routes) = self.remove_stream(id) {
                    routes.fail(message);
                }
            }
        }
        Ok(())
    }

    async fn send_stream_event(
        &self,
        id: u64,
        channel: FirecrackerBridgeStreamChannel,
        event: BridgeReadEvent,
    ) -> Result<()> {
        let sender = {
            let streams = self
                .streams
                .lock()
                .map_err(|_| anyhow!("Firecracker bridge stream lock is poisoned"))?;
            let Some(routes) = streams.get(&id) else {
                return Ok(());
            };
            match channel {
                FirecrackerBridgeStreamChannel::Stdout => routes.stdout.as_ref(),
                FirecrackerBridgeStreamChannel::Stderr => routes.stderr.as_ref(),
            }
            .context("Firecracker bridge data used the wrong stream channel")?
            .clone()
        };
        if sender.send(event).await.is_err() {
            tracing::debug!(id, ?channel, "Firecracker bridge stream reader was dropped");
        }
        Ok(())
    }

    fn insert_stream(&self, id: u64, routes: ClientStreamRoutes) -> Result<()> {
        if self
            .streams
            .lock()
            .map_err(|_| anyhow!("Firecracker bridge stream lock is poisoned"))?
            .insert(id, routes)
            .is_some()
        {
            bail!("duplicate Firecracker bridge stream id {id}");
        }
        Ok(())
    }

    fn remove_request(&self, id: u64) {
        if let Ok(mut requests) = self.requests.lock() {
            requests.remove(&id);
        }
    }

    fn remove_stream(&self, id: u64) -> Option<ClientStreamRoutes> {
        self.streams
            .lock()
            .ok()
            .and_then(|mut streams| streams.remove(&id))
    }
}

impl ClientStreamRoutes {
    fn close_outputs(&mut self) {
        for sender in [&self.stdout, &self.stderr].into_iter().flatten() {
            if sender.try_send(BridgeReadEvent::Closed).is_err() {
                tracing::debug!("Firecracker bridge stream reader already closed");
            }
        }
    }

    fn fail(mut self, message: String) {
        if let Some(sender) = self.opened.take()
            && sender.send(Err(message.clone())).is_err()
        {
            tracing::debug!("Firecracker bridge stream opener already closed");
        }
        for sender in [self.stdout.take(), self.stderr.take()]
            .into_iter()
            .flatten()
        {
            if sender
                .try_send(BridgeReadEvent::Error(message.clone()))
                .is_err()
            {
                tracing::debug!("Firecracker bridge stream reader already closed");
            }
        }
        if let Some(sender) = self.exited.take()
            && sender.send(Err(message)).is_err()
        {
            tracing::debug!("Firecracker bridge process waiter already closed");
        }
    }
}

struct BridgeReadStream {
    receiver: mpsc::Receiver<BridgeReadEvent>,
    buffer: Vec<u8>,
    offset: usize,
    closed: bool,
}

impl BridgeReadStream {
    fn new(receiver: mpsc::Receiver<BridgeReadEvent>) -> Self {
        Self {
            receiver,
            buffer: Vec::new(),
            offset: 0,
            closed: false,
        }
    }
}

impl AsyncRead for BridgeReadStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        destination: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if self.offset < self.buffer.len() {
                let remaining = &self.buffer[self.offset..];
                let copied = remaining.len().min(destination.remaining());
                destination.put_slice(&remaining[..copied]);
                self.offset += copied;
                return Poll::Ready(Ok(()));
            }
            if self.closed {
                return Poll::Ready(Ok(()));
            }
            match self.receiver.poll_recv(context) {
                Poll::Ready(Some(BridgeReadEvent::Data(data))) => {
                    self.buffer = data;
                    self.offset = 0;
                }
                Poll::Ready(Some(BridgeReadEvent::Closed)) | Poll::Ready(None) => {
                    self.closed = true;
                }
                Poll::Ready(Some(BridgeReadEvent::Error(message))) => {
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, message)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

struct BridgeWriteStream {
    id: u64,
    outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>,
    poll_sender: PollSender<FirecrackerBridgeClientFrame>,
    closed: bool,
}

impl BridgeWriteStream {
    fn new(id: u64, outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>) -> Self {
        Self {
            id,
            poll_sender: PollSender::new(outgoing.clone()),
            outgoing,
            closed: false,
        }
    }

    fn poll_send(
        &mut self,
        context: &mut Context<'_>,
        frame: FirecrackerBridgeClientFrame,
    ) -> Poll<io::Result<()>> {
        match self.poll_sender.poll_reserve(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Firecracker bridge closed",
            ))),
            Poll::Ready(Ok(())) => Poll::Ready(self.poll_sender.send_item(frame).map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "Firecracker bridge closed")
            })),
        }
    }
}

impl AsyncWrite for BridgeWriteStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Firecracker bridge stream input is closed",
            )));
        }
        let written = buffer.len().min(BRIDGE_STREAM_CHUNK_BYTES);
        let frame = FirecrackerBridgeClientFrame::StreamInput {
            id: self.id,
            data: BASE64.encode(&buffer[..written]),
        };
        match self.poll_send(context, frame) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(written)),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.closed {
            let id = self.id;
            match self.poll_send(
                context,
                FirecrackerBridgeClientFrame::StreamInputClosed { id },
            ) {
                Poll::Ready(Ok(())) => self.closed = true,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for BridgeWriteStream {
    fn drop(&mut self) {
        if !self.closed {
            send_bridge_frame_on_drop(
                &self.outgoing,
                FirecrackerBridgeClientFrame::StreamInputClosed { id: self.id },
            );
        }
    }
}

struct BridgeStreamCancel {
    id: u64,
    outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>,
    state: Arc<LimaBridgeClientState>,
    armed: bool,
}

impl BridgeStreamCancel {
    fn new(
        id: u64,
        outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>,
        state: Arc<LimaBridgeClientState>,
    ) -> Self {
        Self {
            id,
            outgoing,
            state,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BridgeStreamCancel {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.state.remove_stream(self.id);
        send_bridge_frame_on_drop(
            &self.outgoing,
            FirecrackerBridgeClientFrame::StreamCancel { id: self.id },
        );
    }
}

fn send_bridge_frame_on_drop(
    outgoing: &mpsc::Sender<FirecrackerBridgeClientFrame>,
    frame: FirecrackerBridgeClientFrame,
) {
    match outgoing.try_send(frame) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::debug!("Firecracker bridge already closed while dropping a stream");
        }
        Err(mpsc::error::TrySendError::Full(frame)) => {
            let outgoing = outgoing.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if outgoing.send(frame).await.is_err() {
                        tracing::debug!("Firecracker bridge closed while dropping a stream");
                    }
                });
            }
        }
    }
}

fn env_path(name: &str, default: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

//! E2B remote sandbox backend.
//!
//! Uses E2B's platform REST API (`api.e2b.app`) for lifecycle and the per-sandbox
//! envd Connect API for command execution. Cross-process resume uses sandbox
//! `metadata` (same keys as Docker/Daytona labels). Snapshots are bytes-by-reference
//! via [`SnapshotFormat::E2bRef`] manifests pointing at an E2B snapshot template id.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use uuid::Uuid;

use crate::SandboxAttachment;
use crate::sandbox::{
    DEFAULT_SANDBOX_IMAGE, ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand,
    SandboxCommandOutput, SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotFormat,
    SnapshotPayload, WARM_SANDBOX_KEY_LABEL, WARM_SANDBOX_SPEC_HASH_LABEL, sandbox_spec_hash,
};

pub const DEFAULT_E2B_API_URL: &str = "https://api.e2b.app";
pub const DEFAULT_E2B_ENVD_PORT: u16 = 49_983;

const PROCESS_PIPE_BUFFER_SIZE: usize = 64 * 1024;

/// Connect envelope flag: payload is gzip-compressed (unsupported here).
const CONNECT_FLAG_COMPRESSED: u8 = 0x01;
/// Connect envelope flag: final message carrying stream status / errors.
const CONNECT_FLAG_END_STREAM: u8 = 0x02;
const CONNECT_MAX_ENVELOPE_BYTES: usize = 16 * 1024 * 1024;
static CONSUMABLE_SNAPSHOT_FORMATS: [SnapshotFormat; 1] = [SnapshotFormat::E2bRef];

#[derive(Debug, Clone)]
pub struct E2bConfig {
    pub api_key: String,
    pub api_url: String,
    pub template_id: String,
    pub envd_port: u16,
    /// When set (tests), envd requests go to this base URL instead of `{port}-{sandboxID}.e2b.app`.
    pub envd_base_url: Option<String>,
    /// When false, envd endpoints do not require `X-Access-Token` (E2B default for SDK is true).
    pub secure: bool,
}

/// JSON persisted for [`SnapshotFormat::E2bRef`]. Filesystem state lives in E2B;
/// we only store the snapshot template id returned by `POST /sandboxes/{id}/snapshots`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct E2bSnapshotManifest {
    snapshot_id: String,
    base_template: String,
}

pub struct E2bSandboxBackend {
    client: reqwest::Client,
    api_url: String,
    template_id: String,
    envd_port: u16,
    envd_base_url: Option<String>,
    secure: bool,
}

impl E2bSandboxBackend {
    pub fn new(config: E2bConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let api_key = HeaderValue::from_str(&config.api_key)
            .context("E2B_API_KEY contains characters that aren't valid in an HTTP header")?;
        headers.insert("X-API-Key", api_key);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("building E2B HTTP client")?;
        Ok(Self {
            client,
            api_url: config.api_url.trim_end_matches('/').to_string(),
            template_id: config.template_id,
            envd_port: config.envd_port,
            envd_base_url: config.envd_base_url,
            secure: config.secure,
        })
    }

    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    async fn find_sandbox_by_metadata(
        &self,
        key_label: &str,
        spec_hash: &str,
    ) -> Result<Option<E2bListedSandbox>> {
        let metadata = metadata_filter_query(key_label, spec_hash);
        // OpenAPI `state` uses style=form, explode=false → `state=running,paused`.
        // Two `state=` query params are not accepted reliably by the API.
        if let Some(found) = self.list_sandboxes_with_metadata(&metadata, true).await? {
            return Ok(Some(found));
        }
        // Fallback: metadata only (e.g. API ignores unknown state serialization).
        self.list_sandboxes_with_metadata(&metadata, false).await
    }

    async fn list_sandboxes_with_metadata(
        &self,
        metadata: &str,
        with_state_filter: bool,
    ) -> Result<Option<E2bListedSandbox>> {
        let mut query: Vec<(&str, &str)> = vec![("metadata", metadata)];
        if with_state_filter {
            query.push(("state", "running,paused"));
        }
        let response = self
            .client
            .get(self.api_endpoint("/v2/sandboxes"))
            .query(&query)
            .send()
            .await
            .context("listing E2B sandboxes")?
            .error_for_status()
            .context("E2B list sandboxes returned an error status")?;
        let mut sandboxes: Vec<E2bListedSandbox> = response
            .json()
            .await
            .context("decoding E2B sandbox list response")?;
        sandboxes.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(sandboxes.into_iter().next())
    }

    async fn create_sandbox(
        &self,
        request: &SandboxRequest,
        spec_hash: &str,
        template_id: &str,
    ) -> Result<E2bSandboxCreated> {
        let mut metadata = HashMap::new();
        metadata.insert(WARM_SANDBOX_KEY_LABEL.to_string(), request.key.to_string());
        metadata.insert(
            WARM_SANDBOX_SPEC_HASH_LABEL.to_string(),
            spec_hash.to_string(),
        );

        let (timeout_secs, auto_pause) = idle_ttl_to_e2b_lifecycle(&request.lifecycle.idle_ttl);

        let body = E2bCreateRequest {
            template_id: template_id.to_string(),
            timeout: timeout_secs,
            auto_pause,
            secure: self.secure,
            allow_internet_access: !matches!(request.spec.network, SandboxNetworkPolicy::Disabled),
            metadata,
        };

        let response = self
            .client
            .post(self.api_endpoint("/sandboxes"))
            .json(&body)
            .send()
            .await
            .context("creating E2B sandbox")?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("E2B create-sandbox failed ({status}): {text}");
        }
        let sandbox: E2bSandboxCreated = response
            .json()
            .await
            .context("decoding E2B create-sandbox response")?;
        Ok(sandbox)
    }

    async fn connect_sandbox(
        &self,
        sandbox_id: &str,
        timeout_secs: u32,
    ) -> Result<E2bSandboxCreated> {
        let body = E2bConnectRequest {
            timeout: timeout_secs,
        };
        let response = self
            .client
            .post(self.api_endpoint(&format!("/sandboxes/{sandbox_id}/connect")))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("connecting to E2B sandbox {sandbox_id}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("E2B connect-sandbox failed ({status}): {text}");
        }
        response
            .json()
            .await
            .context("decoding E2B connect-sandbox response")
    }

    fn handle_backend(&self) -> E2bBackendHandle {
        E2bBackendHandle {
            client: self.client.clone(),
            api_url: self.api_url.clone(),
            template_id: self.template_id.clone(),
            envd_port: self.envd_port,
            envd_base_url: self.envd_base_url.clone(),
            secure: self.secure,
        }
    }
}

#[async_trait]
impl ManagedSandboxBackend for E2bSandboxBackend {
    fn is_local(&self) -> bool {
        false
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &CONSUMABLE_SNAPSHOT_FORMATS
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_host_mounts(&request)?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        let key_label = request.key.to_string();
        let template_id = resolve_template_id(&request.spec, &self.template_id);

        if let Some(existing) = self
            .find_sandbox_by_metadata(&key_label, &spec_hash)
            .await?
        {
            let mut envd_access_token = None;
            if existing.state == "paused" {
                let timeout_secs = idle_ttl_to_e2b_lifecycle(&request.lifecycle.idle_ttl).0;
                let connected = self
                    .connect_sandbox(&existing.sandbox_id, timeout_secs)
                    .await?;
                envd_access_token = connected.envd_access_token;
            }
            return Ok(Arc::new(E2bSandboxHandle {
                id: format!("e2b:{}", request.key),
                sandbox_id: existing.sandbox_id,
                envd_access_token,
                request,
                backend: self.handle_backend(),
            }));
        }

        let sandbox = self
            .create_sandbox(&request, &spec_hash, &template_id)
            .await?;
        Ok(Arc::new(E2bSandboxHandle {
            id: format!("e2b:{}", request.key),
            sandbox_id: sandbox.sandbox_id,
            envd_access_token: sandbox.envd_access_token,
            request,
            backend: self.handle_backend(),
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("E2B sandbox backend does not support external attachments")
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_host_mounts(&request)?;
        if payload.format != SnapshotFormat::E2bRef {
            bail!(
                "E2B sandbox backend can only restore from {}, got {}",
                SnapshotFormat::E2bRef,
                payload.format
            );
        }
        let manifest: E2bSnapshotManifest =
            serde_json::from_slice(&payload.bytes).context("decoding E2bSnapshot manifest")?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        let sandbox = self
            .create_sandbox(&request, &spec_hash, &manifest.snapshot_id)
            .await?;
        Ok(Arc::new(E2bSandboxHandle {
            id: format!("e2b-restored:{}", request.key),
            sandbox_id: sandbox.sandbox_id,
            envd_access_token: sandbox.envd_access_token,
            request,
            backend: self.handle_backend(),
        }))
    }
}

#[derive(Clone)]
struct E2bBackendHandle {
    client: reqwest::Client,
    api_url: String,
    template_id: String,
    envd_port: u16,
    envd_base_url: Option<String>,
    secure: bool,
}

impl E2bBackendHandle {
    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    fn envd_endpoint(&self, sandbox_id: &str, path: &str) -> String {
        envd_endpoint_url(
            self.envd_base_url.as_deref(),
            self.envd_port,
            sandbox_id,
            path,
        )
    }
}

fn envd_endpoint_url(
    envd_base_url: Option<&str>,
    envd_port: u16,
    sandbox_id: &str,
    path: &str,
) -> String {
    if let Some(base) = envd_base_url {
        format!("{}{}", base.trim_end_matches('/'), path)
    } else {
        format!("https://{envd_port}-{sandbox_id}.e2b.app{path}")
    }
}

struct E2bSandboxHandle {
    id: String,
    sandbox_id: String,
    envd_access_token: Option<String>,
    request: SandboxRequest,
    backend: E2bBackendHandle,
}

#[async_trait]
impl ManagedSandboxHandle for E2bSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_in_sandbox(
            &self.backend,
            &self.sandbox_id,
            self.envd_access_token.as_deref(),
            &self.request.spec,
            command,
        )
        .await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        start_process_in_sandbox(
            &self.backend,
            &self.sandbox_id,
            self.envd_access_token.as_deref(),
            &self.request.spec,
            command,
        )
        .await
    }

    async fn stop(&self) -> Result<()> {
        pause_via_backend(&self.backend, &self.sandbox_id).await
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("E2B sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        let base_template = resolve_template_id(&self.request.spec, &self.backend.template_id);
        save_snapshot_via_backend(&self.backend, &self.sandbox_id, base_template).await
    }
}

async fn start_process_in_sandbox(
    backend: &E2bBackendHandle,
    sandbox_id: &str,
    envd_access_token: Option<&str>,
    spec: &SandboxSpec,
    command: &SandboxCommand,
) -> Result<crate::SandboxProcessParts> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }
    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());
    let (cmd, args) = command.argv.split_first().expect("checked non-empty");

    let (stdout_reader, stdout_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stderr_reader, stderr_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stdin_reader, stdin_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (wait_tx, wait_rx) = oneshot::channel();
    let (pid_tx, pid_rx) = oneshot::channel();
    let (stdin_error_tx, stdin_error_rx) = mpsc::unbounded_channel();

    let process = E2bRunningProcess {
        backend: backend.clone(),
        sandbox_id: sandbox_id.to_string(),
        envd_access_token: envd_access_token.map(str::to_string),
    };

    spawn_e2b_process_stream_poller(E2bProcessStreamPoller {
        process: process.clone(),
        cmd: cmd.to_string(),
        args: args.to_vec(),
        cwd,
        env: command.env.clone(),
        stdout_writer,
        stderr_writer,
        stdin_error_rx,
        pid_tx,
        wait_tx,
    });
    spawn_e2b_process_stdin_forwarder(process, stdin_reader, pid_rx, stdin_error_tx);

    let wait: BoxFuture<'static, crate::Result<i32>> = Box::pin(async move {
        match wait_rx.await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("E2B process stream stopped before reporting exit")),
        }
    });

    Ok(crate::SandboxProcessParts {
        stdout: Box::pin(stdout_reader.compat()),
        stderr: Box::pin(stderr_reader.compat()),
        stdin: Box::pin(stdin_writer.compat_write()),
        wait,
    })
}

async fn exec_in_sandbox(
    backend: &E2bBackendHandle,
    sandbox_id: &str,
    envd_access_token: Option<&str>,
    spec: &SandboxSpec,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());
    exec_command_in_sandbox(backend, sandbox_id, envd_access_token, cwd, command).await
}

async fn exec_command_in_sandbox(
    backend: &E2bBackendHandle,
    sandbox_id: &str,
    envd_access_token: Option<&str>,
    cwd: String,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }
    let (cmd, args) = command.argv.split_first().expect("checked non-empty");

    let start_request = serde_json::json!({
        "process": {
            "cmd": cmd,
            "args": args,
            "envs": command.env,
            "cwd": cwd,
        },
        "stdin": false,
    });
    let request_payload =
        serde_json::to_vec(&start_request).context("encoding E2B StartRequest")?;
    let request_body = connect_encode_envelope(0, &request_payload)?;

    let mut request = backend
        .client
        .post(backend.envd_endpoint(sandbox_id, "/process.Process/Start"))
        .header("Connect-Protocol-Version", "1")
        .header("Content-Type", "application/connect+json")
        .body(request_body);
    if backend.secure {
        let token = envd_access_token.ok_or_else(|| {
            anyhow!(
                "E2B sandbox {sandbox_id} requires envdAccessToken for command execution \
                 (create with secure: false via E2B_SECURE=0, or reconnect to refresh the token)"
            )
        })?;
        request = request.header("X-Access-Token", token);
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("exec in E2B sandbox {sandbox_id}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("reading E2B exec response")?;
    if !status.is_success() {
        bail!(
            "E2B exec failed ({status}): {}",
            String::from_utf8_lossy(&bytes)
        );
    }

    let (stdout, stderr, exit_code) = parse_connect_start_response(&bytes)?;
    Ok(SandboxCommandOutput {
        ok: exit_code == 0,
        exit_code: Some(exit_code),
        stdout,
        stderr,
        command: command
            .display_argv
            .clone()
            .unwrap_or_else(|| command.argv.clone()),
        cwd,
    })
}

#[derive(Clone)]
struct E2bRunningProcess {
    backend: E2bBackendHandle,
    sandbox_id: String,
    envd_access_token: Option<String>,
}

struct E2bProcessStreamPoller {
    process: E2bRunningProcess,
    cmd: String,
    args: Vec<String>,
    cwd: String,
    env: HashMap<String, String>,
    stdout_writer: tokio::io::DuplexStream,
    stderr_writer: tokio::io::DuplexStream,
    stdin_error_rx: mpsc::UnboundedReceiver<anyhow::Error>,
    pid_tx: oneshot::Sender<i32>,
    wait_tx: oneshot::Sender<crate::Result<i32>>,
}

fn spawn_e2b_process_stream_poller(poller: E2bProcessStreamPoller) {
    tokio::spawn(async move {
        let E2bProcessStreamPoller {
            process,
            cmd,
            args,
            cwd,
            env,
            stdout_writer,
            stderr_writer,
            mut stdin_error_rx,
            pid_tx,
            wait_tx,
        } = poller;
        let sandbox_id = process.sandbox_id.clone();
        let result = poll_e2b_process_stream(
            process,
            cmd,
            args,
            cwd,
            env,
            stdout_writer,
            stderr_writer,
            &mut stdin_error_rx,
            pid_tx,
        )
        .await;
        if wait_tx.send(result).is_err() {
            tracing::debug!(
                sandbox_id = %sandbox_id,
                "E2B process waiter dropped before completion"
            );
        }
    });
}

fn spawn_e2b_process_stdin_forwarder(
    process: E2bRunningProcess,
    stdin_reader: tokio::io::DuplexStream,
    pid_rx: oneshot::Receiver<i32>,
    stdin_error_tx: mpsc::UnboundedSender<anyhow::Error>,
) {
    tokio::spawn(async move {
        let sandbox_id = process.sandbox_id.clone();
        if let Err(error) = forward_e2b_process_stdin(process, stdin_reader, pid_rx).await {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                error = %error,
                "E2B process stdin forwarder stopped"
            );
            if stdin_error_tx.send(error).is_err() {
                tracing::debug!(
                    sandbox_id = %sandbox_id,
                    "E2B process stream stopped before stdin error could be reported"
                );
            }
        }
    });
}

async fn poll_e2b_process_stream(
    process: E2bRunningProcess,
    cmd: String,
    args: Vec<String>,
    cwd: String,
    env: HashMap<String, String>,
    mut stdout_writer: tokio::io::DuplexStream,
    mut stderr_writer: tokio::io::DuplexStream,
    stdin_error_rx: &mut mpsc::UnboundedReceiver<anyhow::Error>,
    pid_tx: oneshot::Sender<i32>,
) -> crate::Result<i32> {
    let start_request = serde_json::json!({
        "process": {
            "cmd": cmd,
            "args": args,
            "envs": env,
            "cwd": cwd,
        },
        "stdin": true,
    });
    let request_payload =
        serde_json::to_vec(&start_request).context("encoding E2B StartRequest")?;
    let request_body = connect_encode_envelope(0, &request_payload)?;

    let mut request = process
        .backend
        .client
        .post(
            process
                .backend
                .envd_endpoint(&process.sandbox_id, "/process.Process/Start"),
        )
        .header("Connect-Protocol-Version", "1")
        .header("Content-Type", "application/connect+json")
        .body(request_body);
    request = apply_envd_access_token(
        request,
        &process.backend,
        &process.sandbox_id,
        process.envd_access_token.as_deref(),
    )?;

    let response = request
        .send()
        .await
        .with_context(|| format!("starting E2B process in sandbox {}", process.sandbox_id))?;
    let status = response.status();
    if !status.is_success() {
        let bytes = response.bytes().await.unwrap_or_default();
        bail!(
            "E2B process start failed ({status}): {}",
            String::from_utf8_lossy(&bytes)
        );
    }

    let mut reader = ConnectEnvelopeReader::default();
    let mut pid_tx = Some(pid_tx);
    let mut exit_code = None;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if let Ok(error) = stdin_error_rx.try_recv() {
            return Err(error.context("E2B process stdin forwarding failed"));
        }
        let chunk = chunk.context("reading E2B process stream chunk")?;
        reader.push(&chunk);
        while let Some((flags, payload)) = reader.pop_envelope()? {
            if flags & CONNECT_FLAG_END_STREAM != 0 {
                let end: serde_json::Value = serde_json::from_slice(&payload)
                    .context("decoding Connect end-stream message")?;
                if let Some(err) = end.get("error") {
                    let code = err
                        .get("code")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let message = err
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    bail!("E2B process stream error ({code}): {message}");
                }
                continue;
            }

            let message: serde_json::Value =
                serde_json::from_slice(&payload).context("decoding Connect stream message")?;
            let Some(event) = message.get("event") else {
                continue;
            };
            if let Some(start) = event.get("start")
                && let Some(pid) = start.get("pid").and_then(|v| v.as_i64())
                && let Some(sender) = pid_tx.take()
                && sender.send(pid as i32).is_err()
            {
                tracing::debug!(
                    sandbox_id = %process.sandbox_id,
                    pid,
                    "E2B process stdin forwarder dropped before pid was delivered"
                );
            }
            if let Some(data) = event.get("data") {
                if let Some(chunk) = data.get("stdout").and_then(|v| v.as_str()) {
                    let decoded = decode_process_bytes(chunk);
                    stdout_writer
                        .write_all(decoded.as_bytes())
                        .await
                        .context("writing E2B process stdout pipe")?;
                }
                if let Some(chunk) = data.get("stderr").and_then(|v| v.as_str()) {
                    let decoded = decode_process_bytes(chunk);
                    stderr_writer
                        .write_all(decoded.as_bytes())
                        .await
                        .context("writing E2B process stderr pipe")?;
                }
            }
            if let Some(end) = event.get("end") {
                exit_code = Some(parse_exit_code(end));
            }
        }
    }

    exit_code.ok_or_else(|| anyhow!("E2B process stream ended without an exit event"))
}

async fn forward_e2b_process_stdin(
    process: E2bRunningProcess,
    mut stdin_reader: tokio::io::DuplexStream,
    pid_rx: oneshot::Receiver<i32>,
) -> Result<()> {
    let pid = pid_rx
        .await
        .map_err(|_| anyhow!("E2B process stream stopped before reporting pid"))?;
    let mut buffer = vec![0u8; PROCESS_PIPE_BUFFER_SIZE];
    loop {
        let bytes_read = stdin_reader
            .read(&mut buffer)
            .await
            .context("reading E2B process stdin pipe")?;
        if bytes_read == 0 {
            e2b_close_stdin(&process, pid).await?;
            return Ok(());
        }
        e2b_send_stdin(&process, pid, &buffer[..bytes_read]).await?;
    }
}

async fn e2b_send_stdin(process: &E2bRunningProcess, pid: i32, data: &[u8]) -> Result<()> {
    let body = serde_json::json!({
        "process": { "pid": pid },
        "input": { "stdin": BASE64.encode(data) },
    });
    let mut request = process
        .backend
        .client
        .post(
            process
                .backend
                .envd_endpoint(&process.sandbox_id, "/process.Process/SendInput"),
        )
        .header("Connect-Protocol-Version", "1")
        .header("Content-Type", "application/json")
        .json(&body);
    request = apply_envd_access_token(
        request,
        &process.backend,
        &process.sandbox_id,
        process.envd_access_token.as_deref(),
    )?;
    let response = request
        .send()
        .await
        .with_context(|| format!("sending stdin to E2B process {pid}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("E2B SendInput failed ({status}): {text}");
    }
    Ok(())
}

async fn e2b_close_stdin(process: &E2bRunningProcess, pid: i32) -> Result<()> {
    let body = serde_json::json!({
        "process": { "pid": pid },
    });
    let mut request = process
        .backend
        .client
        .post(
            process
                .backend
                .envd_endpoint(&process.sandbox_id, "/process.Process/CloseStdin"),
        )
        .header("Connect-Protocol-Version", "1")
        .header("Content-Type", "application/json")
        .json(&body);
    request = apply_envd_access_token(
        request,
        &process.backend,
        &process.sandbox_id,
        process.envd_access_token.as_deref(),
    )?;
    let response = request
        .send()
        .await
        .with_context(|| format!("closing stdin for E2B process {pid}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("E2B CloseStdin failed ({status}): {text}");
    }
    Ok(())
}

fn apply_envd_access_token(
    request: reqwest::RequestBuilder,
    backend: &E2bBackendHandle,
    sandbox_id: &str,
    envd_access_token: Option<&str>,
) -> Result<reqwest::RequestBuilder> {
    if !backend.secure {
        return Ok(request);
    }
    let token = envd_access_token.ok_or_else(|| {
        anyhow!(
            "E2B sandbox {sandbox_id} requires envdAccessToken for command execution \
             (create with secure: false via E2B_SECURE=0, or reconnect to refresh the token)"
        )
    })?;
    Ok(request.header("X-Access-Token", token))
}

#[derive(Default)]
struct ConnectEnvelopeReader {
    buffer: Vec<u8>,
}

impl ConnectEnvelopeReader {
    fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    fn pop_envelope(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        if self.buffer.len() < 5 {
            return Ok(None);
        }
        let len =
            u32::from_be_bytes(self.buffer[1..5].try_into().expect("four length bytes")) as usize;
        if len > CONNECT_MAX_ENVELOPE_BYTES {
            bail!("connect envelope length {len} exceeds max {CONNECT_MAX_ENVELOPE_BYTES}");
        }
        if self.buffer.len() < 5 + len {
            return Ok(None);
        }
        let flags = self.buffer[0];
        if flags & CONNECT_FLAG_COMPRESSED != 0 {
            bail!("compressed Connect envelopes are not supported");
        }
        let payload = self.buffer[5..5 + len].to_vec();
        self.buffer.drain(..5 + len);
        Ok(Some((flags, payload)))
    }
}

/// Wrap a JSON payload in a Connect binary envelope (1-byte flags + 4-byte BE length).
fn connect_encode_envelope(flags: u8, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > CONNECT_MAX_ENVELOPE_BYTES {
        bail!(
            "connect envelope payload is {} bytes (max {CONNECT_MAX_ENVELOPE_BYTES})",
            payload.len()
        );
    }
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

fn connect_decode_envelopes(bytes: &[u8]) -> Result<Vec<(u8, Vec<u8>)>> {
    let mut remaining = bytes;
    let mut envelopes = Vec::new();
    while !remaining.is_empty() {
        if remaining.len() < 5 {
            bail!("truncated Connect envelope header");
        }
        let flags = remaining[0];
        let len =
            u32::from_be_bytes(remaining[1..5].try_into().expect("four length bytes")) as usize;
        remaining = &remaining[5..];
        if len > CONNECT_MAX_ENVELOPE_BYTES {
            bail!("connect envelope length {len} exceeds max {CONNECT_MAX_ENVELOPE_BYTES}");
        }
        if remaining.len() < len {
            bail!(
                "truncated Connect envelope payload: expected {len} bytes, got {}",
                remaining.len()
            );
        }
        let payload = remaining[..len].to_vec();
        remaining = &remaining[len..];
        if flags & CONNECT_FLAG_COMPRESSED != 0 {
            bail!("compressed Connect envelopes are not supported");
        }
        envelopes.push((flags, payload));
    }
    Ok(envelopes)
}

/// Parse a Connect server-streaming `application/connect+json` body into stdout/stderr/exit.
fn parse_connect_start_response(bytes: &[u8]) -> Result<(String, String, i32)> {
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = 0i32;

    for (flags, payload) in connect_decode_envelopes(bytes)? {
        if flags & CONNECT_FLAG_END_STREAM != 0 {
            let end: serde_json::Value =
                serde_json::from_slice(&payload).context("decoding Connect end-stream message")?;
            if let Some(err) = end.get("error") {
                let code = err
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                bail!("E2B process stream error ({code}): {message}");
            }
            continue;
        }

        let message: serde_json::Value =
            serde_json::from_slice(&payload).context("decoding Connect stream message")?;
        let Some(event) = message.get("event") else {
            continue;
        };
        if let Some(data) = event.get("data") {
            if let Some(chunk) = data.get("stdout").and_then(|v| v.as_str()) {
                stdout.push_str(&decode_process_bytes(chunk));
            }
            if let Some(chunk) = data.get("stderr").and_then(|v| v.as_str()) {
                stderr.push_str(&decode_process_bytes(chunk));
            }
        }
        if let Some(end) = event.get("end") {
            exit_code = parse_exit_code(end);
        }
    }

    Ok((stdout, stderr, exit_code))
}

#[cfg(test)]
mod connect_tests {
    use super::*;

    #[test]
    fn round_trip_envelope_header() {
        let payload = br#"{"event":{"data":{"stdout":"hi"}}}"#;
        let encoded = connect_encode_envelope(0, payload).unwrap();
        let decoded = connect_decode_envelopes(&encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].0, 0);
        assert_eq!(decoded[0].1, payload);
    }

    #[test]
    fn decode_process_bytes_handles_base64_stdout() {
        assert_eq!(
            super::decode_process_bytes("ZXhvLWUyYi1saXZlCg=="),
            "exo-e2b-live\n"
        );
    }
}

/// E2B envd encodes stdout/stderr chunks as standard base64 in Connect JSON events.
fn decode_process_bytes(raw: &str) -> String {
    if let Ok(decoded) = BASE64.decode(raw.trim().as_bytes())
        && let Ok(text) = String::from_utf8(decoded)
    {
        return text;
    }
    raw.to_string()
}

fn parse_exit_code(end: &serde_json::Value) -> i32 {
    if let Some(code) = end.get("exitCode").and_then(|v| v.as_i64()) {
        return code as i32;
    }
    if let Some(status) = end.get("status").and_then(|v| v.as_str())
        && let Some(rest) = status.strip_prefix("exit status ")
        && let Ok(code) = rest.trim().parse::<i32>()
    {
        return code;
    }
    0
}

async fn pause_via_backend(backend: &E2bBackendHandle, sandbox_id: &str) -> Result<()> {
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/sandboxes/{sandbox_id}/pause")))
        .send()
        .await
        .with_context(|| format!("pausing E2B sandbox {sandbox_id}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("E2B pause-sandbox failed ({status}): {text}");
    }
    Ok(())
}

async fn save_snapshot_via_backend(
    backend: &E2bBackendHandle,
    sandbox_id: &str,
    base_template: String,
) -> Result<SnapshotPayload> {
    let snapshot_name = format!("exo-snap-{}", Uuid::new_v4().simple());
    let body = E2bSnapshotCreateRequest {
        name: Some(snapshot_name),
    };
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/sandboxes/{sandbox_id}/snapshots")))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("snapshotting E2B sandbox {sandbox_id}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("E2B snapshot failed ({status}): {text}");
    }
    let info: E2bSnapshotInfo = response
        .json()
        .await
        .context("decoding E2B snapshot response")?;
    let manifest = E2bSnapshotManifest {
        snapshot_id: info.snapshot_id,
        base_template,
    };
    let bytes = serde_json::to_vec(&manifest).context("serializing E2bSnapshot manifest")?;
    Ok(SnapshotPayload {
        format: SnapshotFormat::E2bRef,
        bytes: Bytes::from(bytes),
    })
}

fn reject_host_mounts(request: &SandboxRequest) -> Result<()> {
    if request.spec.mounts.is_empty() {
        return Ok(());
    }
    bail!(
        "E2B sandbox backend does not support host bind-mounts; \
         remove conversation mounts or use a local sandbox provider"
    )
}

fn resolve_template_id(spec: &SandboxSpec, default_template: &str) -> String {
    if spec.image.trim().is_empty() {
        if default_template.is_empty() {
            DEFAULT_SANDBOX_IMAGE.to_string()
        } else {
            default_template.to_string()
        }
    } else {
        spec.image.clone()
    }
}

/// Builds the `metadata` query value for `GET /v2/sandboxes`.
///
/// E2B expects `key=value&key2=value2` with URL encoding applied once at the
/// HTTP layer. Pre-encoding here and then passing through reqwest's `.query()`
/// double-encodes colons (`%253A`), so list filters never match created sandboxes.
fn metadata_filter_query(key_label: &str, spec_hash: &str) -> String {
    format!(
        "{}={}&{}={}",
        WARM_SANDBOX_KEY_LABEL, key_label, WARM_SANDBOX_SPEC_HASH_LABEL, spec_hash,
    )
}

fn idle_ttl_to_e2b_lifecycle(idle_ttl: &Option<Duration>) -> (u32, bool) {
    let Some(ttl) = idle_ttl else {
        return (300, false);
    };
    let secs = ttl.as_secs().clamp(1, u32::MAX as u64) as u32;
    (secs, true)
}

#[derive(Debug, Serialize)]
struct E2bCreateRequest {
    #[serde(rename = "templateID")]
    template_id: String,
    timeout: u32,
    #[serde(rename = "autoPause")]
    auto_pause: bool,
    secure: bool,
    allow_internet_access: bool,
    metadata: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct E2bConnectRequest {
    timeout: u32,
}

#[derive(Debug, Serialize)]
struct E2bSnapshotCreateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct E2bSandboxCreated {
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
    #[serde(default, rename = "envdAccessToken")]
    envd_access_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct E2bListedSandbox {
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
    state: String,
    #[serde(default, rename = "startedAt")]
    started_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct E2bSnapshotInfo {
    #[serde(rename = "snapshotID")]
    snapshot_id: String,
}

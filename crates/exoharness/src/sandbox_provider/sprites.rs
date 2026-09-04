//! Sprites.dev remote sandbox backend.
//!
//! Uses the Sprites platform REST API (`api.sprites.dev`) for lifecycle, HTTP
//! exec for one-shot commands, WebSocket exec for streaming processes, and
//! checkpoint/restore snapshots. Cross-process resume uses a
//! deterministic sprite name derived from [`SandboxKey`] + spec hash (same role
//! as Docker labels / E2B metadata). Snapshots are bytes-by-reference via
//! [`SnapshotFormat::SpritesRef`] manifests pointing at a checkpoint id.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use url::Url;

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxRequest, SandboxSpec, SnapshotFormat, SnapshotPayload, WARM_SANDBOX_KEY_LABEL,
    WARM_SANDBOX_SPEC_HASH_LABEL, sandbox_spec_hash,
};
use crate::{
    EgressCapabilities, EgressPolicy, SandboxAttachment, validate_egress_policy_capabilities,
};

pub const DEFAULT_SPRITES_API_URL: &str = "https://api.sprites.dev";

const PROCESS_PIPE_BUFFER_SIZE: usize = 64 * 1024;
const SPRITES_STREAM_STDIN: u8 = 0;
const SPRITES_STREAM_STDOUT: u8 = 1;
const SPRITES_STREAM_STDERR: u8 = 2;
const SPRITES_STREAM_EXIT: u8 = 3;
const SPRITES_STREAM_STDIN_EOF: u8 = 4;
static CONSUMABLE_SNAPSHOT_FORMATS: [SnapshotFormat; 1] = [SnapshotFormat::SpritesRef];

#[derive(Debug, Clone)]
pub struct SpritesConfig {
    pub token: String,
    pub api_url: String,
    /// Sprite HTTP URL auth: `sprite` or `public`. `None` lets Sprites default to `sprite`.
    pub url_auth: Option<String>,
    /// Organization slug for multi-org tokens.
    pub organization: Option<String>,
    /// Binding-level labels; exo resume labels are always merged in on create.
    pub extra_labels: Vec<String>,
}

/// JSON persisted for [`SnapshotFormat::SpritesRef`]. Filesystem state lives on
/// the sprite; we only store the checkpoint id and sprite name.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpritesSnapshotManifest {
    checkpoint_id: String,
    sprite_name: String,
}

pub struct SpritesSandboxBackend {
    client: reqwest::Client,
    api_url: String,
    token: String,
    url_auth: Option<String>,
    organization: Option<String>,
    extra_labels: Vec<String>,
}

impl SpritesSandboxBackend {
    pub fn new(config: SpritesConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", config.token))
            .context("SPRITES_TOKEN contains characters that aren't valid in an HTTP header")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("building Sprites HTTP client")?;
        if let Some(url_auth) = config.url_auth.as_deref() {
            validate_url_auth(url_auth)?;
        }
        Ok(Self {
            client,
            api_url: config.api_url.trim_end_matches('/').to_string(),
            token: config.token,
            url_auth: config.url_auth,
            organization: config.organization,
            extra_labels: config.extra_labels,
        })
    }

    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    async fn get_sprite(&self, name: &str) -> Result<Option<()>> {
        let response = self
            .client
            .get(self.api_endpoint(&format!("/v1/sprites/{name}")))
            .send()
            .await
            .with_context(|| format!("fetching Sprites sprite {name}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("Sprites get-sprite failed ({status}): {text}");
        }
        Ok(Some(()))
    }

    async fn create_sprite(&self, name: &str, request: &SandboxRequest) -> Result<()> {
        let spec_hash = sandbox_spec_hash(&request.spec);
        let labels = sprite_labels_for_request(request, &spec_hash, &self.extra_labels);
        let body = SpritesCreateRequest {
            name: name.to_string(),
            organization: self.organization.clone(),
            url_settings: self
                .url_auth
                .as_ref()
                .map(|auth| SpritesUrlSettings { auth: auth.clone() }),
            labels,
        };
        let response = self
            .client
            .post(self.api_endpoint("/v1/sprites"))
            .json(&body)
            .send()
            .await
            .context("creating Sprites sprite")?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::CONFLICT
            || (status == reqwest::StatusCode::BAD_REQUEST && text.contains("exists"))
        {
            return Ok(());
        }
        bail!("Sprites create-sprite failed ({status}): {text}");
    }

    async fn ensure_sprite(&self, name: &str, request: &SandboxRequest) -> Result<()> {
        if self.get_sprite(name).await?.is_some() {
            return Ok(());
        }
        self.create_sprite(name, request).await
    }

    fn handle_backend(&self) -> SpritesBackendHandle {
        SpritesBackendHandle {
            client: self.client.clone(),
            api_url: self.api_url.clone(),
            token: self.token.clone(),
        }
    }
}

#[async_trait]
impl ManagedSandboxBackend for SpritesSandboxBackend {
    fn is_local(&self) -> bool {
        false
    }

    fn egress_capabilities(&self) -> EgressCapabilities {
        EgressCapabilities::default()
    }

    fn validate_egress_policy(&self, policy: &EgressPolicy) -> Result<()> {
        validate_egress_policy_capabilities(policy, self.egress_capabilities())
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &CONSUMABLE_SNAPSHOT_FORMATS
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        self.validate_egress_policy(&request.spec.egress_policy)?;
        reject_host_mounts(&request)?;
        let sprite_name = sprite_name_for_request(&request);
        self.ensure_sprite(&sprite_name, &request).await?;
        Ok(Arc::new(SpritesSandboxHandle {
            id: format!("sprites:{}", request.key),
            sprite_name,
            request,
            backend: self.handle_backend(),
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Sprites sandbox backend does not support external attachments")
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_host_mounts(&request)?;
        if payload.format != SnapshotFormat::SpritesRef {
            bail!(
                "Sprites sandbox backend can only restore from {}, got {}",
                SnapshotFormat::SpritesRef,
                payload.format
            );
        }
        let manifest: SpritesSnapshotManifest =
            serde_json::from_slice(&payload.bytes).context("decoding SpritesSnapshot manifest")?;
        let sprite_name = sprite_name_for_request(&request);
        if manifest.sprite_name != sprite_name {
            bail!(
                "Sprites snapshot belongs to sprite {}, but this sandbox key maps to {}",
                manifest.sprite_name,
                sprite_name
            );
        }
        self.ensure_sprite(&sprite_name, &request).await?;
        restore_checkpoint_via_backend(
            &self.handle_backend(),
            &sprite_name,
            &manifest.checkpoint_id,
        )
        .await?;
        Ok(Arc::new(SpritesSandboxHandle {
            id: format!("sprites-restored:{}", request.key),
            sprite_name,
            request,
            backend: self.handle_backend(),
        }))
    }
}

#[derive(Clone)]
struct SpritesBackendHandle {
    client: reqwest::Client,
    api_url: String,
    token: String,
}

impl SpritesBackendHandle {
    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    fn websocket_endpoint(&self, path: &str) -> Result<Url> {
        let origin = if let Some(rest) = self.api_url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.api_url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            bail!(
                "Sprites api_url must be an http(s) URL, got {}",
                self.api_url
            );
        };
        Url::parse(&format!("{origin}{path}")).context("parsing Sprites WebSocket URL")
    }
}

struct SpritesSandboxHandle {
    id: String,
    sprite_name: String,
    request: SandboxRequest,
    backend: SpritesBackendHandle,
}

#[async_trait]
impl ManagedSandboxHandle for SpritesSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_in_sprite(
            &self.backend,
            &self.sprite_name,
            &self.request.spec,
            command,
        )
        .await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        start_process_in_sprite(
            &self.backend,
            &self.sprite_name,
            &self.request.spec,
            command,
        )
        .await
    }

    async fn stop(&self) -> Result<()> {
        // Sprites hibernate when idle; do not DELETE — the next session resumes the
        // same sprite by deterministic name via `acquire`.
        Ok(())
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("Sprites sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        save_checkpoint_via_backend(&self.backend, &self.sprite_name).await
    }
}

async fn start_process_in_sprite(
    backend: &SpritesBackendHandle,
    sprite_name: &str,
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

    let mut url = backend.websocket_endpoint(&format!("/v1/sprites/{sprite_name}/exec"))?;
    {
        let mut query = url.query_pairs_mut();
        for arg in &command.argv {
            query.append_pair("cmd", arg);
        }
        query.append_pair("stdin", "true");
        if !cwd.is_empty() {
            query.append_pair("dir", &cwd);
        }
        for (key, value) in &command.env {
            query.append_pair("env", &format!("{key}={value}"));
        }
    }

    let mut request = url
        .as_str()
        .into_client_request()
        .context("building Sprites exec WebSocket request")?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", backend.token))
        .context("SPRITES_TOKEN contains characters that aren't valid in an HTTP header")?;
    request.headers_mut().insert(AUTHORIZATION, auth);

    let (ws_stream, _) = connect_async(request)
        .await
        .with_context(|| format!("connecting Sprites exec WebSocket for sprite {sprite_name}"))?;

    let (stdout_reader, stdout_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stderr_reader, stderr_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stdin_reader, stdin_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (wait_tx, wait_rx) = oneshot::channel();

    let sprite_name_owned = sprite_name.to_string();
    tokio::spawn(async move {
        let result =
            run_sprites_exec_websocket(ws_stream, stdin_reader, stdout_writer, stderr_writer).await;
        if wait_tx.send(result).is_err() {
            tracing::debug!(
                sprite_name = %sprite_name_owned,
                "Sprites process waiter dropped before completion"
            );
        }
    });

    let wait: BoxFuture<'static, crate::Result<i32>> = Box::pin(async move {
        match wait_rx.await {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "Sprites exec WebSocket stopped before reporting exit"
            )),
        }
    });

    Ok(crate::SandboxProcessParts {
        stdout: Box::pin(stdout_reader.compat()),
        stderr: Box::pin(stderr_reader.compat()),
        stdin: Box::pin(stdin_writer.compat_write()),
        wait,
    })
}

async fn run_sprites_exec_websocket(
    ws_stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    mut stdin_reader: tokio::io::DuplexStream,
    mut stdout_writer: tokio::io::DuplexStream,
    mut stderr_writer: tokio::io::DuplexStream,
) -> crate::Result<i32> {
    let (mut ws_write, mut ws_read) = ws_stream.split();
    let mut exit_code = None;
    let mut stdin_buffer = vec![0u8; PROCESS_PIPE_BUFFER_SIZE];
    let mut stdin_closed = false;

    loop {
        if exit_code.is_some() {
            break;
        }

        tokio::select! {
            message = ws_read.next() => {
                let Some(message) = message else {
                    break;
                };
                let message = message.context("reading Sprites exec WebSocket message")?;
                match message {
                    Message::Binary(data) => {
                        if data.is_empty() {
                            continue;
                        }
                        let stream_id = data[0];
                        let payload = &data[1..];
                        match stream_id {
                            SPRITES_STREAM_STDOUT => {
                                stdout_writer
                                    .write_all(payload)
                                    .await
                                    .context("writing Sprites process stdout pipe")?;
                            }
                            SPRITES_STREAM_STDERR => {
                                stderr_writer
                                    .write_all(payload)
                                    .await
                                    .context("writing Sprites process stderr pipe")?;
                            }
                            SPRITES_STREAM_EXIT => {
                                exit_code = Some(parse_sprites_binary_exit_code(payload));
                            }
                            _ => {}
                        }
                    }
                    Message::Text(text) => {
                        if let Ok(event) = serde_json::from_str::<SpritesExecJsonMessage>(&text)
                            && event.message_type == "exit"
                            && let Some(code) = event.exit_code
                        {
                            exit_code = Some(code);
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(payload) => {
                        ws_write
                            .send(Message::Pong(payload))
                            .await
                            .context("responding to Sprites exec WebSocket ping")?;
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            read_result = stdin_reader.read(&mut stdin_buffer), if !stdin_closed => {
                let bytes_read = read_result.context("reading Sprites process stdin pipe")?;
                if bytes_read == 0 {
                    ws_write
                        .send(Message::Binary(Bytes::from_static(&[
                            SPRITES_STREAM_STDIN_EOF,
                        ])))
                        .await
                        .context("sending Sprites stdin EOF")?;
                    stdin_closed = true;
                } else {
                    let mut frame = Vec::with_capacity(1 + bytes_read);
                    frame.push(SPRITES_STREAM_STDIN);
                    frame.extend_from_slice(&stdin_buffer[..bytes_read]);
                    ws_write
                        .send(Message::Binary(Bytes::from(frame)))
                        .await
                        .context("sending Sprites process stdin")?;
                }
            }
        }
    }

    exit_code.ok_or_else(|| anyhow!("Sprites exec WebSocket closed without an exit event"))
}

fn parse_sprites_binary_exit_code(payload: &[u8]) -> i32 {
    if payload.is_empty() {
        return 0;
    }
    if payload.len() == 1 {
        return payload[0] as i32;
    }
    if let Ok(text) = std::str::from_utf8(payload)
        && let Ok(code) = text.trim().parse::<i32>()
    {
        return code;
    }
    payload.last().copied().unwrap_or(0) as i32
}

#[derive(Debug, Deserialize)]
struct SpritesExecJsonMessage {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(rename = "exit_code")]
    exit_code: Option<i32>,
}

async fn exec_in_sprite(
    backend: &SpritesBackendHandle,
    sprite_name: &str,
    spec: &SandboxSpec,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());
    exec_command_in_sprite(backend, sprite_name, cwd, command).await
}

async fn exec_command_in_sprite(
    backend: &SpritesBackendHandle,
    sprite_name: &str,
    cwd: String,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }

    let mut query: Vec<(&str, String)> = Vec::new();
    for arg in &command.argv {
        query.push(("cmd", arg.clone()));
    }
    query.push(("stdin", "false".to_string()));
    if !cwd.is_empty() {
        query.push(("dir", cwd.clone()));
    }
    for (key, value) in &command.env {
        query.push(("env", format!("{key}={value}")));
    }

    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/v1/sprites/{sprite_name}/exec")))
        .query(&query)
        .send()
        .await
        .with_context(|| format!("exec in Sprites sprite {sprite_name}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("reading Sprites exec response body")?;
    if !status.is_success() {
        bail!("Sprites exec failed ({status}): {body}");
    }

    let (exit_code, stdout, stderr) = parse_exec_response(&body)?;
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

fn parse_exec_response(body: &str) -> Result<(i32, String, String)> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok((0, String::new(), String::new()));
    }

    // Some deployments return a single JSON object; others stream NDJSON or raw stdout.
    if trimmed.starts_with('{')
        && !trimmed.contains('\n')
        && let Ok(parsed) = serde_json::from_str::<SpritesHttpExecResponse>(trimmed)
    {
        return Ok(parsed.into_parts());
    }

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = None;
    let mut parsed_stream_event = false;
    let mut raw_lines = String::new();

    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<SpritesStreamEvent>(line) else {
            raw_lines.push_str(line);
            raw_lines.push('\n');
            continue;
        };
        parsed_stream_event = true;
        match event.event_type.as_str() {
            "stdout" => {
                if let Some(data) = event.data {
                    stdout.push_str(&data);
                }
            }
            "stderr" => {
                if let Some(data) = event.data {
                    stderr.push_str(&data);
                }
            }
            "complete" | "exit" => {
                if let Some(code) = event.exit_code {
                    exit_code = Some(code);
                }
            }
            "error" => {
                let message = event
                    .error
                    .or(event.message)
                    .unwrap_or_else(|| "Sprites exec stream error".into());
                bail!("{message}");
            }
            _ => {}
        }
    }

    if parsed_stream_event {
        return Ok((exit_code.unwrap_or(0), stdout, stderr));
    }

    if !raw_lines.is_empty() {
        return Ok((exit_code.unwrap_or(0), raw_lines, stderr));
    }

    // Plain-text body (common for simple HTTP exec, e.g. `curl ... | python`).
    Ok((exit_code.unwrap_or(0), trimmed.to_string(), stderr))
}

async fn save_checkpoint_via_backend(
    backend: &SpritesBackendHandle,
    sprite_name: &str,
) -> Result<SnapshotPayload> {
    let body = serde_json::json!({ "comment": "exo snapshot" });
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/v1/sprites/{sprite_name}/checkpoint")))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("creating Sprites checkpoint for {sprite_name}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("reading Sprites checkpoint response")?;
    if !status.is_success() {
        bail!("Sprites checkpoint failed ({status}): {text}");
    }
    let checkpoint_id = parse_checkpoint_id_from_stream(&text)?;
    let manifest = SpritesSnapshotManifest {
        checkpoint_id,
        sprite_name: sprite_name.to_string(),
    };
    let bytes = serde_json::to_vec(&manifest).context("serializing Sprites snapshot manifest")?;
    Ok(SnapshotPayload {
        format: SnapshotFormat::SpritesRef,
        bytes: Bytes::from(bytes),
    })
}

async fn restore_checkpoint_via_backend(
    backend: &SpritesBackendHandle,
    sprite_name: &str,
    checkpoint_id: &str,
) -> Result<()> {
    let response = backend
        .client
        .post(backend.api_endpoint(&format!(
            "/v1/sprites/{sprite_name}/checkpoints/{checkpoint_id}/restore"
        )))
        .send()
        .await
        .with_context(|| {
            format!("restoring Sprites checkpoint {checkpoint_id} on {sprite_name}")
        })?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("reading Sprites restore response")?;
    if !status.is_success() {
        bail!("Sprites checkpoint restore failed ({status}): {text}");
    }
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: SpritesStreamEvent =
            serde_json::from_str(line).context("decoding Sprites restore NDJSON line")?;
        if event.event_type == "error" {
            let message = event
                .error
                .or(event.message)
                .unwrap_or_else(|| "Sprites restore stream error".into());
            bail!("{message}");
        }
    }
    Ok(())
}

fn parse_checkpoint_id_from_stream(body: &str) -> Result<String> {
    let mut checkpoint_id = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: SpritesStreamEvent =
            serde_json::from_str(line).context("decoding Sprites checkpoint NDJSON line")?;
        if event.event_type == "error" {
            let message = event
                .error
                .or(event.message)
                .unwrap_or_else(|| "Sprites checkpoint stream error".into());
            bail!("{message}");
        }
        if let Some(data) = event.data {
            if let Some(id) = data.strip_prefix("  ID: ") {
                checkpoint_id = Some(id.trim().to_string());
            }
            if let Some(rest) = data.strip_prefix("Checkpoint ")
                && let Some(id) = rest.split_whitespace().next()
            {
                checkpoint_id = Some(id.to_string());
            }
        }
    }
    checkpoint_id.ok_or_else(|| anyhow!("Sprites checkpoint stream did not report a checkpoint id"))
}

/// Deterministic sprite name for a sandbox key + spec. Sprites names must be unique
/// per organization; hashing the exo key and spec hash gives stable cross-process resume.
fn sprite_name_for_request(request: &SandboxRequest) -> String {
    let spec_hash = sandbox_spec_hash(&request.spec);
    let mut hasher = DefaultHasher::new();
    request.key.hash(&mut hasher);
    spec_hash.hash(&mut hasher);
    format!("exo-{:016x}", hasher.finish())
}

fn reject_host_mounts(request: &SandboxRequest) -> Result<()> {
    if request.spec.mounts.is_empty() {
        return Ok(());
    }
    bail!(
        "Sprites sandbox backend does not support host bind-mounts; \
         remove conversation mounts or use a local sandbox provider"
    )
}

fn validate_url_auth(url_auth: &str) -> Result<()> {
    match url_auth {
        "sprite" | "public" => Ok(()),
        other => bail!("Sprites url_auth must be `sprite` or `public`, got {other:?}"),
    }
}

fn sprite_label(key: &str, value: &str) -> String {
    format!("{key}={value}")
}

fn sprite_labels_for_request(
    request: &SandboxRequest,
    spec_hash: &str,
    extra_labels: &[String],
) -> Vec<String> {
    let mut labels = Vec::with_capacity(extra_labels.len() + 2);
    labels.push(sprite_label(
        WARM_SANDBOX_KEY_LABEL,
        &request.key.to_string(),
    ));
    labels.push(sprite_label(WARM_SANDBOX_SPEC_HASH_LABEL, spec_hash));
    labels.extend(extra_labels.iter().cloned());
    labels
}

#[derive(Debug, Serialize)]
struct SpritesUrlSettings {
    auth: String,
}

#[derive(Debug, Serialize)]
struct SpritesCreateRequest {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    organization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url_settings: Option<SpritesUrlSettings>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SpritesHttpExecResponse {
    #[serde(default, alias = "exitCode")]
    exit_code: Option<i32>,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    output: String,
}

impl SpritesHttpExecResponse {
    fn into_parts(self) -> (i32, String, String) {
        let exit_code = self.exit_code.unwrap_or(0);
        let stdout = if self.stdout.is_empty() {
            self.output
        } else {
            self.stdout
        };
        (exit_code, stdout, self.stderr)
    }
}

#[derive(Debug, Deserialize)]
struct SpritesStreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, alias = "exitCode")]
    exit_code: Option<i32>,
}

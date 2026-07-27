//! Tensorlake remote sandbox backend.
//!
//! Lifecycle goes through the Tensorlake platform API (`api.tensorlake.ai`):
//! create, get, suspend, resume, delete, snapshot. Commands run against the
//! per-sandbox proxy (`sandbox_url`, e.g. `https://<id>.sandbox.tensorlake.ai`)
//! using the sandbox process API — `/api/v1/processes/run` for one-shot exec and
//! `/api/v1/processes` + stdin/follow endpoints for streaming processes.
//!
//! Cross-process resume uses a deterministic *named* sandbox derived from
//! [`SandboxKey`](crate::SandboxKey) + spec hash (same role as Docker labels /
//! E2B metadata); Tensorlake creates have no label field, and only named
//! sandboxes support suspend/resume. A request with no `idle_ttl` gets an
//! ephemeral sandbox instead, terminated on `stop`.
//!
//! Snapshots are bytes-by-reference via [`SnapshotKind::TensorlakeSnapshot`]
//! manifests pointing at a platform snapshot id.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{Stream, StreamExt};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotKind, SnapshotPayload,
    sandbox_spec_hash,
};

pub const DEFAULT_TENSORLAKE_API_URL: &str = "https://api.tensorlake.ai";
pub const DEFAULT_TENSORLAKE_IMAGE: &str = "tensorlake/ubuntu-minimal";

pub fn default_tensorlake_image() -> String {
    DEFAULT_TENSORLAKE_IMAGE.to_string()
}

const PROCESS_PIPE_BUFFER_SIZE: usize = 64 * 1024;
/// How long to wait for a created/resumed sandbox to reach `running`.
const SANDBOX_READY_TIMEOUT: Duration = Duration::from_secs(180);
const SANDBOX_READY_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PROCESS_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Grace period for the output followers to drain after the process exits.
const PROCESS_OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Client-side backstop on top of the server-side `timeout` for one-shot exec.
const EXEC_TIMEOUT_GRACE: Duration = Duration::from_secs(10);
/// A just-terminated name can linger briefly before it is free to reuse.
const NAME_CONFLICT_RETRIES: usize = 5;
const NAME_CONFLICT_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct TensorlakeConfig {
    pub api_key: String,
    pub api_url: String,
    /// Base image for sandboxes whose spec doesn't request one.
    pub default_image: String,
    /// CPU cores per sandbox. `None` uses the Tensorlake default.
    pub cpus: Option<f64>,
    /// Memory per sandbox in MiB. `None` uses the Tensorlake default.
    pub memory_mb: Option<u64>,
    /// When set (tests), sandbox proxy requests go here instead of the
    /// `sandbox_url` the platform reports.
    pub sandbox_base_url: Option<String>,
}

/// JSON persisted for [`SnapshotKind::TensorlakeSnapshot`]. Filesystem state lives
/// in Tensorlake; we only store the snapshot id returned by
/// `POST /sandboxes/{id}/snapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TensorlakeSnapshotManifest {
    snapshot_id: String,
    /// The named sandbox the snapshot was taken from, when it wasn't ephemeral.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox_name: Option<String>,
}

pub struct TensorlakeSandboxBackend {
    client: reqwest::Client,
    api_url: String,
    default_image: String,
    cpus: Option<f64>,
    memory_mb: Option<u64>,
    sandbox_base_url: Option<String>,
}

impl TensorlakeSandboxBackend {
    pub fn new(config: TensorlakeConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", config.api_key)).context(
            "TENSORLAKE_API_KEY contains characters that aren't valid in an HTTP header",
        )?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("building Tensorlake HTTP client")?;
        Ok(Self {
            client,
            api_url: config.api_url.trim_end_matches('/').to_string(),
            default_image: config.default_image,
            cpus: config.cpus,
            memory_mb: config.memory_mb,
            sandbox_base_url: config
                .sandbox_base_url
                .map(|url| url.trim_end_matches('/').to_string()),
        })
    }

    fn handle_backend(&self) -> TensorlakeBackendHandle {
        TensorlakeBackendHandle {
            client: self.client.clone(),
            api_url: self.api_url.clone(),
            sandbox_base_url: self.sandbox_base_url.clone(),
        }
    }

    fn create_body(&self, request: &SandboxRequest, name: Option<&str>) -> CreateSandboxBody {
        CreateSandboxBody {
            name: name.map(ToOwned::to_owned),
            image: Some(resolve_image(&request.spec, &self.default_image)),
            snapshot_id: None,
            resources: self.resource_overrides(),
            // Named sandboxes suspend at the idle timeout and resume on the next
            // `acquire`; ephemeral ones get the plan default and are deleted on stop.
            timeout_secs: request.lifecycle.idle_ttl.map(|ttl| ttl.as_secs().max(1)),
            network: Some(SandboxNetworkAccessControl {
                allow_internet_access: matches!(
                    request.spec.network,
                    SandboxNetworkPolicy::Enabled
                ),
            }),
        }
    }

    fn resource_overrides(&self) -> Option<SandboxResourceOverrides> {
        (self.cpus.is_some() || self.memory_mb.is_some()).then_some(SandboxResourceOverrides {
            cpus: self.cpus,
            memory_mb: self.memory_mb,
        })
    }
}

#[async_trait]
impl ManagedSandboxBackend for TensorlakeSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_unsupported_mounts(&request)?;
        let backend = self.handle_backend();
        let name = sandbox_name_for_request(&request);

        let target = match &name {
            Some(name) => match backend.get_sandbox(name).await? {
                Some(info) => backend.drive_to_running(info).await?,
                None => {
                    let body = self.create_body(&request, Some(name));
                    backend.create_and_wait(body).await?
                }
            },
            None => {
                let body = self.create_body(&request, None);
                backend.create_and_wait(body).await?
            }
        };

        Ok(Arc::new(TensorlakeSandboxHandle::new(
            format!("tensorlake:{}", request.key),
            name,
            target,
            request,
            backend,
        )))
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_unsupported_mounts(&request)?;
        if !matches!(payload.kind, SnapshotKind::TensorlakeSnapshot) {
            bail!(
                "Tensorlake sandbox backend can only restore from \
                 SnapshotKind::TensorlakeSnapshot, got {:?}",
                payload.kind
            );
        }
        let manifest: TensorlakeSnapshotManifest = serde_json::from_slice(&payload.bytes)
            .context("decoding TensorlakeSnapshot manifest")?;

        let backend = self.handle_backend();
        let name = sandbox_name_for_request(&request);

        // A restored sandbox must boot from the snapshot, so any sandbox already
        // holding this name is terminated first — mirrors the Docker backend
        // evicting its warm container before restoring an image.
        if let Some(name) = &name
            && backend.get_sandbox(name).await?.is_some()
        {
            backend.delete_sandbox(name).await?;
        }

        let mut body = self.create_body(&request, name.as_deref());
        body.image = None;
        body.snapshot_id = Some(manifest.snapshot_id);
        let target = backend.create_and_wait(body).await?;

        Ok(Arc::new(TensorlakeSandboxHandle::new(
            format!("tensorlake-restored:{}", request.key),
            name,
            target,
            request,
            backend,
        )))
    }
}

/// A sandbox that is running and reachable: its platform id plus the proxy base
/// URL its commands go to.
#[derive(Debug, Clone)]
struct TensorlakeSandboxTarget {
    sandbox_id: String,
    base_url: String,
}

#[derive(Clone)]
struct TensorlakeBackendHandle {
    client: reqwest::Client,
    api_url: String,
    sandbox_base_url: Option<String>,
}

impl TensorlakeBackendHandle {
    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    async fn get_sandbox(&self, identifier: &str) -> Result<Option<SandboxInfo>> {
        let response = self
            .client
            .get(self.api_endpoint(&format!("/sandboxes/{identifier}")))
            .send()
            .await
            .with_context(|| format!("fetching Tensorlake sandbox {identifier}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let info: SandboxInfo = decode_json_response(response, "Tensorlake get-sandbox").await?;
        // A terminated sandbox is a tombstone: the name is free to recreate.
        if matches!(info.status, SandboxStatus::Terminated) {
            return Ok(None);
        }
        Ok(Some(info))
    }

    async fn create_and_wait(&self, body: CreateSandboxBody) -> Result<TensorlakeSandboxTarget> {
        let created = self.create_sandbox(&body).await?;
        self.wait_until_running(&created.sandbox_id).await
    }

    async fn create_sandbox(&self, body: &CreateSandboxBody) -> Result<CreateSandboxResponse> {
        let mut last_conflict = None;
        for attempt in 0..NAME_CONFLICT_RETRIES {
            let response = self
                .client
                .post(self.api_endpoint("/sandboxes"))
                .json(body)
                .send()
                .await
                .context("creating Tensorlake sandbox")?;
            if response.status() != reqwest::StatusCode::CONFLICT {
                return decode_json_response(response, "Tensorlake create-sandbox").await;
            }
            last_conflict = Some(response.text().await.unwrap_or_default());
            if attempt + 1 < NAME_CONFLICT_RETRIES {
                time::sleep(NAME_CONFLICT_RETRY_DELAY).await;
            }
        }
        bail!(
            "Tensorlake create-sandbox kept reporting a name conflict for {:?}: {}",
            body.name,
            last_conflict.unwrap_or_default()
        )
    }

    async fn delete_sandbox(&self, identifier: &str) -> Result<()> {
        let response = self
            .client
            .delete(self.api_endpoint(&format!("/sandboxes/{identifier}")))
            .send()
            .await
            .with_context(|| format!("deleting Tensorlake sandbox {identifier}"))?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Tensorlake delete-sandbox failed ({status}): {text}")
    }

    async fn suspend_sandbox(&self, identifier: &str) -> Result<()> {
        let response = self
            .client
            .post(self.api_endpoint(&format!("/sandboxes/{identifier}/suspend")))
            .send()
            .await
            .with_context(|| format!("suspending Tensorlake sandbox {identifier}"))?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Tensorlake suspend-sandbox failed ({status}): {text}")
    }

    async fn resume_sandbox(&self, identifier: &str) -> Result<()> {
        let response = self
            .client
            .post(self.api_endpoint(&format!("/sandboxes/{identifier}/resume")))
            .send()
            .await
            .with_context(|| format!("resuming Tensorlake sandbox {identifier}"))?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Tensorlake resume-sandbox failed ({status}): {text}")
    }

    /// Bring an existing sandbox back to `running`, resuming it if it suspended.
    async fn drive_to_running(&self, info: SandboxInfo) -> Result<TensorlakeSandboxTarget> {
        match info.status {
            SandboxStatus::Running => self.target_for(&info),
            SandboxStatus::Suspended => self.resume_and_wait(&info.id).await,
            // A resume is rejected while a suspend is still in flight, so
            // `suspending` waits like any other transitional state; the poll
            // loop fires the resume once it settles.
            SandboxStatus::Suspending | SandboxStatus::Pending | SandboxStatus::Snapshotting => {
                self.wait_until_running(&info.id).await
            }
            SandboxStatus::Terminated => bail!(
                "Tensorlake sandbox {} is terminated and cannot be reused",
                info.id
            ),
        }
    }

    async fn wait_until_running(&self, sandbox_id: &str) -> Result<TensorlakeSandboxTarget> {
        self.poll_until_running(sandbox_id, false).await
    }

    async fn resume_and_wait(&self, sandbox_id: &str) -> Result<TensorlakeSandboxTarget> {
        self.resume_sandbox(sandbox_id).await?;
        self.poll_until_running(sandbox_id, true).await
    }

    /// Polls until the sandbox reports `running`. A sandbox that settles into
    /// `suspended` along the way is resumed exactly once — resume is not
    /// idempotent enough to re-send on every tick, and the API rejects it
    /// outright in the intermediate states.
    async fn poll_until_running(
        &self,
        sandbox_id: &str,
        mut resumed: bool,
    ) -> Result<TensorlakeSandboxTarget> {
        let deadline = time::Instant::now() + SANDBOX_READY_TIMEOUT;
        loop {
            let response = self
                .client
                .get(self.api_endpoint(&format!("/sandboxes/{sandbox_id}")))
                .send()
                .await
                .with_context(|| format!("polling Tensorlake sandbox {sandbox_id}"))?;
            let info: SandboxInfo =
                decode_json_response(response, "Tensorlake get-sandbox").await?;
            match info.status {
                SandboxStatus::Running => return self.target_for(&info),
                SandboxStatus::Terminated => bail!(
                    "Tensorlake sandbox {sandbox_id} terminated while starting{}",
                    info.outcome
                        .as_deref()
                        .map(|outcome| format!(": {outcome}"))
                        .unwrap_or_default()
                ),
                SandboxStatus::Suspended if !resumed => {
                    resumed = true;
                    self.resume_sandbox(sandbox_id).await?;
                }
                SandboxStatus::Suspended
                | SandboxStatus::Suspending
                | SandboxStatus::Pending
                | SandboxStatus::Snapshotting => {}
            }
            if time::Instant::now() >= deadline {
                bail!(
                    "Tensorlake sandbox {sandbox_id} was still {:?} after {}s{}",
                    info.status,
                    SANDBOX_READY_TIMEOUT.as_secs(),
                    info.pending_reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                );
            }
            time::sleep(SANDBOX_READY_POLL_INTERVAL).await;
        }
    }

    fn target_for(&self, info: &SandboxInfo) -> Result<TensorlakeSandboxTarget> {
        let base_url = match &self.sandbox_base_url {
            Some(base_url) => base_url.clone(),
            None => info
                .sandbox_url
                .as_deref()
                .map(|url| url.trim_end_matches('/').to_string())
                .ok_or_else(|| {
                    anyhow!(
                        "Tensorlake sandbox {} is running but reported no sandbox_url",
                        info.id
                    )
                })?,
        };
        Ok(TensorlakeSandboxTarget {
            sandbox_id: info.id.clone(),
            base_url,
        })
    }

    async fn snapshot_sandbox(&self, sandbox_id: &str) -> Result<String> {
        let body = SnapshotSandboxBody {
            // Filesystem snapshots match what every other exo backend captures
            // (Docker image tar, Daytona/E2B filesystem snapshots).
            snapshot_type: "filesystem",
        };
        let response = self
            .client
            .post(self.api_endpoint(&format!("/sandboxes/{sandbox_id}/snapshot")))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("snapshotting Tensorlake sandbox {sandbox_id}"))?;
        let snapshot: SnapshotSandboxResponse =
            decode_json_response(response, "Tensorlake snapshot-sandbox").await?;
        Ok(snapshot.snapshot_id)
    }
}

struct TensorlakeSandboxHandle {
    id: String,
    /// `Some` for named (resumable) sandboxes, `None` for ephemeral ones.
    name: Option<String>,
    target: Mutex<TensorlakeSandboxTarget>,
    /// Cleared when a proxy call fails so the next call re-checks placement.
    target_is_live: AtomicBool,
    request: SandboxRequest,
    backend: TensorlakeBackendHandle,
}

impl TensorlakeSandboxHandle {
    fn new(
        id: String,
        name: Option<String>,
        target: TensorlakeSandboxTarget,
        request: SandboxRequest,
        backend: TensorlakeBackendHandle,
    ) -> Self {
        Self {
            id,
            name,
            target: Mutex::new(target),
            target_is_live: AtomicBool::new(true),
            request,
            backend,
        }
    }

    /// The proxy target for the next command, re-resolving (and resuming) when a
    /// previous call found the sandbox gone — named sandboxes suspend on idle.
    async fn running_target(&self) -> Result<TensorlakeSandboxTarget> {
        let mut target = self.target.lock().await;
        if self.target_is_live.load(Ordering::Acquire) {
            return Ok(target.clone());
        }
        let identifier = self
            .name
            .clone()
            .unwrap_or_else(|| target.sandbox_id.clone());
        let info = self
            .backend
            .get_sandbox(&identifier)
            .await?
            .ok_or_else(|| anyhow!("Tensorlake sandbox {identifier} no longer exists"))?;
        *target = self.backend.drive_to_running(info).await?;
        self.target_is_live.store(true, Ordering::Release);
        Ok(target.clone())
    }

    fn mark_unreachable(&self) {
        self.target_is_live.store(false, Ordering::Release);
    }

    /// Runs a proxy call, retrying once against a freshly resolved target when
    /// the first attempt says the sandbox isn't reachable.
    async fn with_proxy_retry<T, Fut, F>(&self, call: F) -> Result<T>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, ProxyCallError>>,
    {
        let mut last_unavailable = None;
        for _ in 0..2 {
            let target = self.running_target().await?;
            match call(target.base_url).await {
                Ok(value) => return Ok(value),
                Err(ProxyCallError::Unavailable(error)) => {
                    self.mark_unreachable();
                    last_unavailable = Some(error);
                }
                Err(ProxyCallError::Failed(error)) => return Err(error),
            }
        }
        Err(last_unavailable.expect("unavailable error recorded before the retry budget ran out"))
    }
}

#[async_trait]
impl ManagedSandboxHandle for TensorlakeSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        if command.argv.is_empty() {
            bail!("sandbox command requires at least one argv entry");
        }
        let cwd = command
            .cwd
            .clone()
            .unwrap_or_else(|| self.request.spec.default_workdir.clone());
        self.with_proxy_retry(|base_url| {
            let cwd = cwd.clone();
            async move { run_process(&self.backend, &base_url, cwd, command).await }
        })
        .await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        if command.argv.is_empty() {
            bail!("sandbox command requires at least one argv entry");
        }
        let cwd = command
            .cwd
            .clone()
            .unwrap_or_else(|| self.request.spec.default_workdir.clone());
        self.with_proxy_retry(|base_url| {
            let cwd = cwd.clone();
            async move { start_process(&self.backend, &base_url, cwd, command).await }
        })
        .await
    }

    async fn stop(&self) -> Result<()> {
        let target = self.target.lock().await.clone();
        match &self.name {
            // Named sandboxes keep their filesystem across sessions; the next
            // `acquire` resumes this same sandbox by name.
            Some(name) => self.backend.suspend_sandbox(name).await,
            None => self.backend.delete_sandbox(&target.sandbox_id).await,
        }
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        let target = self.running_target().await?;
        let snapshot_id = self.backend.snapshot_sandbox(&target.sandbox_id).await?;
        let manifest = TensorlakeSnapshotManifest {
            snapshot_id,
            sandbox_name: self.name.clone(),
        };
        let bytes =
            serde_json::to_vec(&manifest).context("serializing Tensorlake snapshot manifest")?;
        Ok(SnapshotPayload {
            kind: SnapshotKind::TensorlakeSnapshot,
            bytes: Bytes::from(bytes),
        })
    }
}

/// Distinguishes "the sandbox proxy isn't reachable" (worth re-resolving the
/// sandbox and retrying) from a real command/API failure.
#[derive(Debug, thiserror::Error)]
enum ProxyCallError {
    #[error("{0}")]
    Unavailable(#[source] anyhow::Error),
    #[error("{0}")]
    Failed(#[from] anyhow::Error),
}

fn classify_proxy_status(status: reqwest::StatusCode, context: &str, body: &str) -> ProxyCallError {
    let error = anyhow!("{context} failed ({status}): {body}");
    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    ) {
        ProxyCallError::Unavailable(error)
    } else {
        ProxyCallError::Failed(error)
    }
}

fn classify_transport_error(error: reqwest::Error, context: &str) -> ProxyCallError {
    ProxyCallError::Unavailable(anyhow::Error::new(error).context(context.to_string()))
}

/// One-shot exec: `POST /api/v1/processes/run` streams captured output over SSE
/// and ends with an exit event.
async fn run_process(
    backend: &TensorlakeBackendHandle,
    base_url: &str,
    cwd: String,
    command: &SandboxCommand,
) -> std::result::Result<SandboxCommandOutput, ProxyCallError> {
    let body = RunProcessBody {
        command: command.argv[0].clone(),
        args: command.argv[1..].to_vec(),
        env: command.env.clone(),
        working_dir: (!cwd.is_empty()).then(|| cwd.clone()),
        timeout: command.timeout.map(|timeout| timeout.as_secs_f64()),
    };

    let response = backend
        .client
        .post(format!("{base_url}/api/v1/processes/run"))
        .json(&body)
        .send()
        .await
        .map_err(|error| classify_transport_error(error, "Tensorlake run-process"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(classify_proxy_status(
            status,
            "Tensorlake run-process",
            &text,
        ));
    }

    // Past this point the command is already running in the sandbox, so every
    // failure is terminal: a retry would execute it a second time. Only the
    // send/status failures above are safe to classify as `Unavailable`.
    let collect = collect_run_events(response);
    let (exit_code, stdout, stderr) = match command.timeout {
        Some(timeout) => time::timeout(timeout + EXEC_TIMEOUT_GRACE, collect)
            .await
            .map_err(|_| {
                anyhow!(
                    "sandbox command timed out after {}s: {}",
                    timeout.as_secs(),
                    command
                        .display_argv
                        .as_ref()
                        .unwrap_or(&command.argv)
                        .join(" ")
                )
            })??,
        None => collect.await?,
    };

    Ok(SandboxCommandOutput {
        ok: exit_code == Some(0),
        exit_code,
        stdout,
        stderr,
        command: command
            .display_argv
            .clone()
            .unwrap_or_else(|| command.argv.clone()),
        cwd,
    })
}

/// Drains the run stream. Every error here is reported as-is rather than as a
/// retryable one: the command has already run, so replaying it is not an option.
async fn collect_run_events(response: reqwest::Response) -> Result<(Option<i32>, String, String)> {
    let mut events = SseReader::new(response.bytes_stream());
    let mut stdout = OutputLines::default();
    let mut stderr = OutputLines::default();
    let mut exit_code = None;
    let mut saw_exit = false;

    while let Some(event) = events
        .next_event()
        .await
        .context("Tensorlake run-process")?
    {
        if event.data.is_empty() {
            continue;
        }
        let payload: RunProcessEvent = serde_json::from_str(&event.data)
            .with_context(|| format!("decoding Tensorlake run-process event: {}", event.data))?;
        match payload.line {
            Some(line) => match payload.stream {
                Some(ProcessOutputStream::Stderr) => stderr.push(line),
                Some(ProcessOutputStream::Stdout) | None => stdout.push(line),
            },
            // The started event carries the pid; nothing to collect from it.
            None if payload.started_at.is_some() => {}
            None => {
                saw_exit = true;
                exit_code = resolve_exit_code(payload.exit_code, payload.signal);
            }
        }
    }

    if !saw_exit {
        bail!(
            "Tensorlake run-process stream ended without an exit event; \
             the command may have run without its result being reported"
        );
    }
    Ok((exit_code, stdout.into_text(), stderr.into_text()))
}

/// Streaming process: start it in `pipe` stdin mode, then bridge stdin over
/// `/stdin`, stdout/stderr over the SSE follow endpoints, and exit status over
/// polled process metadata.
///
/// Tensorlake's process API frames output as lines, so a partial line (a prompt
/// with no trailing newline) only reaches the reader once the process emits a
/// newline or exits.
async fn start_process(
    backend: &TensorlakeBackendHandle,
    base_url: &str,
    cwd: String,
    command: &SandboxCommand,
) -> std::result::Result<crate::SandboxProcessParts, ProxyCallError> {
    let body = StartProcessBody {
        command: command.argv[0].clone(),
        args: command.argv[1..].to_vec(),
        env: command.env.clone(),
        working_dir: (!cwd.is_empty()).then_some(cwd),
        stdin_mode: "pipe",
        stdout_mode: "capture",
        stderr_mode: "capture",
    };

    let response = backend
        .client
        .post(format!("{base_url}/api/v1/processes"))
        .json(&body)
        .send()
        .await
        .map_err(|error| classify_transport_error(error, "Tensorlake start-process"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(classify_proxy_status(
            status,
            "Tensorlake start-process",
            &text,
        ));
    }
    let process: ProcessInfo = response
        .json()
        .await
        .context("decoding Tensorlake start-process response")
        .map_err(ProxyCallError::Failed)?;

    let (stdout_reader, stdout_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stderr_reader, stderr_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stdin_reader, stdin_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);

    let stdout_task = spawn_output_follower(
        backend.clone(),
        format!("{base_url}/api/v1/processes/{}/stdout/follow", process.pid),
        stdout_writer,
    );
    let stderr_task = spawn_output_follower(
        backend.clone(),
        format!("{base_url}/api/v1/processes/{}/stderr/follow", process.pid),
        stderr_writer,
    );
    spawn_stdin_forwarder(
        backend.clone(),
        format!("{base_url}/api/v1/processes/{}", process.pid),
        stdin_reader,
    );

    let backend_for_wait = backend.clone();
    let process_url = format!("{base_url}/api/v1/processes/{}", process.pid);
    let wait: BoxFuture<'static, crate::Result<i32>> = Box::pin(async move {
        let exit_code = wait_for_process_exit(&backend_for_wait, &process_url).await?;
        // Let the followers drain whatever the process emitted before exiting.
        for task in [stdout_task, stderr_task] {
            if time::timeout(PROCESS_OUTPUT_DRAIN_TIMEOUT, task)
                .await
                .is_err()
            {
                tracing::debug!(%process_url, "Tensorlake output follower did not finish draining");
            }
        }
        Ok(exit_code)
    });

    Ok(crate::SandboxProcessParts {
        stdout: Box::pin(stdout_reader.compat()),
        stderr: Box::pin(stderr_reader.compat()),
        stdin: Box::pin(stdin_writer.compat_write()),
        wait,
    })
}

fn spawn_output_follower(
    backend: TensorlakeBackendHandle,
    url: String,
    mut writer: tokio::io::DuplexStream,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = follow_output(&backend, &url, &mut writer).await {
            tracing::debug!(%url, %error, "Tensorlake output follower stopped");
        }
    })
}

async fn follow_output(
    backend: &TensorlakeBackendHandle,
    url: &str,
    writer: &mut tokio::io::DuplexStream,
) -> Result<()> {
    let response = backend
        .client
        .get(url)
        .send()
        .await
        .with_context(|| format!("following Tensorlake process output at {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Tensorlake follow-output failed ({status}): {text}");
    }

    let mut events = SseReader::new(response.bytes_stream());
    while let Some(event) = events.next_event().await? {
        if event.event == "eof" {
            break;
        }
        if event.data.is_empty() {
            continue;
        }
        let payload: ProcessOutputEvent = serde_json::from_str(&event.data)
            .with_context(|| format!("decoding Tensorlake output event: {}", event.data))?;
        let Some(line) = payload.line else {
            continue;
        };
        writer
            .write_all(line.as_bytes())
            .await
            .context("writing Tensorlake process output pipe")?;
        writer
            .write_all(b"\n")
            .await
            .context("writing Tensorlake process output pipe")?;
    }
    Ok(())
}

fn spawn_stdin_forwarder(
    backend: TensorlakeBackendHandle,
    process_url: String,
    mut reader: tokio::io::DuplexStream,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = vec![0u8; PROCESS_PIPE_BUFFER_SIZE];
        loop {
            let bytes_read = match reader.read(&mut buffer).await {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    tracing::debug!(%process_url, %error, "Tensorlake stdin pipe read failed");
                    break;
                }
            };
            if bytes_read == 0 {
                break;
            }
            let response = backend
                .client
                .post(format!("{process_url}/stdin"))
                .header(CONTENT_TYPE, "application/octet-stream")
                .body(buffer[..bytes_read].to_vec())
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => {
                    tracing::debug!(
                        %process_url,
                        status = %response.status(),
                        "Tensorlake stdin write rejected"
                    );
                    break;
                }
                Err(error) => {
                    tracing::debug!(%process_url, %error, "Tensorlake stdin write failed");
                    break;
                }
            }
        }

        if let Err(error) = backend
            .client
            .post(format!("{process_url}/stdin/close"))
            .send()
            .await
        {
            tracing::debug!(%process_url, %error, "Tensorlake stdin close failed");
        }
    })
}

async fn wait_for_process_exit(
    backend: &TensorlakeBackendHandle,
    process_url: &str,
) -> crate::Result<i32> {
    loop {
        let response = backend
            .client
            .get(process_url)
            .send()
            .await
            .with_context(|| format!("polling Tensorlake process at {process_url}"))?;
        let process: ProcessInfo = decode_json_response(response, "Tensorlake get-process").await?;
        if !matches!(process.status, ProcessStatus::Running) {
            return Ok(resolve_exit_code(process.exit_code, process.signal).unwrap_or_default());
        }
        time::sleep(PROCESS_WAIT_POLL_INTERVAL).await;
    }
}

/// Shells report a signalled death as `128 + signum`; mirror that so callers see
/// a conventional non-zero code instead of a missing one.
fn resolve_exit_code(exit_code: Option<i32>, signal: Option<i32>) -> Option<i32> {
    exit_code.or_else(|| signal.map(|signal| 128 + signal))
}

/// Accumulates line-framed output and rebuilds it as text.
#[derive(Debug, Default)]
struct OutputLines {
    lines: Vec<String>,
}

impl OutputLines {
    fn push(&mut self, line: String) {
        self.lines.push(line);
    }

    fn into_text(self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }
}

#[derive(Debug)]
struct SseEvent {
    event: String,
    data: String,
}

/// Minimal `text/event-stream` reader: accumulates `event:`/`data:` field lines
/// and yields one event per blank-line dispatch.
struct SseReader<S> {
    stream: S,
    buffer: Vec<u8>,
    finished: bool,
    event: String,
    data: String,
}

impl<S> SseReader<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    fn new(stream: S) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
            finished: false,
            event: String::new(),
            data: String::new(),
        }
    }

    async fn next_event(&mut self) -> Result<Option<SseEvent>> {
        loop {
            while let Some(line) = self.take_line() {
                if let Some(event) = self.consume_line(&line) {
                    return Ok(Some(event));
                }
            }
            if self.finished {
                // A stream that ends mid-event still owes us that event.
                return Ok(self.take_pending_event());
            }
            match self.stream.next().await {
                Some(chunk) => {
                    let chunk = chunk.context("reading Tensorlake event stream")?;
                    self.buffer.extend_from_slice(&chunk);
                }
                None => self.finished = true,
            }
        }
    }

    fn take_line(&mut self) -> Option<String> {
        let newline = self.buffer.iter().position(|byte| *byte == b'\n')?;
        let line = self.buffer.drain(..=newline).collect::<Vec<_>>();
        Some(
            String::from_utf8_lossy(&line)
                .trim_end_matches(['\n', '\r'])
                .to_string(),
        )
    }

    fn consume_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.take_pending_event();
        }
        if let Some(value) = line.strip_prefix("event:") {
            self.event = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
        // Comments (`:` lines) and unknown fields are ignored per the SSE spec.
        None
    }

    fn take_pending_event(&mut self) -> Option<SseEvent> {
        if self.event.is_empty() && self.data.is_empty() {
            return None;
        }
        Some(SseEvent {
            event: std::mem::take(&mut self.event),
            data: std::mem::take(&mut self.data),
        })
    }
}

async fn decode_json_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    context: &str,
) -> Result<T> {
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("reading {context} response body"))?;
    if !status.is_success() {
        bail!("{context} failed ({status}): {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("decoding {context} response: {body}"))
}

/// Deterministic sandbox name for a resumable sandbox key + spec. Tensorlake has
/// no label/metadata field on create, so the name is the only resume handle;
/// hashing the key and spec keeps it stable across processes and forces a new
/// sandbox whenever the spec changes. Ephemeral requests (no `idle_ttl`) get
/// `None` — only named sandboxes can suspend and resume.
fn sandbox_name_for_request(request: &SandboxRequest) -> Option<String> {
    request.lifecycle.idle_ttl?;
    let spec_hash = sandbox_spec_hash(&request.spec);
    let mut hasher = DefaultHasher::new();
    request.key.hash(&mut hasher);
    spec_hash.hash(&mut hasher);
    Some(format!("exo-{:016x}", hasher.finish()))
}

fn resolve_image(spec: &SandboxSpec, default_image: &str) -> String {
    if !spec.image.trim().is_empty() {
        return spec.image.clone();
    }
    if !default_image.trim().is_empty() {
        return default_image.to_string();
    }
    default_tensorlake_image()
}

fn reject_unsupported_mounts(request: &SandboxRequest) -> Result<()> {
    if !request.spec.mounts.is_empty() {
        bail!(
            "Tensorlake sandbox backend does not support host bind-mounts; \
             remove conversation mounts or use a local sandbox provider"
        );
    }
    if !request.spec.durable_file_systems.is_empty() {
        bail!("Tensorlake sandbox backend does not support durable file systems");
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct CreateSandboxBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<SandboxResourceOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    network: Option<SandboxNetworkAccessControl>,
}

#[derive(Debug, Serialize)]
struct SandboxResourceOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    cpus: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_mb: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SandboxNetworkAccessControl {
    allow_internet_access: bool,
}

#[derive(Debug, Serialize)]
struct SnapshotSandboxBody {
    snapshot_type: &'static str,
}

#[derive(Debug, Deserialize)]
struct SnapshotSandboxResponse {
    snapshot_id: String,
}

#[derive(Debug, Deserialize)]
struct CreateSandboxResponse {
    sandbox_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SandboxStatus {
    Pending,
    Running,
    Snapshotting,
    Suspending,
    Suspended,
    Terminated,
}

#[derive(Debug, Deserialize)]
struct SandboxInfo {
    id: String,
    status: SandboxStatus,
    #[serde(default)]
    pending_reason: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    sandbox_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct RunProcessBody {
    command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    env: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<f64>,
}

#[derive(Debug, Serialize)]
struct StartProcessBody {
    command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    env: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_dir: Option<String>,
    stdin_mode: &'static str,
    stdout_mode: &'static str,
    stderr_mode: &'static str,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProcessOutputStream {
    Stdout,
    Stderr,
}

/// One `data:` frame from `/api/v1/processes/run`. The endpoint emits three
/// shapes on the same stream — started, output, exited — distinguished by which
/// fields are present.
#[derive(Debug, Deserialize)]
struct RunProcessEvent {
    #[serde(default)]
    started_at: Option<i64>,
    #[serde(default)]
    line: Option<String>,
    #[serde(default)]
    stream: Option<ProcessOutputStream>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    signal: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct ProcessOutputEvent {
    #[serde(default)]
    line: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProcessStatus {
    Running,
    Exited,
    Signaled,
    OomKilled,
}

#[derive(Debug, Deserialize)]
struct ProcessInfo {
    pid: i32,
    status: ProcessStatus,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    signal: Option<i32>,
}

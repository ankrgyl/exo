//! Daytona remote-container sandbox backend, speaking its REST API via
//! `reqwest`. API reference: <https://www.daytona.io/docs/en/tools/api/>.
//!
//! Daytona persists state itself: `stop` keeps the filesystem and the next
//! `acquire` finds the sandbox by label and `start`s it. Snapshots use
//! [`SnapshotFormat::DaytonaRef`] payload — a JSON manifest naming a
//! snapshot in Daytona's registry, not the bytes themselves. A
//! [`SnapshotFormat::DockerImageTar`] payload (the Docker backend's snapshot
//! format) can also be restored here, which is how a sandbox teleports from
//! local Docker up to Daytona.

const DEFAULT_DAYTONA_IMAGE: &str = "daytonaio/sandbox:0.8.0";

pub fn default_daytona_image() -> String {
    DEFAULT_DAYTONA_IMAGE.to_string()
}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use uuid::Uuid;

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxRequest, SandboxSpec, SnapshotFormat, SnapshotPayload, WARM_SANDBOX_KEY_LABEL,
    WARM_SANDBOX_SPEC_HASH_LABEL, sandbox_spec_hash,
};
use crate::sandbox_provider::shell_quote;
use crate::{NetworkPolicyCapabilities, SandboxAttachment, SandboxNetworkPolicy};

pub const DEFAULT_DAYTONA_API_URL: &str = "https://app.daytona.io/api";

/// Per-sandbox operations (exec, fs) go through Daytona's toolbox proxy rather
/// than the control-plane API. The two have different base hosts.
pub const DEFAULT_DAYTONA_TOOLBOX_URL: &str = "https://proxy.app.daytona.io";

const START_POLL_INTERVAL: Duration = Duration::from_millis(500);
const START_TIMEOUT: Duration = Duration::from_secs(120);
const SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_secs(2);
const SNAPSHOT_WAIT_TIMEOUT: Duration = Duration::from_secs(180);
static CONSUMABLE_SNAPSHOT_FORMATS: [SnapshotFormat; 2] =
    [SnapshotFormat::DaytonaRef, SnapshotFormat::DockerImageTar];
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PROCESS_PIPE_BUFFER_SIZE: usize = 64 * 1024;
const PROCESS_ENV_END_MARKER: &str = "__EXO_ENV_END__";

/// Resolved Daytona connection parameters: assembled from a
/// [`crate::DaytonaBackendSpec`] plus secrets read on first use.
#[derive(Debug, Clone)]
pub struct DaytonaConfig {
    pub api_key: String,
    pub api_url: String,
    pub toolbox_url: String,
    /// Region target (e.g. `eu`, `us`), passed through on create when set.
    pub target: Option<String>,
    /// Scopes requests via `X-Daytona-Organization-ID`; without it Daytona may
    /// default to a "personal" org that lacks credit.
    pub organization_id: Option<String>,
}

pub struct DaytonaSandboxBackend {
    client: reqwest::Client,
    api_url: String,
    toolbox_url: String,
    target: Option<String>,
}

impl DaytonaSandboxBackend {
    pub fn new(config: DaytonaConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .context("Daytona API key contains characters that aren't valid in an HTTP header")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(org_id) = config.organization_id.as_deref() {
            let value = HeaderValue::from_str(org_id).context(
                "Daytona organization id contains characters that aren't valid in an HTTP header",
            )?;
            headers.insert("X-Daytona-Organization-ID", value);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("building Daytona HTTP client")?;
        Ok(Self {
            client,
            api_url: config.api_url.trim_end_matches('/').to_string(),
            toolbox_url: config.toolbox_url.trim_end_matches('/').to_string(),
            target: config.target,
        })
    }

    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    fn compile_network_policy(&self, policy: &SandboxNetworkPolicy) -> Result<bool> {
        // Daytona only exposes a coarse block-all networking switch. The
        // capability validation rejects domain/CIDR rules, so
        // `default_deny` fully captures the remaining policy and maps
        // directly to Daytona's native field.
        self.validate_network_policy(policy)?;
        Ok(policy.default_deny)
    }

    async fn find_sandbox_by_labels(
        &self,
        key_label: &str,
        spec_hash: &str,
    ) -> Result<Option<DaytonaSandbox>> {
        // `labels` is one query param holding a JSON object (per `GET /sandbox`);
        // both keys share it so a spec change yields a fresh sandbox.
        let labels_filter = serde_json::json!({
            WARM_SANDBOX_KEY_LABEL: key_label,
            WARM_SANDBOX_SPEC_HASH_LABEL: spec_hash,
        });
        let response = self
            .client
            .get(self.api_endpoint("/sandbox"))
            .query(&[("labels", labels_filter.to_string())])
            .send()
            .await
            .context("listing Daytona sandboxes")?
            .error_for_status()
            .context("Daytona list sandboxes returned an error status")?;
        let list: DaytonaSandboxList = response
            .json()
            .await
            .context("decoding Daytona sandbox list response")?;
        let mut sandboxes = list.items;
        // On multiple matches keep the most recent; the rest age out via Daytona.
        sandboxes.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(sandboxes.into_iter().next())
    }

    async fn create_sandbox(
        &self,
        request: &SandboxRequest,
        spec_hash: &str,
        snapshot_name: Option<&str>,
        network_block_all: bool,
    ) -> Result<DaytonaSandbox> {
        let mut labels = HashMap::new();
        labels.insert(WARM_SANDBOX_KEY_LABEL.to_string(), request.key.to_string());
        labels.insert(
            WARM_SANDBOX_SPEC_HASH_LABEL.to_string(),
            spec_hash.to_string(),
        );

        // 0 would disable auto-stop; default to Daytona's 15 min instead.
        let auto_stop_minutes = request
            .lifecycle
            .idle_ttl
            .map(idle_ttl_to_minutes)
            .unwrap_or(15);

        // Restore uses the saved snapshot; a fresh create falls back to the
        // requested image as a snapshot name (Daytona's only base selector).
        let snapshot = match snapshot_name {
            Some(name) => Some(name.to_string()),
            None => {
                let image = request.spec.image.trim();
                (!image.is_empty()).then(|| image.to_string())
            }
        };
        let body = DaytonaCreateRequest {
            snapshot,
            target: self.target.clone(),
            labels,
            env: HashMap::new(),
            auto_stop_interval: auto_stop_minutes,
            network_block_all,
        };

        let response = self
            .client
            .post(self.api_endpoint("/sandbox"))
            .json(&body)
            .send()
            .await
            .context("creating Daytona sandbox")?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("Daytona create-sandbox failed ({status}): {text}");
        }
        let sandbox: DaytonaSandbox = response
            .json()
            .await
            .context("decoding Daytona create-sandbox response")?;
        Ok(sandbox)
    }

    async fn start_sandbox(&self, id: &str) -> Result<()> {
        let response = self
            .client
            .post(self.api_endpoint(&format!("/sandbox/{id}/start")))
            .send()
            .await
            .with_context(|| format!("starting Daytona sandbox {id}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("Daytona start-sandbox failed ({status}): {text}");
        }
        Ok(())
    }

    async fn get_sandbox(&self, id: &str) -> Result<DaytonaSandbox> {
        self.client
            .get(self.api_endpoint(&format!("/sandbox/{id}")))
            .send()
            .await
            .with_context(|| format!("fetching Daytona sandbox {id}"))?
            .error_for_status()
            .with_context(|| format!("Daytona get-sandbox {id} returned an error status"))?
            .json()
            .await
            .with_context(|| format!("decoding Daytona sandbox {id}"))
    }

    /// Poll until the sandbox is `Started`, so the first exec doesn't race a
    /// still-booting sandbox (create and start are both async on Daytona's side).
    async fn wait_until_started(&self, id: &str) -> Result<()> {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match self.get_sandbox(id).await?.state {
                DaytonaSandboxState::Started => return Ok(()),
                DaytonaSandboxState::Error
                | DaytonaSandboxState::Destroying
                | DaytonaSandboxState::Destroyed => {
                    bail!("Daytona sandbox {id} entered a terminal state before starting")
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                bail!(
                    "Daytona sandbox {id} did not reach `started` within {}s",
                    START_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(START_POLL_INTERVAL).await;
        }
    }
}

#[async_trait]
impl ManagedSandboxBackend for DaytonaSandboxBackend {
    fn is_local(&self) -> bool {
        false
    }

    fn network_policy_capabilities(&self) -> NetworkPolicyCapabilities {
        NetworkPolicyCapabilities {
            default_deny: true,
            ..NetworkPolicyCapabilities::default()
        }
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &CONSUMABLE_SNAPSHOT_FORMATS
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_unsupported_mounts(&request)?;
        let network_block_all = self.compile_network_policy(&request.spec.network_policy)?;
        let spec_hash = sandbox_spec_hash(&request.spec);

        // Reuse a matching sandbox if one exists (also how we recover across exo
        // restarts); Daytona keeps its filesystem across stop.
        let sandbox = match self
            .find_sandbox_by_labels(&request.key.to_string(), &spec_hash)
            .await?
        {
            // Replace a terminal/error sandbox; start only durably-stopped ones
            // (starting a running/mid-transition sandbox would race).
            Some(existing) if existing.is_reusable() => {
                if existing.needs_start() {
                    self.start_sandbox(&existing.id).await?;
                }
                existing
            }
            _ => {
                self.create_sandbox(&request, &spec_hash, None, network_block_all)
                    .await?
            }
        };

        self.wait_until_started(&sandbox.id).await?;
        Ok(Arc::new(DaytonaSandboxHandle {
            id: format!("daytona:{}", request.key),
            sandbox_id: sandbox.id,
            request,
            backend: self.handle_backend(),
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Daytona sandbox backend does not support external attachments")
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_unsupported_mounts(&request)?;
        let network_block_all = self.compile_network_policy(&request.spec.network_policy)?;
        let snapshot_name = if payload.format == SnapshotFormat::DaytonaRef {
            let manifest: DaytonaSnapshotManifest = serde_json::from_slice(&payload.bytes)
                .context("decoding Daytona snapshot manifest")?;
            manifest.snapshot_name
        } else if payload.format == SnapshotFormat::DockerImageTar {
            import_docker_image_tar(&self.handle_backend(), &payload.bytes).await?
        } else {
            bail!(
                "the Daytona backend cannot restore a {} payload",
                payload.format
            )
        };
        let spec_hash = sandbox_spec_hash(&request.spec);
        let sandbox = self
            .create_sandbox(
                &request,
                &spec_hash,
                Some(&snapshot_name),
                network_block_all,
            )
            .await?;
        self.wait_until_started(&sandbox.id).await?;
        Ok(Arc::new(DaytonaSandboxHandle {
            id: format!("daytona:{}", request.key),
            sandbox_id: sandbox.id,
            request,
            backend: self.handle_backend(),
        }))
    }
}

impl DaytonaSandboxBackend {
    /// Clone the HTTP state into a value the handle owns, so handle operations
    /// don't re-borrow the backend.
    fn handle_backend(&self) -> DaytonaBackendHandle {
        DaytonaBackendHandle {
            client: self.client.clone(),
            api_url: self.api_url.clone(),
            toolbox_url: self.toolbox_url.clone(),
            target: self.target.clone(),
        }
    }
}

#[derive(Clone)]
struct DaytonaBackendHandle {
    client: reqwest::Client,
    api_url: String,
    toolbox_url: String,
    target: Option<String>,
}

impl DaytonaBackendHandle {
    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    fn toolbox_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.toolbox_url, path)
    }

    /// Short-lived push credentials for Daytona's transient registry, used by
    /// the Docker bridge (`GET /docker-registry/registry-push-access`).
    async fn get_registry_push_access(&self) -> Result<DaytonaRegistryPushAccess> {
        self.client
            .get(self.api_endpoint("/docker-registry/registry-push-access"))
            .send()
            .await
            .context("fetching Daytona registry push access")?
            .error_for_status()
            .context("Daytona registry-push-access returned an error status")?
            .json()
            .await
            .context("decoding Daytona registry push access")
    }

    /// Register a pushed transient-registry image as a Daytona snapshot in the
    /// backend's region (`POST /snapshots`). The snapshot builds asynchronously
    /// (pending -> active); see `wait_for_snapshot_active`.
    async fn register_snapshot(&self, name: &str, image_name: &str) -> Result<()> {
        let body = DaytonaCreateSnapshotRequest {
            name: name.to_string(),
            image_name: image_name.to_string(),
            region_id: self.target.clone(),
        };
        let response = self
            .client
            .post(self.api_endpoint("/snapshots"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("registering Daytona snapshot {name}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("Daytona snapshot registration failed ({status}): {text}");
        }
        Ok(())
    }

    /// `None` until the snapshot is registered: a freshly-captured snapshot 404s
    /// for a few seconds before it appears, so callers poll through that window.
    async fn get_snapshot(&self, name: &str) -> Result<Option<DaytonaSnapshotInfo>> {
        let response = self
            .client
            .get(self.api_endpoint(&format!("/snapshots/{name}")))
            .send()
            .await
            .with_context(|| format!("fetching Daytona snapshot {name}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        response
            .error_for_status()
            .with_context(|| format!("Daytona get-snapshot {name} returned an error status"))?
            .json()
            .await
            .map(Some)
            .with_context(|| format!("decoding Daytona snapshot {name}"))
    }

    /// Snapshot capture is asynchronous (404 -> Pending -> Building/Pulling ->
    /// Active); only `active` snapshots can be used to create sandboxes, so block
    /// until it reaches that state before returning.
    async fn wait_for_snapshot_active(&self, name: &str) -> Result<()> {
        let deadline = Instant::now() + SNAPSHOT_WAIT_TIMEOUT;
        loop {
            if let Some(info) = self.get_snapshot(name).await? {
                match info.state.to_ascii_lowercase().as_str() {
                    "active" => return Ok(()),
                    "error" | "build_failed" => {
                        bail!("Daytona snapshot {name} failed (state={})", info.state)
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "Daytona snapshot {name} did not become active within {}s",
                    SNAPSHOT_WAIT_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(SNAPSHOT_POLL_INTERVAL).await;
        }
    }

    async fn get_sandbox(&self, id: &str) -> Result<DaytonaSandbox> {
        self.client
            .get(self.api_endpoint(&format!("/sandbox/{id}")))
            .send()
            .await
            .with_context(|| format!("fetching Daytona sandbox {id}"))?
            .error_for_status()
            .with_context(|| format!("Daytona get-sandbox {id} returned an error status"))?
            .json()
            .await
            .with_context(|| format!("decoding Daytona sandbox {id}"))
    }

    async fn wait_until_snapshot_complete(&self, id: &str) -> Result<()> {
        let deadline = Instant::now() + SNAPSHOT_WAIT_TIMEOUT;
        loop {
            if !matches!(
                self.get_sandbox(id).await?.state,
                DaytonaSandboxState::Snapshotting
            ) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "Daytona snapshot for {id} did not finish within {}s",
                    SNAPSHOT_WAIT_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(SNAPSHOT_POLL_INTERVAL).await;
        }
    }
}

struct DaytonaSandboxHandle {
    id: String,
    sandbox_id: String,
    request: SandboxRequest,
    backend: DaytonaBackendHandle,
}

#[async_trait]
impl ManagedSandboxHandle for DaytonaSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_in_sandbox(&self.backend, &self.sandbox_id, &self.request.spec, command).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        start_process_in_sandbox(&self.backend, &self.sandbox_id, &self.request.spec, command).await
    }

    async fn stop(&self) -> Result<()> {
        // Stop, don't delete: Daytona preserves filesystem state across stop and
        // the next acquire for the same key finds and starts this sandbox again.
        stop_via_backend(&self.backend, &self.sandbox_id).await
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("Daytona sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        save_as_snapshot_via_backend(&self.backend, &self.sandbox_id).await
    }
}

async fn exec_in_sandbox(
    backend: &DaytonaBackendHandle,
    id: &str,
    spec: &SandboxSpec,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }
    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());
    let response = execute_daytona_process(
        backend,
        id,
        DaytonaExecRequest {
            command: render_shell_command(&command.argv),
            cwd: Some(cwd.clone()),
            env: command.env.clone(),
            timeout: command.timeout.map(|t| t.as_secs()),
        },
        "exec",
    )
    .await
    .context("running Daytona exec command")?;
    Ok(SandboxCommandOutput {
        ok: response.exit_code == 0,
        exit_code: Some(response.exit_code),
        stdout: response.result.unwrap_or_default(),
        stderr: String::new(),
        command: command
            .display_argv
            .clone()
            .unwrap_or_else(|| command.argv.clone()),
        cwd,
    })
}

async fn start_process_in_sandbox(
    backend: &DaytonaBackendHandle,
    id: &str,
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
    let session_id = format!("exo-process-{}", Uuid::new_v4());
    create_process_session(backend, id, &session_id).await?;
    let exit_status_path = format!("/tmp/exo-process-exit-{}.status", Uuid::new_v4());
    let command_id = match start_session_command(
        backend,
        id,
        &session_id,
        command,
        cwd,
        &exit_status_path,
    )
    .await
    {
        Ok(command_id) => command_id,
        Err(error) => {
            if let Err(cleanup_error) = delete_process_session(backend, id, &session_id).await {
                return Err(error).context(format!(
                "also failed to clean up Daytona process session {session_id}: {cleanup_error:#}"
            ));
            }
            return Err(error);
        }
    };
    let process = DaytonaSessionProcess {
        backend: backend.clone(),
        sandbox_id: id.to_string(),
        session_id,
        command_id,
        exit_status_path,
    };

    if !command.env.is_empty()
        && let Err(error) = send_session_environment_input(&process, &command.env).await
    {
        if let Err(cleanup_error) = cleanup_daytona_process(&process).await {
            return Err(error).context(format!(
                "also failed to clean up Daytona process session {}: {cleanup_error:#}",
                process.session_id
            ));
        }
        return Err(error);
    }

    let (stdout_reader, stdout_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stderr_reader, stderr_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (stdin_reader, stdin_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
    let (wait_tx, wait_rx) = oneshot::channel();
    let (stdin_error_tx, stdin_error_rx) = mpsc::unbounded_channel();

    spawn_daytona_process_poller(
        process.clone(),
        stdout_writer,
        stderr_writer,
        stdin_error_rx,
        wait_tx,
    );
    spawn_daytona_process_stdin_forwarder(process.clone(), stdin_reader, stdin_error_tx);

    let wait: BoxFuture<'static, crate::Result<i32>> = Box::pin(async move {
        let mut cleanup = DaytonaProcessCleanup::armed(process);
        match wait_rx.await {
            Ok(result) => {
                cleanup.disarm();
                result
            }
            Err(_) => Err(anyhow!("Daytona process poller stopped")),
        }
    });

    Ok(crate::SandboxProcessParts {
        stdout: Box::pin(stdout_reader.compat()),
        stderr: Box::pin(stderr_reader.compat()),
        stdin: Box::pin(stdin_writer.compat_write()),
        wait,
    })
}

fn spawn_daytona_process_poller(
    process: DaytonaSessionProcess,
    stdout_writer: tokio::io::DuplexStream,
    stderr_writer: tokio::io::DuplexStream,
    stdin_error_rx: mpsc::UnboundedReceiver<anyhow::Error>,
    wait_tx: oneshot::Sender<crate::Result<i32>>,
) {
    tokio::spawn(async move {
        let result = poll_daytona_process(
            process.clone(),
            stdout_writer,
            stderr_writer,
            stdin_error_rx,
        )
        .await;
        if result.is_err()
            && let Err(cleanup_error) = cleanup_daytona_process(&process).await
        {
            tracing::warn!(
                sandbox_id = %process.sandbox_id,
                session_id = %process.session_id,
                error = %cleanup_error,
                "failed to clean up Daytona process session"
            );
        }
        if wait_tx.send(result).is_err() {
            tracing::debug!(
                sandbox_id = %process.sandbox_id,
                session_id = %process.session_id,
                command_id = %process.command_id,
                "Daytona process waiter dropped before completion"
            );
        }
    });
}

fn spawn_daytona_process_stdin_forwarder(
    process: DaytonaSessionProcess,
    stdin_reader: tokio::io::DuplexStream,
    stdin_error_tx: mpsc::UnboundedSender<anyhow::Error>,
) {
    tokio::spawn(async move {
        if let Err(error) = forward_daytona_process_stdin(process.clone(), stdin_reader).await {
            tracing::warn!(
                sandbox_id = %process.sandbox_id,
                session_id = %process.session_id,
                command_id = %process.command_id,
                error = %error,
                "Daytona process stdin forwarder stopped"
            );
            if stdin_error_tx.send(error).is_err() {
                tracing::debug!(
                    sandbox_id = %process.sandbox_id,
                    session_id = %process.session_id,
                    command_id = %process.command_id,
                    "Daytona process poller stopped before stdin error could be reported"
                );
            }
        }
    });
}

async fn poll_daytona_process(
    process: DaytonaSessionProcess,
    mut stdout_writer: tokio::io::DuplexStream,
    mut stderr_writer: tokio::io::DuplexStream,
    mut stdin_error_rx: mpsc::UnboundedReceiver<anyhow::Error>,
) -> Result<i32> {
    let mut stdout_offset = 0usize;
    let mut stderr_offset = 0usize;
    loop {
        if let Ok(error) = stdin_error_rx.try_recv() {
            return Err(error.context("Daytona process stdin forwarding failed"));
        }
        let output = get_session_command_logs(&process).await?;
        write_log_update(&mut stdout_writer, &mut stdout_offset, output.stdout()).await?;
        write_log_update(&mut stderr_writer, &mut stderr_offset, output.stderr()).await?;
        let exit_code = get_session_command_exit_code(&process).await?;
        if let Some(exit_code) = exit_code {
            let output = get_session_command_logs(&process).await?;
            write_log_update(&mut stdout_writer, &mut stdout_offset, output.stdout()).await?;
            write_log_update(&mut stderr_writer, &mut stderr_offset, output.stderr()).await?;
            cleanup_daytona_process(&process).await?;
            return Ok(exit_code);
        }
        tokio::time::sleep(PROCESS_POLL_INTERVAL).await;
    }
}

async fn write_log_update(
    writer: &mut tokio::io::DuplexStream,
    offset: &mut usize,
    output: &str,
) -> Result<()> {
    if output.len() < *offset {
        *offset = 0;
    }
    if output.len() == *offset {
        return Ok(());
    }
    writer
        .write_all(&output.as_bytes()[*offset..])
        .await
        .context("writing Daytona process output pipe")?;
    *offset = output.len();
    Ok(())
}

async fn forward_daytona_process_stdin(
    process: DaytonaSessionProcess,
    mut stdin_reader: tokio::io::DuplexStream,
) -> Result<()> {
    let mut buffer = vec![0; PROCESS_PIPE_BUFFER_SIZE];
    let mut pending = Vec::new();
    loop {
        let bytes_read = stdin_reader
            .read(&mut buffer)
            .await
            .context("reading Daytona process stdin pipe")?;
        if bytes_read == 0 {
            if !pending.is_empty() {
                let data = String::from_utf8(pending)
                    .context("Daytona process stdin ended with invalid UTF-8")?;
                send_session_command_input(&process, data).await?;
            }
            return Ok(());
        }
        pending.extend_from_slice(&buffer[..bytes_read]);
        while let Some(prefix_len) = valid_utf8_prefix_len(&pending)? {
            let data = String::from_utf8(pending.drain(..prefix_len).collect())
                .context("validated Daytona process stdin UTF-8 prefix failed to decode")?;
            send_session_command_input(&process, data).await?;
        }
    }
}

fn valid_utf8_prefix_len(bytes: &[u8]) -> Result<Option<usize>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => Ok(Some(bytes.len())),
        Err(error) if error.error_len().is_none() => {
            let valid_up_to = error.valid_up_to();
            if valid_up_to == 0 {
                Ok(None)
            } else {
                Ok(Some(valid_up_to))
            }
        }
        Err(error) => bail!(
            "Daytona process stdin contains invalid UTF-8 at byte {}",
            error.valid_up_to()
        ),
    }
}

async fn create_process_session(
    backend: &DaytonaBackendHandle,
    id: &str,
    session_id: &str,
) -> Result<()> {
    let body = DaytonaCreateSessionRequest {
        session_id: session_id.to_string(),
    };
    let response = backend
        .client
        .post(backend.toolbox_endpoint(&format!("/toolbox/{id}/process/session")))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("creating Daytona process session {session_id}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona create process session failed ({status}): {text}");
    }
    Ok(())
}

async fn start_session_command(
    backend: &DaytonaBackendHandle,
    id: &str,
    session_id: &str,
    command: &SandboxCommand,
    cwd: String,
    exit_status_path: &str,
) -> Result<String> {
    let rendered_command = render_shell_command(&command.argv);
    if !command.env.is_empty() {
        validate_daytona_env(&command.env)?;
    }
    let wrapped_command = wrap_session_command_with_exit_status(
        &rendered_command,
        !command.env.is_empty(),
        exit_status_path,
    );
    let body = DaytonaSessionCommandRequest {
        command: wrapped_command,
        cwd: Some(cwd),
        timeout: command.timeout.map(|t| t.as_secs()),
        run_async: true,
        suppress_input_echo: true,
    };
    let response = backend
        .client
        .post(backend.toolbox_endpoint(&format!("/toolbox/{id}/process/session/{session_id}/exec")))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("starting Daytona session command in {session_id}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona start session command failed ({status}): {text}");
    }
    let response: DaytonaSessionCommandResponse = response
        .json()
        .await
        .context("decoding Daytona session command response")?;
    let command_id = response
        .command_id
        .ok_or_else(|| anyhow!("Daytona session command response did not include cmdId"))?;
    Ok(command_id)
}

async fn execute_daytona_process(
    backend: &DaytonaBackendHandle,
    id: &str,
    body: DaytonaExecRequest,
    operation: &str,
) -> Result<DaytonaExecResponse> {
    let response = backend
        .client
        .post(backend.toolbox_endpoint(&format!("/toolbox/{id}/process/execute")))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("{operation} in Daytona sandbox {id}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona {operation} failed ({status}): {text}");
    }
    response
        .json()
        .await
        .with_context(|| format!("decoding Daytona {operation} response"))
}

async fn send_session_environment_input(
    process: &DaytonaSessionProcess,
    env: &HashMap<String, String>,
) -> Result<()> {
    let input = render_session_environment_input(env)?;
    send_session_command_input(process, input)
        .await
        .context("sending Daytona process environment")
}

async fn cleanup_daytona_process(process: &DaytonaSessionProcess) -> Result<()> {
    delete_process_session(&process.backend, &process.sandbox_id, &process.session_id).await
}

async fn get_session_command_logs(process: &DaytonaSessionProcess) -> Result<DaytonaCommandLogs> {
    let response = process
        .backend
        .client
        .get(process.backend.toolbox_endpoint(&format!(
            "/toolbox/{}/process/session/{}/command/{}/logs",
            process.sandbox_id, process.session_id, process.command_id
        )))
        .send()
        .await
        .with_context(|| {
            format!(
                "fetching Daytona process logs for session {} command {}",
                process.session_id, process.command_id
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona process logs failed ({status}): {text}");
    }
    let is_json = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let text = response
        .text()
        .await
        .context("decoding Daytona process log body")?;
    if is_json {
        let logs: DaytonaSessionCommandLogsResponse =
            serde_json::from_str(&text).context("decoding Daytona process log JSON body")?;
        Ok(DaytonaCommandLogs::Structured(logs))
    } else {
        Ok(DaytonaCommandLogs::Raw(text))
    }
}

async fn get_session_command_exit_code(process: &DaytonaSessionProcess) -> Result<Option<i32>> {
    let status_exit_code = get_session_status_exit_code(process).await?;
    let file_exit_code = match get_session_exit_status_file(process).await {
        Ok(exit_code) => exit_code,
        Err(error) if status_exit_code.is_some() => {
            tracing::debug!(
                sandbox_id = %process.sandbox_id,
                session_id = %process.session_id,
                command_id = %process.command_id,
                error = %error,
                "failed to read Daytona process exit status file; falling back to session status"
            );
            None
        }
        Err(error) => return Err(error),
    };
    if let Some(file_exit_code) = file_exit_code {
        if status_exit_code.is_some_and(|status_exit_code| status_exit_code != file_exit_code) {
            tracing::debug!(
                sandbox_id = %process.sandbox_id,
                session_id = %process.session_id,
                command_id = %process.command_id,
                status_exit_code = ?status_exit_code,
                file_exit_code,
                "Daytona process status disagreed with exit status file"
            );
        }
        return Ok(Some(file_exit_code));
    }
    if let Some(status_exit_code) = status_exit_code {
        tracing::debug!(
            sandbox_id = %process.sandbox_id,
            session_id = %process.session_id,
            command_id = %process.command_id,
            status_exit_code,
            "Daytona process status reported an exit before the exit status file was visible"
        );
    }
    Ok(None)
}

async fn get_session_status_exit_code(process: &DaytonaSessionProcess) -> Result<Option<i32>> {
    let response = process
        .backend
        .client
        .get(process.backend.toolbox_endpoint(&format!(
            "/toolbox/{}/process/session/{}",
            process.sandbox_id, process.session_id
        )))
        .send()
        .await
        .with_context(|| format!("fetching Daytona process session {}", process.session_id))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona process session status failed ({status}): {text}");
    }
    let session: DaytonaSessionStatus = response
        .json()
        .await
        .context("decoding Daytona process session status")?;
    Ok(session
        .commands
        .iter()
        .find(|command| command.id.as_deref() == Some(process.command_id.as_str()))
        .and_then(|command| command.exit_code))
}

async fn get_session_exit_status_file(process: &DaytonaSessionProcess) -> Result<Option<i32>> {
    let path = shell_quote(&process.exit_status_path);
    let response = execute_daytona_process(
        &process.backend,
        &process.sandbox_id,
        DaytonaExecRequest {
            command: format!("if [ -f {path} ]; then cat {path}; fi"),
            cwd: None,
            env: HashMap::new(),
            timeout: Some(5),
        },
        "reading Daytona process exit status",
    )
    .await?;
    let output = response.result.unwrap_or_default();
    let output = output.trim();
    if output.is_empty() {
        return Ok(None);
    }
    Ok(Some(output.parse::<i32>().with_context(|| {
        format!(
            "decoding Daytona process exit status from {}",
            process.exit_status_path
        )
    })?))
}

async fn send_session_command_input(process: &DaytonaSessionProcess, data: String) -> Result<()> {
    let body = DaytonaSessionCommandInputRequest { data };
    let response = process
        .backend
        .client
        .post(process.backend.toolbox_endpoint(&format!(
            "/toolbox/{}/process/session/{}/command/{}/input",
            process.sandbox_id, process.session_id, process.command_id
        )))
        .json(&body)
        .send()
        .await
        .with_context(|| {
            format!(
                "sending Daytona process input for session {} command {}",
                process.session_id, process.command_id
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona process input failed ({status}): {text}");
    }
    Ok(())
}

async fn delete_process_session(
    backend: &DaytonaBackendHandle,
    id: &str,
    session_id: &str,
) -> Result<()> {
    let response = backend
        .client
        .delete(backend.toolbox_endpoint(&format!("/toolbox/{id}/process/session/{session_id}")))
        .send()
        .await
        .with_context(|| format!("deleting Daytona process session {session_id}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona delete process session failed ({status}): {text}");
    }
    Ok(())
}

async fn stop_via_backend(backend: &DaytonaBackendHandle, id: &str) -> Result<()> {
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/sandbox/{id}/stop")))
        .send()
        .await
        .with_context(|| format!("stopping Daytona sandbox {id}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona stop-sandbox failed ({status}): {text}");
    }
    Ok(())
}

// Snapshot the running sandbox (`POST /sandbox/{id}/snapshot`); can later be restored
// via `create_sandbox(snapshot = <name>)`.
async fn save_as_snapshot_via_backend(
    backend: &DaytonaBackendHandle,
    id: &str,
) -> Result<SnapshotPayload> {
    let snapshot_name = format!("exo-snap-{}", Uuid::new_v4().simple());
    let body = DaytonaSnapshotRequest {
        name: snapshot_name.clone(),
    };
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/sandbox/{id}/snapshot")))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("snapshotting Daytona sandbox {id}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::FORBIDDEN {
            bail!(
                "Daytona sandbox snapshots are not enabled for this organization \
                 (HTTP 403: {text}). Enable the feature in the Daytona dashboard or \
                 request it from Daytona support."
            );
        }
        bail!("Daytona snapshot failed ({status}): {text}");
    }
    // Capture is async for both the sandbox and the snapshot resource itself, wait for both.
    backend.wait_until_snapshot_complete(id).await?;
    backend.wait_for_snapshot_active(&snapshot_name).await?;

    let manifest = DaytonaSnapshotManifest { snapshot_name };
    let bytes = serde_json::to_vec(&manifest).context("serializing Daytona snapshot manifest")?;
    Ok(SnapshotPayload {
        format: SnapshotFormat::DaytonaRef,
        bytes: Bytes::from(bytes),
    })
}

// Cross-backend bridge: import a `docker save` tarball (the Docker backend's
// snapshot format) into Daytona as a snapshot and return its name, so a
// sandbox snapshotted on local Docker can be teleported to Daytona with its
// files intact.
//
// Note: requires a `docker` daemon on the harness host (used for load/tag/push).
// The resulting snapshot is disk/filesystem-only, i.e. not CRIU (no memory/process
// state). The image *must* be linux/amd64 to run on Daytona.
async fn import_docker_image_tar(backend: &DaytonaBackendHandle, tar: &[u8]) -> Result<String> {
    let snapshot_name = format!("exo-bridge-{}", Uuid::new_v4().simple());
    let tar_path = std::env::temp_dir().join(format!("{snapshot_name}.tar"));
    tokio::fs::write(&tar_path, tar)
        .await
        .with_context(|| format!("writing docker image tar to {}", tar_path.display()))?;

    let load = tokio::process::Command::new("docker")
        .arg("load")
        .arg("-i")
        .arg(&tar_path)
        .output()
        .await
        .context("running `docker load` (is docker installed on the harness host?)")?;
    let _ = tokio::fs::remove_file(&tar_path).await;
    if !load.status.success() {
        bail!(
            "docker load failed: {}",
            String::from_utf8_lossy(&load.stderr)
        );
    }
    let image_ref = String::from_utf8_lossy(&load.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Loaded image: ").map(str::trim))
        .map(str::to_string)
        .context("could not parse an image ref from `docker load` output")?;

    let push_access = backend.get_registry_push_access().await?;
    let remote_ref = format!(
        "{}/{}/{snapshot_name}:1",
        push_access.registry_url, push_access.project
    );
    let tag = tokio::process::Command::new("docker")
        .args(["tag", &image_ref, &remote_ref])
        .output()
        .await
        .context("running `docker tag`")?;
    if !tag.status.success() {
        bail!(
            "docker tag failed: {}",
            String::from_utf8_lossy(&tag.stderr)
        );
    }

    let push_result = docker_push_with_credentials(&push_access, &remote_ref).await;
    for image in [&image_ref, &remote_ref] {
        let _ = tokio::process::Command::new("docker")
            .args(["image", "rm", image])
            .output()
            .await;
    }
    push_result?;
    backend
        .register_snapshot(&snapshot_name, &remote_ref)
        .await?;

    backend.wait_for_snapshot_active(&snapshot_name).await?;
    Ok(snapshot_name)
}

/// `docker login` + `docker push` against an isolated `--config` directory:
/// the temp registry credentials are fed over stdin and written only to the
/// throwaway config, never to the host user's docker config or process argv.
async fn docker_push_with_credentials(
    push_access: &DaytonaRegistryPushAccess,
    remote_ref: &str,
) -> Result<()> {
    use tokio::process::Command;

    // We create a temp docker config dir so that the credentials are not written to the host
    // user's docker config. The temp dir is removed after the push. Note, these are the (1-hour
    // temporary) credentials provided by the Daytona backend, not the host user's docker credentials.
    let config_dir = std::env::temp_dir().join(format!("exo-docker-config-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&config_dir)
        .await
        .with_context(|| format!("creating temp docker config {}", config_dir.display()))?;

    let result = async {
        let mut login = Command::new("docker")
            .arg("--config")
            .arg(&config_dir)
            .args([
                "login",
                &push_access.registry_url,
                "--username",
                &push_access.username,
                "--password-stdin",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("running `docker login` for the Daytona transient registry")?;
        let mut stdin = login
            .stdin
            .take()
            .context("opening stdin for `docker login`")?;
        stdin
            .write_all(push_access.secret.as_bytes())
            .await
            .context("writing registry credentials to `docker login`")?;
        drop(stdin);
        let login = login.wait_with_output().await.context("`docker login`")?;
        if !login.status.success() {
            bail!(
                "docker login to {} failed: {}",
                push_access.registry_url,
                String::from_utf8_lossy(&login.stderr)
            );
        }

        let push = Command::new("docker")
            .arg("--config")
            .arg(&config_dir)
            .args(["push", remote_ref])
            .output()
            .await
            .context("running `docker push` to the Daytona transient registry")?;
        if !push.status.success() {
            bail!(
                "docker push {remote_ref} failed: {}",
                String::from_utf8_lossy(&push.stderr)
            );
        }
        Ok(())
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&config_dir).await;
    result
}

fn reject_unsupported_mounts(request: &SandboxRequest) -> Result<()> {
    if !request.spec.mounts.is_empty() {
        bail!(
            "Daytona sandbox backend does not support host bind-mounts; \
         remove conversation mounts or use a local sandbox provider"
        );
    }
    if !request.spec.durable_file_systems.is_empty() {
        bail!("Daytona sandbox backend does not support durable file systems");
    }
    Ok(())
}

fn idle_ttl_to_minutes(ttl: Duration) -> u32 {
    // Daytona auto-stop is in minutes, 0 meaning "disabled". Round up so we
    // never expire earlier than asked, and floor at 1 so the smallest TTL still
    // produces an active timer.
    let minutes = ttl.as_secs().div_ceil(60);
    minutes.clamp(1, u32::MAX as u64) as u32
}

fn render_shell_command(argv: &[String]) -> String {
    // Daytona's execute endpoint takes a single shell-interpreted string. Quote
    // each argv element so spaces / metacharacters survive.
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_daytona_env(env: &HashMap<String, String>) -> Result<Vec<(&str, &str)>> {
    let mut entries = env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    entries.sort_by_key(|(key, _)| *key);
    for (key, value) in &entries {
        if !is_shell_env_key(key) {
            bail!("Daytona process env key is not a valid shell identifier: {key}");
        }
        if value.contains(['\0', '\n']) {
            bail!("Daytona streamed process env value contains an unsupported newline or NUL");
        }
    }
    Ok(entries)
}

fn is_shell_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn render_session_environment_input(env: &HashMap<String, String>) -> Result<String> {
    let entries = validate_daytona_env(env)?;
    let mut input = String::new();
    for (key, value) in entries {
        input.push_str(key);
        input.push('\n');
        input.push_str(value);
        input.push('\n');
    }
    input.push_str(PROCESS_ENV_END_MARKER);
    input.push('\n');
    Ok(input)
}

fn wrap_session_command_with_exit_status(
    command: &str,
    needs_stdin_env: bool,
    exit_status_path: &str,
) -> String {
    let env_prelude = if needs_stdin_env {
        format!(
            "while IFS= read -r key; do [ \"$key\" = {} ] && break; IFS= read -r value; export \"$key=$value\"; done; ",
            shell_quote(PROCESS_ENV_END_MARKER)
        )
    } else {
        String::new()
    };
    format!(
        "set -e; {env_prelude}set +e; {command}; exo_exit_status=$?; printf '%s\\n' \"$exo_exit_status\" > {}",
        shell_quote(exit_status_path)
    )
}

#[derive(Debug, Serialize)]
struct DaytonaCreateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    labels: HashMap<String, String>,
    env: HashMap<String, String>,
    #[serde(rename = "autoStopInterval")]
    auto_stop_interval: u32,
    #[serde(rename = "networkBlockAll")]
    network_block_all: bool,
}

#[derive(Debug, Serialize)]
struct DaytonaExecRequest {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    env: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DaytonaExecResponse {
    #[serde(rename = "exitCode", alias = "exit_code")]
    exit_code: i32,
    #[serde(default)]
    result: Option<String>,
}

#[derive(Clone)]
struct DaytonaSessionProcess {
    backend: DaytonaBackendHandle,
    sandbox_id: String,
    session_id: String,
    command_id: String,
    exit_status_path: String,
}

struct DaytonaProcessCleanup {
    process: Option<DaytonaSessionProcess>,
}

impl DaytonaProcessCleanup {
    fn armed(process: DaytonaSessionProcess) -> Self {
        Self {
            process: Some(process),
        }
    }

    fn disarm(&mut self) {
        self.process = None;
    }
}

impl Drop for DaytonaProcessCleanup {
    fn drop(&mut self) {
        let Some(process) = self.process.take() else {
            return;
        };
        tokio::spawn(async move {
            match cleanup_daytona_process(&process).await {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(
                        sandbox_id = %process.sandbox_id,
                        session_id = %process.session_id,
                        command_id = %process.command_id,
                        error = %error,
                        "failed to clean up dropped Daytona process session"
                    );
                }
            }
        });
    }
}

#[derive(Debug, Serialize)]
struct DaytonaCreateSessionRequest {
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Debug, Serialize)]
struct DaytonaSessionCommandRequest {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
    #[serde(rename = "runAsync")]
    run_async: bool,
    #[serde(rename = "suppressInputEcho")]
    suppress_input_echo: bool,
}

#[derive(Debug, Deserialize)]
struct DaytonaSessionCommandResponse {
    #[serde(default, rename = "cmdId", alias = "cmd_id", alias = "id")]
    command_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DaytonaSessionStatus {
    #[serde(default)]
    commands: Vec<DaytonaSessionCommandStatus>,
}

#[derive(Debug, Deserialize)]
struct DaytonaSessionCommandStatus {
    #[serde(default, rename = "id", alias = "cmdId", alias = "cmd_id")]
    id: Option<String>,
    #[serde(default, rename = "exitCode", alias = "exit_code")]
    exit_code: Option<i32>,
}

#[derive(Debug)]
enum DaytonaCommandLogs {
    Structured(DaytonaSessionCommandLogsResponse),
    Raw(String),
}

impl DaytonaCommandLogs {
    fn stdout(&self) -> &str {
        match self {
            Self::Structured(logs) => {
                if logs.stdout.is_empty() {
                    logs.output.as_str()
                } else {
                    logs.stdout.as_str()
                }
            }
            Self::Raw(output) => output.as_str(),
        }
    }

    fn stderr(&self) -> &str {
        match self {
            Self::Structured(logs) => logs.stderr.as_str(),
            Self::Raw(_) => "",
        }
    }
}

#[derive(Debug, Deserialize)]
struct DaytonaSessionCommandLogsResponse {
    #[serde(default)]
    output: String,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
}

#[derive(Debug, Serialize)]
struct DaytonaSessionCommandInputRequest {
    data: String,
}

/// Persisted alongside a `SnapshotFormat::DaytonaRef`: a Daytona snapshot is
/// self-contained, so the registered name is all we need to recreate it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaytonaSnapshotManifest {
    snapshot_name: String,
}

#[derive(Debug, Serialize)]
struct DaytonaSnapshotRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct DaytonaSnapshotInfo {
    #[serde(default)]
    state: String,
}

/// Temporary robot-account credentials for the transient registry; expire
/// after about an hour, so they're fetched per-push and never persisted.
#[derive(Deserialize)]
struct DaytonaRegistryPushAccess {
    username: String,
    secret: String,
    #[serde(rename = "registryUrl")]
    registry_url: String,
    project: String,
}

#[derive(Debug, Serialize)]
struct DaytonaCreateSnapshotRequest {
    name: String,
    #[serde(rename = "imageName")]
    image_name: String,
    #[serde(rename = "regionId", skip_serializing_if = "Option::is_none")]
    region_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DaytonaSandboxList {
    #[serde(default)]
    items: Vec<DaytonaSandbox>,
}

#[derive(Debug, Deserialize)]
struct DaytonaSandbox {
    id: String,
    state: DaytonaSandboxState,
    #[serde(default, rename = "createdAt", alias = "created_at")]
    created_at: Option<String>,
}

/// Daytona sandbox lifecycle states (see the API reference). Unrecognized
/// values fold into `Unknown`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DaytonaSandboxState {
    Creating,
    Starting,
    #[serde(alias = "running")]
    Started,
    Snapshotting,
    Stopping,
    Stopped,
    Archiving,
    Archived,
    Destroying,
    Destroyed,
    Error,
    #[serde(other)]
    Unknown,
}

impl DaytonaSandbox {
    /// Reusable unless terminal/error (then `acquire` creates a fresh one).
    fn is_reusable(&self) -> bool {
        !matches!(
            self.state,
            DaytonaSandboxState::Error
                | DaytonaSandboxState::Destroying
                | DaytonaSandboxState::Destroyed
        )
    }

    /// Needs a `start` before use. Running/mid-transition states are left alone;
    /// `Unknown` errs toward starting (a no-op if already running).
    fn needs_start(&self) -> bool {
        matches!(
            self.state,
            DaytonaSandboxState::Stopped
                | DaytonaSandboxState::Archived
                | DaytonaSandboxState::Unknown
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_backend() -> DaytonaSandboxBackend {
        DaytonaSandboxBackend::new(DaytonaConfig {
            api_key: "test-key".to_string(),
            api_url: DEFAULT_DAYTONA_API_URL.to_string(),
            toolbox_url: DEFAULT_DAYTONA_TOOLBOX_URL.to_string(),
            target: None,
            organization_id: None,
        })
        .expect("create Daytona backend")
    }

    #[test]
    fn compiles_supported_network_policy_to_network_block_all() {
        let backend = test_backend();

        assert!(
            backend
                .compile_network_policy(&SandboxNetworkPolicy::default())
                .expect("compile default-deny policy")
        );
        assert!(
            !backend
                .compile_network_policy(&SandboxNetworkPolicy::allow_all())
                .expect("compile unrestricted policy")
        );
        assert!(
            backend
                .compile_network_policy(&SandboxNetworkPolicy {
                    allowed_domains: vec!["api.github.com".to_string()],
                    ..SandboxNetworkPolicy::default()
                })
                .is_err()
        );
    }

    #[test]
    fn utf8_prefix_waits_for_split_multibyte_character() {
        let mut bytes = "hello ".as_bytes().to_vec();
        bytes.push(0xc3);
        assert_eq!(valid_utf8_prefix_len(&bytes).unwrap(), Some(6));

        bytes.push(0xa9);
        assert_eq!(valid_utf8_prefix_len(&bytes).unwrap(), Some(8));
    }

    #[test]
    fn utf8_prefix_rejects_invalid_bytes() {
        let error = valid_utf8_prefix_len(&[0xff]).unwrap_err();
        assert!(
            format!("{error:#}").contains("invalid UTF-8"),
            "unexpected error: {error:#}"
        );
    }
}

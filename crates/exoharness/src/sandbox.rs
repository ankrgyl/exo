use std::collections::{HashMap, hash_map::DefaultHasher};
use std::ffi::OsString;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use uuid::Uuid;

use crate::{DurableFileSystem, SandboxAttachment};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SandboxKey {
    AgentSandbox {
        agent_id: String,
        sandbox_id: String,
    },
    ConversationSandbox {
        conversation_id: String,
        sandbox_id: String,
    },
}

impl fmt::Display for SandboxKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentSandbox {
                agent_id,
                sandbox_id,
            } => write!(f, "agent:{agent_id}:{sandbox_id}"),
            Self::ConversationSandbox {
                conversation_id,
                sandbox_id,
            } => write!(f, "conversation:{conversation_id}:{sandbox_id}"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxLifecycleConfig {
    pub idle_ttl: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxMountAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxMount {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub access: SandboxMountAccess,
    pub internal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxNetworkPolicy {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxSpec {
    pub image: String,
    pub mounts: Vec<SandboxMount>,
    pub durable_file_systems: Vec<DurableFileSystem>,
    pub network: SandboxNetworkPolicy,
    pub default_workdir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    pub key: SandboxKey,
    pub spec: SandboxSpec,
    pub lifecycle: SandboxLifecycleConfig,
    pub provider_state: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct SandboxCommand {
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    pub display_argv: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct SandboxCommandOutput {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub command: Vec<String>,
    pub cwd: String,
}

/// Opaque blob produced by `ManagedSandboxHandle::snapshot` and consumed by
/// `ManagedSandboxBackend::acquire_from_snapshot`. The `kind` tag is the
/// contract: a snapshot produced by one backend can only be restored by a
/// backend that knows how to interpret that kind.
#[derive(Debug, Clone)]
pub struct SnapshotPayload {
    pub kind: SnapshotKind,
    pub bytes: Bytes,
}

/// Tag identifying the on-disk format of a snapshot payload. Backends both
/// produce and consume a specific kind; the conversation layer just hands the
/// bytes back to the same backend type that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    /// `docker save` output: a tar of OCI image layers + manifest, loadable
    /// with `docker load`.
    DockerImageTar,
    /// JSON manifest pointing at a named snapshot in Daytona's registry; the
    /// filesystem bytes live in Daytona, not in the payload.
    DaytonaSnapshot,
    /// Reference to an E2B snapshot template id. Payload bytes are a small JSON
    /// manifest; restoring is `POST /sandboxes { templateID: <snapshot_id> }`.
    E2bSnapshot,
    /// Reference to a Sprites checkpoint id on a named sprite. Payload bytes are
    /// a small JSON manifest; restoring is `POST .../checkpoints/{id}/restore`.
    SpritesSnapshot,
    /// Reference to a Tensorlake platform snapshot id. Payload bytes are a small
    /// JSON manifest; restoring is `POST /sandboxes { snapshot_id: <id> }`.
    TensorlakeSnapshot,
}

#[async_trait]
pub trait ManagedSandboxHandle: Send + Sync {
    fn id(&self) -> &str;

    fn provider_state(&self) -> Option<Value> {
        None
    }

    /// The image the sandbox is actually running when the backend substituted
    /// the requested one (e.g. a snapshot restore boots from a freshly loaded
    /// tag). Callers persist this so every process resolves the same warm
    /// sandbox after a restore.
    fn effective_image(&self) -> Option<String> {
        None
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput>;

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts>;

    async fn stop(&self) -> Result<()>;

    /// Relinquish lifecycle ownership without stopping the sandbox and return
    /// the descriptor required to attach to it elsewhere.
    async fn detach(&self) -> Result<SandboxAttachment>;

    /// Capture the sandbox's current state as an opaque blob. Returns an
    /// error if this backend doesn't (yet) support snapshotting.
    async fn snapshot(&self) -> Result<SnapshotPayload>;
}

#[async_trait]
pub trait ManagedSandboxBackend: Send + Sync {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>>;
    async fn attach(
        &self,
        request: SandboxRequest,
        attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>>;

    /// Acquire a sandbox initialised from a previously-captured snapshot.
    /// The request is honoured for mounts, network, lifecycle, etc., but the
    /// container's filesystem is sourced from the payload instead of
    /// `request.spec.image`. Returns an error if this backend can't restore
    /// the supplied `payload.kind`.
    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>>;
}

pub const DEFAULT_SANDBOX_IMAGE: &str = crate::sandbox_provider::DEFAULT_DOCKER_IMAGE;
pub const SANDBOX_HOME_DIR: &str = "/home/exo";
pub const SANDBOX_MAIN_MOUNT_DIR: &str = "/home/exo/workspace";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerCliFlavor {
    AppleContainer,
    Docker,
}

impl ContainerCliFlavor {
    fn default_binary(self) -> &'static str {
        match self {
            Self::AppleContainer => "container",
            Self::Docker => "docker",
        }
    }

    fn requires_system_start(self) -> bool {
        matches!(self, Self::AppleContainer)
    }
}

const DEFAULT_ENABLED_NETWORK_NAME: &str = "exo-default";
const WARM_SANDBOX_KEEPALIVE_ARGV: &[&str] = &["sleep", "infinity"];
const WARM_SANDBOX_HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);
const WARM_SANDBOX_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const ORPHANED_WARM_SANDBOX_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_NETWORK_CREATE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_NETWORK_CREATE_RETRY_DELAY: Duration = Duration::from_millis(200);
pub(crate) const WARM_SANDBOX_KEY_LABEL: &str = "exo.sandbox.key";
pub(crate) const WARM_SANDBOX_SPEC_HASH_LABEL: &str = "exo.sandbox.spec-hash";
const WARM_SANDBOX_OWNER_PID_LABEL: &str = "exo.sandbox.owner-pid";
const APPLE_ABSOLUTE_TIME_UNIX_OFFSET_SECONDS: f64 = 978_307_200.0;
const DURABLE_FILE_SYSTEM_ROOT_ENV: &str = "EXO_DURABLE_FILE_SYSTEM_ROOT";

#[derive(Debug, Clone)]
struct WarmSandboxEntry {
    name: String,
    request: SandboxRequest,
    last_used_at: Instant,
    owned: bool,
}

#[derive(Debug, Deserialize)]
struct ContainerListItem {
    status: Option<String>,
    #[serde(rename = "startedDate")]
    started_date: Option<f64>,
    configuration: ContainerListConfiguration,
}

#[derive(Debug, Deserialize)]
struct ContainerListConfiguration {
    id: String,
    #[serde(default)]
    labels: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct DockerInspectItem {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "State")]
    state: DockerInspectState,
    #[serde(rename = "Config")]
    config: DockerInspectConfiguration,
}

#[derive(Debug, Deserialize)]
struct DockerInspectState {
    #[serde(rename = "Status")]
    status: Option<String>,
    #[serde(rename = "StartedAt")]
    started_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerInspectConfiguration {
    #[serde(rename = "Labels", default)]
    labels: HashMap<String, String>,
}

async fn inspect_running_docker_container(
    container_bin: &Path,
    container_id: &str,
) -> Result<String> {
    let container_id = container_id.trim();
    if container_id.is_empty() {
        bail!("Docker container id must not be empty");
    }
    let output = run_container_admin_command(
        container_bin,
        WARM_SANDBOX_CLEANUP_TIMEOUT,
        ["inspect", container_id],
    )
    .await
    .with_context(|| missing_container_cli_message(ContainerCliFlavor::Docker, container_bin))?;
    if !output.status.success() {
        bail!(
            "failed to inspect Docker container {container_id}: {}",
            render_command_error(&output.stderr)
        );
    }
    let mut containers = serde_json::from_slice::<Vec<DockerInspectItem>>(&output.stdout)?;
    let container = containers
        .pop()
        .ok_or_else(|| anyhow!("Docker inspect returned no container for {container_id}"))?;
    if container.state.status.as_deref() != Some("running") {
        bail!("Docker container {container_id} is not running");
    }
    Ok(container.id)
}

#[derive(Debug)]
pub struct CliContainerSandboxBackend {
    cli: ContainerCliFlavor,
    container_bin: PathBuf,
    durable_file_system_root: Option<PathBuf>,
    system_started: Mutex<bool>,
    network_created: Mutex<bool>,
    warm_sandboxes: Arc<Mutex<HashMap<SandboxKey, WarmSandboxEntry>>>,
}

impl CliContainerSandboxBackend {
    pub fn apple_container() -> Self {
        Self::new(ContainerCliFlavor::AppleContainer)
    }

    pub fn docker() -> Self {
        Self::new(ContainerCliFlavor::Docker)
    }

    pub fn with_durable_file_system_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.durable_file_system_root = Some(root.into());
        self
    }

    fn new(cli: ContainerCliFlavor) -> Self {
        Self {
            cli,
            container_bin: PathBuf::from(cli.default_binary()),
            durable_file_system_root: None,
            system_started: Mutex::new(false),
            network_created: Mutex::new(false),
            warm_sandboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn ensure_system_started(&self) -> Result<()> {
        let mut started = self.system_started.lock().await;
        if *started {
            return Ok(());
        }

        if self.cli.requires_system_start() {
            let output = Command::new(&self.container_bin)
                .arg("system")
                .arg("start")
                .kill_on_drop(true)
                .output()
                .await
                .with_context(|| missing_container_cli_message(self.cli, &self.container_bin))?;
            if !output.status.success() {
                return Err(anyhow!(
                    "failed to start container system: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }

        if let Err(error) = reap_orphaned_warm_sandboxes(&self.container_bin, self.cli).await {
            tracing::warn!(%error, "failed to reap orphaned warm sandboxes");
        }
        *started = true;
        Ok(())
    }

    async fn ensure_default_network_created(&self) -> Result<()> {
        let mut created = self.network_created.lock().await;
        if *created {
            return Ok(());
        }

        let deadline = Instant::now() + DEFAULT_NETWORK_CREATE_TIMEOUT;
        loop {
            let output = Command::new(&self.container_bin)
                .arg("network")
                .arg("create")
                .arg(DEFAULT_ENABLED_NETWORK_NAME)
                .kill_on_drop(true)
                .output()
                .await?;
            if output.status.success() {
                *created = true;
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.contains("already exists") {
                *created = true;
                return Ok(());
            }
            if stderr.contains("pending operation") && Instant::now() < deadline {
                tokio::time::sleep(DEFAULT_NETWORK_CREATE_RETRY_DELAY).await;
                continue;
            }

            return Err(anyhow!(
                "failed to create default container network {DEFAULT_ENABLED_NETWORK_NAME}: {stderr}"
            ));
        }
    }

    async fn prepare_request(&self, request: SandboxRequest) -> Result<SandboxRequest> {
        validate_durable_file_system_mounts(
            &request.spec.mounts,
            &request.spec.durable_file_systems,
        )?;
        self.ensure_system_started().await?;
        if matches!(request.spec.network, SandboxNetworkPolicy::Enabled) {
            self.ensure_default_network_created().await?;
        }

        let mut mounts = request
            .spec
            .mounts
            .into_iter()
            .map(|mount| {
                let host_path = std::fs::canonicalize(&mount.host_path)?;
                if !host_path.is_dir() {
                    bail!(
                        "sandbox mount root is not a directory: {}",
                        host_path.display()
                    );
                }
                Ok(SandboxMount { host_path, ..mount })
            })
            .collect::<Result<Vec<_>>>()?;
        mounts.extend(materialize_durable_file_systems(
            &request.key,
            &request.spec.durable_file_systems,
            self.durable_file_system_root.as_deref(),
        )?);

        Ok(SandboxRequest {
            key: request.key,
            spec: SandboxSpec {
                image: if request.spec.image.trim().is_empty() {
                    DEFAULT_SANDBOX_IMAGE.to_string()
                } else {
                    request.spec.image
                },
                mounts,
                durable_file_systems: request.spec.durable_file_systems,
                network: request.spec.network,
                default_workdir: request.spec.default_workdir,
            },
            lifecycle: request.lifecycle,
            provider_state: request.provider_state,
        })
    }

    async fn reap_expired_warm_sandboxes(&self) -> Result<()> {
        let now = Instant::now();
        let expired = {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            let expired_keys = warm_sandboxes
                .iter()
                .filter_map(|(key, entry)| {
                    let ttl = entry.request.lifecycle.idle_ttl?;
                    (entry.owned && entry.last_used_at + ttl <= now).then(|| key.clone())
                })
                .collect::<Vec<_>>();

            expired_keys
                .into_iter()
                .filter_map(|key| warm_sandboxes.remove(&key))
                .collect::<Vec<_>>()
        };

        for entry in expired {
            if entry.owned {
                cleanup_named_container(&self.container_bin, self.cli, &entry.name).await?;
            }
        }

        Ok(())
    }
}

impl Drop for CliContainerSandboxBackend {
    fn drop(&mut self) {
        // Warm sandboxes intentionally outlive a single CLI/REPL process so a
        // restarted agent can reattach to the same environment. Stale containers
        // are cleaned by the orphan reaper on later backend startup.
    }
}

#[async_trait]
impl ManagedSandboxBackend for CliContainerSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let request = self.prepare_request(request).await?;

        if request.lifecycle.idle_ttl.is_none() {
            return Ok(Arc::new(OneShotSandboxHandle {
                id: format!("oneshot:{}", request.key),
                container_bin: self.container_bin.clone(),
                request,
            }));
        }

        self.reap_expired_warm_sandboxes().await?;

        let replaced = {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            match warm_sandboxes.get(&request.key) {
                Some(entry) if entry.request.spec == request.spec => {
                    return Ok(Arc::new(WarmSandboxHandle {
                        id: format!("warm:{}", request.key),
                        cli: self.cli,
                        container_bin: self.container_bin.clone(),
                        request,
                        warm_sandboxes: Arc::clone(&self.warm_sandboxes),
                    }));
                }
                Some(_) => warm_sandboxes.remove(&request.key),
                None => None,
            }
        };
        if let Some(entry) = replaced
            && entry.owned
        {
            schedule_cleanup_named_container(self.container_bin.clone(), self.cli, entry.name);
        }

        let (name, owned) =
            match find_running_warm_sandbox(&self.container_bin, self.cli, &request).await? {
                Some(name) => (name, false),
                None => (
                    create_unique_warm_sandbox(&self.container_bin, &request).await?,
                    true,
                ),
            };

        {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            warm_sandboxes.insert(
                request.key.clone(),
                WarmSandboxEntry {
                    name: name.clone(),
                    request: request.clone(),
                    last_used_at: Instant::now(),
                    owned,
                },
            );
        }

        Ok(Arc::new(WarmSandboxHandle {
            id: format!("warm:{}", request.key),
            cli: self.cli,
            container_bin: self.container_bin.clone(),
            request,
            warm_sandboxes: Arc::clone(&self.warm_sandboxes),
        }))
    }

    async fn attach(
        &self,
        request: SandboxRequest,
        attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        if self.cli != ContainerCliFlavor::Docker {
            bail!("Docker container attachments require the Docker sandbox provider");
        }
        let SandboxAttachment::DockerContainer { container_id } = attachment;
        let container_id =
            inspect_running_docker_container(&self.container_bin, &container_id).await?;
        Ok(Arc::new(BorrowedDockerSandboxHandle {
            id: format!("borrowed-docker:{container_id}"),
            container_bin: self.container_bin.clone(),
            container_id,
            spec: request.spec,
        }))
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        if request.lifecycle.idle_ttl.is_none() {
            bail!("restore-from-snapshot requires a warm sandbox lifecycle (idle_ttl must be set)");
        }
        match (self.cli, payload.kind) {
            (ContainerCliFlavor::Docker, SnapshotKind::DockerImageTar) => {}
            (ContainerCliFlavor::AppleContainer, _) => bail!(
                "restore-from-snapshot is not yet implemented for the apple-container backend"
            ),
            (_, SnapshotKind::DaytonaSnapshot) => {
                bail!("restoring a Daytona snapshot on a container backend is not implemented yet")
            }
            (_, SnapshotKind::E2bSnapshot) => bail!(
                "E2bSnapshot payloads can only be restored by the E2B sandbox provider; \
                 select provider e2b to rewind this snapshot"
            ),
            (_, SnapshotKind::SpritesSnapshot) => bail!(
                "SpritesSnapshot payloads can only be restored by the Sprites sandbox provider; \
                 select provider sprites to rewind this snapshot"
            ),
            (_, SnapshotKind::TensorlakeSnapshot) => bail!(
                "TensorlakeSnapshot payloads can only be restored by the Tensorlake sandbox \
                 provider; select provider tensorlake to rewind this snapshot"
            ),
        }

        let image_tag = docker_load_image(&self.container_bin, &payload.bytes).await?;

        // Build a fresh request that points at the loaded image. Mounts,
        // network policy, lifecycle, and key are all preserved from the
        // original request so the restored sandbox is otherwise identical.
        let mut request = self.prepare_request(request).await?;
        request.spec.image = image_tag;

        // Evict any pre-existing warm container for this key — we want a
        // fresh container booted from the restored image, not a reuse of
        // whatever was running before.
        let replaced = {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            warm_sandboxes.remove(&request.key)
        };
        if let Some(entry) = replaced {
            schedule_cleanup_named_container(self.container_bin.clone(), self.cli, entry.name);
        }

        let name = create_unique_warm_sandbox(&self.container_bin, &request).await?;
        {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            warm_sandboxes.insert(
                request.key.clone(),
                WarmSandboxEntry {
                    name: name.clone(),
                    request: request.clone(),
                    last_used_at: Instant::now(),
                    owned: true,
                },
            );
        }

        Ok(Arc::new(WarmSandboxHandle {
            id: format!("warm:{}", request.key),
            cli: self.cli,
            container_bin: self.container_bin.clone(),
            request,
            warm_sandboxes: Arc::clone(&self.warm_sandboxes),
        }))
    }
}

struct BorrowedDockerSandboxHandle {
    id: String,
    container_bin: PathBuf,
    container_id: String,
    spec: SandboxSpec,
}

#[async_trait]
impl ManagedSandboxHandle for BorrowedDockerSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        inspect_running_docker_container(&self.container_bin, &self.container_id).await?;
        exec_warm(&self.container_bin, &self.container_id, &self.spec, command).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        inspect_running_docker_container(&self.container_bin, &self.container_id).await?;
        start_warm_process(&self.container_bin, &self.container_id, &self.spec, command).await
    }

    async fn stop(&self) -> Result<()> {
        bail!("borrowed Docker containers must be detached, not stopped")
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        Ok(SandboxAttachment::DockerContainer {
            container_id: self.container_id.clone(),
        })
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!("borrowed Docker containers cannot be snapshotted")
    }
}

struct OneShotSandboxHandle {
    id: String,
    container_bin: PathBuf,
    request: SandboxRequest,
}

#[async_trait]
impl ManagedSandboxHandle for OneShotSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_one_shot(
            &self.container_bin,
            &self.request.spec,
            network_name_for_policy(self.request.spec.network),
            command,
        )
        .await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        start_one_shot_process(
            &self.container_bin,
            &self.request.spec,
            network_name_for_policy(self.request.spec.network),
            command,
        )
        .await
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("one-shot sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!(
            "snapshot is not supported for one-shot sandboxes (set a positive idle_ttl to enable warm sandbox + snapshotting)"
        )
    }
}

struct WarmSandboxHandle {
    id: String,
    cli: ContainerCliFlavor,
    container_bin: PathBuf,
    request: SandboxRequest,
    warm_sandboxes: Arc<Mutex<HashMap<SandboxKey, WarmSandboxEntry>>>,
}

#[async_trait]
impl ManagedSandboxHandle for WarmSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn effective_image(&self) -> Option<String> {
        Some(self.request.spec.image.clone())
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let name = ensure_warm_sandbox_ready(
            &self.container_bin,
            self.cli,
            &self.request,
            &self.warm_sandboxes,
        )
        .await?;
        touch_warm_sandbox(&self.warm_sandboxes, &self.request.key).await;
        let output = exec_warm(&self.container_bin, &name, &self.request.spec, command).await;
        touch_warm_sandbox(&self.warm_sandboxes, &self.request.key).await;
        output
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        let name = ensure_warm_sandbox_ready(
            &self.container_bin,
            self.cli,
            &self.request,
            &self.warm_sandboxes,
        )
        .await?;
        touch_warm_sandbox(&self.warm_sandboxes, &self.request.key).await;
        start_warm_process(&self.container_bin, &name, &self.request.spec, command).await
    }

    async fn stop(&self) -> Result<()> {
        let removed = {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            warm_sandboxes.remove(&self.request.key)
        };

        if let Some(entry) = removed
            && entry.owned
        {
            cleanup_named_container(&self.container_bin, self.cli, &entry.name).await
        } else {
            Ok(())
        }
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        if self.cli != ContainerCliFlavor::Docker {
            bail!("detaching an Exo-created sandbox is only supported by Docker");
        }
        let name = ensure_warm_sandbox_ready(
            &self.container_bin,
            self.cli,
            &self.request,
            &self.warm_sandboxes,
        )
        .await?;
        let container_id = inspect_running_docker_container(&self.container_bin, &name).await?;
        let mut warm_sandboxes = self.warm_sandboxes.lock().await;
        let entry = warm_sandboxes
            .get_mut(&self.request.key)
            .ok_or_else(|| anyhow!("warm sandbox disappeared while detaching"))?;
        entry.owned = false;
        Ok(SandboxAttachment::DockerContainer { container_id })
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        match self.cli {
            ContainerCliFlavor::Docker => {
                let name = ensure_warm_sandbox_ready(
                    &self.container_bin,
                    self.cli,
                    &self.request,
                    &self.warm_sandboxes,
                )
                .await?;
                touch_warm_sandbox(&self.warm_sandboxes, &self.request.key).await;
                docker_snapshot_container(&self.container_bin, &name).await
            }
            // The Apple `container` CLI exposes `container image save` and a
            // `container commit`-style flow on its roadmap but neither is in
            // the released versions we target today. When it lands, the path
            // will mirror docker_snapshot_container: produce a single tarball
            // and tag it with a new SnapshotKind variant (e.g.
            // AppleContainerImageTar). Until then, fail explicitly so callers
            // know to choose Docker for snapshot-using flows.
            ContainerCliFlavor::AppleContainer => bail!(
                "snapshot is not yet implemented for the apple-container backend; \
                 use --sandbox-provider docker for snapshot-using flows"
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct LocalProcessSandboxBackend;

impl LocalProcessSandboxBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ManagedSandboxBackend for LocalProcessSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        if !request.spec.durable_file_systems.is_empty() {
            bail!("local-process sandbox backend does not support durable file systems");
        }
        Ok(Arc::new(LocalProcessSandboxHandle {
            id: format!("local:{}", request.key),
            request,
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("local-process sandbox backend does not support attachments")
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("restore-from-snapshot is not supported by the local-process sandbox backend")
    }
}

struct LocalProcessSandboxHandle {
    id: String,
    request: SandboxRequest,
}

#[async_trait]
impl ManagedSandboxHandle for LocalProcessSandboxHandle {
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
        let mut process = Command::new(&command.argv[0]);
        process.args(&command.argv[1..]);
        process.envs(&command.env);
        if let Some(workdir) = resolve_local_workdir(&self.request.spec, &cwd) {
            process.current_dir(workdir);
        }
        process.kill_on_drop(true);
        run_command(process, command, cwd).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        if command.argv.is_empty() {
            bail!("sandbox command requires at least one argv entry");
        }
        let cwd = command
            .cwd
            .clone()
            .unwrap_or_else(|| self.request.spec.default_workdir.clone());
        let mut process = Command::new(&command.argv[0]);
        process.args(&command.argv[1..]);
        process.envs(&command.env);
        if let Some(workdir) = resolve_local_workdir(&self.request.spec, &cwd) {
            process.current_dir(workdir);
        }
        process.kill_on_drop(true);
        spawn_sandbox_process(process, command).await
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("local-process sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        // The local-process backend runs commands directly on the host; there
        // is no container filesystem to capture. A meaningful implementation
        // would tar up the writable mounts, but the semantics differ enough
        // from container snapshots (no isolated filesystem, no rollback of
        // host-side state) that we don't pretend to support it.
        bail!("snapshot is not supported by the local-process sandbox backend")
    }
}

fn resolve_local_workdir(spec: &SandboxSpec, cwd: &str) -> Option<PathBuf> {
    let cwd_path = PathBuf::from(cwd);
    if cwd_path.is_absolute() {
        if let Some(mount) = spec.mounts.iter().find(|mount| mount.guest_path == cwd) {
            return Some(mount.host_path.clone());
        }
        return cwd_path.exists().then_some(cwd_path);
    }

    spec.mounts
        .iter()
        .find(|mount| mount.guest_path == cwd)
        .map(|mount| mount.host_path.clone())
}

fn materialize_durable_file_systems(
    key: &SandboxKey,
    file_systems: &[DurableFileSystem],
    configured_root: Option<&Path>,
) -> Result<Vec<SandboxMount>> {
    if file_systems.is_empty() {
        return Ok(Vec::new());
    }
    let root = durable_file_system_root(configured_root)?;
    file_systems
        .iter()
        .map(|file_system| {
            let host_path = root.join(stable_fnv1a_hex(&format!("{}\n{}", key, file_system.name)));
            std::fs::create_dir_all(&host_path).with_context(|| {
                format!(
                    "creating durable file system {} at {}",
                    file_system.name,
                    host_path.display()
                )
            })?;
            let host_path = std::fs::canonicalize(&host_path).with_context(|| {
                format!(
                    "canonicalizing durable file system {} at {}",
                    file_system.name,
                    host_path.display()
                )
            })?;
            Ok(SandboxMount {
                host_path,
                guest_path: file_system.mount_path.clone(),
                access: match file_system.mode {
                    crate::FileSystemMountMode::ReadOnly => SandboxMountAccess::ReadOnly,
                    crate::FileSystemMountMode::ReadWrite => SandboxMountAccess::ReadWrite,
                },
                internal: true,
            })
        })
        .collect()
}

fn validate_durable_file_system_mounts(
    mounts: &[SandboxMount],
    file_systems: &[DurableFileSystem],
) -> Result<()> {
    validate_durable_file_systems(file_systems)?;
    for mount in mounts {
        for file_system in file_systems {
            if mount.guest_path == file_system.mount_path {
                bail!(
                    "sandbox mount path is used by both a host mount and durable file system: {}",
                    file_system.mount_path
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_durable_file_systems(file_systems: &[DurableFileSystem]) -> Result<()> {
    for (index, file_system) in file_systems.iter().enumerate() {
        if file_system.name.trim().is_empty() {
            bail!("durable file system name must not be empty");
        }
        if file_system.name.contains('/') || file_system.name.contains('\0') {
            bail!(
                "durable file system name must not contain path separators or NUL bytes: {}",
                file_system.name
            );
        }
        if !file_system.mount_path.starts_with('/') {
            bail!(
                "durable file system mount path must be absolute: {}",
                file_system.mount_path
            );
        }
        if file_system.mount_path.trim() == "/" {
            bail!("durable file system mount path must not be /");
        }
        for prior in &file_systems[..index] {
            if prior.name == file_system.name {
                bail!("duplicate durable file system name: {}", file_system.name);
            }
            if prior.mount_path == file_system.mount_path {
                bail!(
                    "duplicate durable file system mount path: {}",
                    file_system.mount_path
                );
            }
        }
    }
    Ok(())
}

fn durable_file_system_root(configured_root: Option<&Path>) -> Result<PathBuf> {
    if let Some(value) = std::env::var_os(DURABLE_FILE_SYSTEM_ROOT_ENV) {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }
    if let Some(root) = configured_root {
        return Ok(root.to_path_buf());
    }
    if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Ok(path.join("exo").join("durable-filesystems"));
        }
    }
    if let Some(value) = std::env::var_os("HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Ok(path
                .join(".local")
                .join("share")
                .join("exo")
                .join("durable-filesystems"));
        }
    }
    bail!(
        "could not determine durable file system root: set {DURABLE_FILE_SYSTEM_ROOT_ENV}, XDG_DATA_HOME, or HOME"
    )
}

pub(crate) fn stable_fnv1a_hex(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

async fn touch_warm_sandbox(
    warm_sandboxes: &Arc<Mutex<HashMap<SandboxKey, WarmSandboxEntry>>>,
    key: &SandboxKey,
) {
    let mut warm_sandboxes = warm_sandboxes.lock().await;
    if let Some(entry) = warm_sandboxes.get_mut(key) {
        entry.last_used_at = Instant::now();
    }
}

async fn create_named_warm_sandbox(
    container_bin: &Path,
    request: &SandboxRequest,
    name: &str,
) -> Result<()> {
    let mut process = Command::new(container_bin);
    process
        .arg("run")
        .arg("--detach")
        .arg("--name")
        .arg(name)
        .arg("--label")
        .arg(format!("{WARM_SANDBOX_KEY_LABEL}={}", request.key))
        .arg("--label")
        .arg(format!(
            "{WARM_SANDBOX_SPEC_HASH_LABEL}={}",
            sandbox_spec_hash(&request.spec)
        ))
        .arg("--label")
        .arg(format!(
            "{WARM_SANDBOX_OWNER_PID_LABEL}={}",
            std::process::id()
        ))
        .arg("--workdir")
        .arg(&request.spec.default_workdir);

    configure_network_args(
        &mut process,
        request.spec.network,
        Some(DEFAULT_ENABLED_NETWORK_NAME),
    );
    configure_mount_args(&mut process, &request.spec.mounts);

    process.arg(&request.spec.image);
    process.args(WARM_SANDBOX_KEEPALIVE_ARGV);
    process.kill_on_drop(true);

    let output = process.output().await?;
    if !output.status.success() {
        let stderr = render_command_error(&output.stderr);
        return Err(anyhow!("failed to start warm sandbox {}: {}", name, stderr));
    }

    Ok(())
}

async fn create_unique_warm_sandbox(
    container_bin: &Path,
    request: &SandboxRequest,
) -> Result<String> {
    for _ in 0..4 {
        let name = new_warm_container_name(&request.key);
        match create_named_warm_sandbox(container_bin, request, &name).await {
            Ok(()) => return Ok(name),
            Err(err) if is_already_exists_error(&err.to_string()) => continue,
            Err(err) => return Err(err),
        }
    }

    Err(anyhow!(
        "failed to allocate a unique warm sandbox name for {}",
        request.key
    ))
}

async fn find_running_warm_sandbox(
    container_bin: &Path,
    cli: ContainerCliFlavor,
    request: &SandboxRequest,
) -> Result<Option<String>> {
    match cli {
        ContainerCliFlavor::AppleContainer => {
            find_running_apple_container_warm_sandbox(container_bin, request).await
        }
        ContainerCliFlavor::Docker => {
            find_running_docker_warm_sandbox(container_bin, request).await
        }
    }
}

async fn find_running_apple_container_warm_sandbox(
    container_bin: &Path,
    request: &SandboxRequest,
) -> Result<Option<String>> {
    let output = run_container_admin_command(
        container_bin,
        WARM_SANDBOX_CLEANUP_TIMEOUT,
        ["list", "--format", "json"],
    )
    .await?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to list warm sandboxes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let spec_hash = sandbox_spec_hash(&request.spec);
    let containers: Vec<ContainerListItem> = serde_json::from_slice(&output.stdout)?;
    Ok(containers.into_iter().find_map(|container| {
        if !container
            .status
            .as_deref()
            .is_some_and(is_running_container_status)
        {
            return None;
        }
        let labels = &container.configuration.labels;
        let key_matches = labels
            .get(WARM_SANDBOX_KEY_LABEL)
            .is_some_and(|value| value == &request.key.to_string());
        let spec_matches = labels
            .get(WARM_SANDBOX_SPEC_HASH_LABEL)
            .is_some_and(|value| value == &spec_hash);
        (key_matches && spec_matches).then_some(container.configuration.id)
    }))
}

async fn find_running_docker_warm_sandbox(
    container_bin: &Path,
    request: &SandboxRequest,
) -> Result<Option<String>> {
    let spec_hash = sandbox_spec_hash(&request.spec);
    let key_filter = format!("label={WARM_SANDBOX_KEY_LABEL}={}", request.key);
    let spec_filter = format!("label={WARM_SANDBOX_SPEC_HASH_LABEL}={spec_hash}");
    let output = run_container_admin_command(
        container_bin,
        WARM_SANDBOX_CLEANUP_TIMEOUT,
        [
            "ps",
            "--filter",
            key_filter.as_str(),
            "--filter",
            spec_filter.as_str(),
            "--filter",
            "status=running",
            "--format",
            "{{.Names}}",
        ],
    )
    .await?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to list warm sandboxes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|name| !name.is_empty())
        .map(ToOwned::to_owned))
}

async fn ensure_warm_sandbox_ready(
    container_bin: &Path,
    cli: ContainerCliFlavor,
    request: &SandboxRequest,
    warm_sandboxes: &Arc<Mutex<HashMap<SandboxKey, WarmSandboxEntry>>>,
) -> Result<String> {
    let healthcheck = SandboxCommand {
        argv: vec!["/bin/true".to_string()],
        env: HashMap::new(),
        display_argv: Some(vec!["/bin/true".to_string()]),
        cwd: None,
        timeout: Some(WARM_SANDBOX_HEALTHCHECK_TIMEOUT),
    };

    let mut warm_sandboxes = warm_sandboxes.lock().await;
    let (current_name, current_owned) = match warm_sandboxes.get_mut(&request.key) {
        Some(entry) if entry.request.spec == request.spec => {
            entry.last_used_at = Instant::now();
            (entry.name.clone(), entry.owned)
        }
        Some(_) => {
            let stale = warm_sandboxes
                .remove(&request.key)
                .expect("entry disappeared while locked");
            if stale.owned {
                schedule_cleanup_named_container(container_bin.to_path_buf(), cli, stale.name);
            }
            let (name, owned) = match find_running_warm_sandbox(container_bin, cli, request).await?
            {
                Some(name) => (name, false),
                None => (
                    create_unique_warm_sandbox(container_bin, request).await?,
                    true,
                ),
            };
            warm_sandboxes.insert(
                request.key.clone(),
                WarmSandboxEntry {
                    name: name.clone(),
                    request: request.clone(),
                    last_used_at: Instant::now(),
                    owned,
                },
            );
            return Ok(name);
        }
        None => {
            let (name, owned) = match find_running_warm_sandbox(container_bin, cli, request).await?
            {
                Some(name) => (name, false),
                None => (
                    create_unique_warm_sandbox(container_bin, request).await?,
                    true,
                ),
            };
            warm_sandboxes.insert(
                request.key.clone(),
                WarmSandboxEntry {
                    name: name.clone(),
                    request: request.clone(),
                    last_used_at: Instant::now(),
                    owned,
                },
            );
            return Ok(name);
        }
    };

    let healthy = matches!(
        exec_warm(container_bin, &current_name, &request.spec, &healthcheck).await,
        Ok(output) if output.ok
    );
    if healthy {
        return Ok(current_name);
    }

    let (replacement_name, owned) =
        match find_running_warm_sandbox(container_bin, cli, request).await? {
            Some(name) => (name, false),
            None => (
                create_unique_warm_sandbox(container_bin, request).await?,
                true,
            ),
        };
    warm_sandboxes.insert(
        request.key.clone(),
        WarmSandboxEntry {
            name: replacement_name.clone(),
            request: request.clone(),
            last_used_at: Instant::now(),
            owned,
        },
    );
    drop(warm_sandboxes);
    if current_owned {
        schedule_cleanup_named_container(container_bin.to_path_buf(), cli, current_name);
    }
    Ok(replacement_name)
}

async fn exec_one_shot(
    container_bin: &Path,
    spec: &SandboxSpec,
    network_name: Option<&'static str>,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }

    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());

    let mut process = Command::new(container_bin);
    process.arg("run").arg("--rm").arg("--workdir").arg(&cwd);
    configure_network_args(&mut process, spec.network, network_name);
    configure_mount_args(&mut process, &spec.mounts);
    configure_env_args(&mut process, &command.env);
    process.arg(&spec.image);
    process.args(&command.argv);
    process.kill_on_drop(true);

    run_command(process, command, cwd).await
}

async fn start_one_shot_process(
    container_bin: &Path,
    spec: &SandboxSpec,
    network_name: Option<&'static str>,
    command: &SandboxCommand,
) -> Result<crate::SandboxProcessParts> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }

    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());

    let mut process = Command::new(container_bin);
    process
        .arg("run")
        .arg("--rm")
        .arg("--interactive")
        .arg("--workdir")
        .arg(&cwd);
    configure_network_args(&mut process, spec.network, network_name);
    configure_mount_args(&mut process, &spec.mounts);
    configure_env_args(&mut process, &command.env);
    process.arg(&spec.image);
    process.args(&command.argv);
    process.kill_on_drop(true);

    spawn_sandbox_process(process, command).await
}

async fn exec_warm(
    container_bin: &Path,
    name: &str,
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

    let mut process = Command::new(container_bin);
    process.arg("exec");
    if !cwd.is_empty() {
        process.arg("--workdir").arg(&cwd);
    }
    configure_env_args(&mut process, &command.env);
    process.arg(name);
    process.args(&command.argv);
    process.kill_on_drop(true);

    run_command(process, command, cwd).await
}

async fn start_warm_process(
    container_bin: &Path,
    name: &str,
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

    let mut process = Command::new(container_bin);
    process.arg("exec").arg("--interactive");
    if !cwd.is_empty() {
        process.arg("--workdir").arg(&cwd);
    }
    configure_env_args(&mut process, &command.env);
    process.arg(name);
    process.args(&command.argv);
    process.kill_on_drop(true);

    spawn_sandbox_process(process, command).await
}

async fn run_command(
    mut process: Command,
    command: &SandboxCommand,
    cwd: String,
) -> Result<SandboxCommandOutput> {
    let output = match command.timeout {
        Some(timeout) => match time::timeout(timeout, process.output()).await {
            Ok(output) => output?,
            Err(_) => {
                return Err(anyhow!(
                    "sandbox command timed out after {}s: {}",
                    timeout.as_secs(),
                    command
                        .display_argv
                        .as_ref()
                        .unwrap_or(&command.argv)
                        .join(" ")
                ));
            }
        },
        None => process.output().await?,
    };

    Ok(SandboxCommandOutput {
        ok: output.status.success(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        command: command
            .display_argv
            .clone()
            .unwrap_or_else(|| command.argv.clone()),
        cwd,
    })
}

async fn spawn_sandbox_process(
    mut process: Command,
    command: &SandboxCommand,
) -> Result<crate::SandboxProcessParts> {
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn().with_context(|| {
        format!(
            "failed to start sandbox command: {}",
            command
                .display_argv
                .as_ref()
                .unwrap_or(&command.argv)
                .join(" ")
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("sandbox process did not expose stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("sandbox process did not expose stderr"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("sandbox process did not expose stdin"))?;

    Ok(crate::SandboxProcessParts {
        stdout: Box::pin(stdout.compat()),
        stderr: Box::pin(stderr.compat()),
        stdin: Box::pin(stdin.compat_write()),
        wait: wait_for_child(child),
    })
}

fn wait_for_child(mut child: Child) -> BoxFuture<'static, crate::Result<i32>> {
    Box::pin(async move {
        let status = child.wait().await?;
        Ok(status.code().unwrap_or_default())
    })
}

fn configure_network_args(
    process: &mut Command,
    policy: SandboxNetworkPolicy,
    network_name: Option<&str>,
) {
    match policy {
        SandboxNetworkPolicy::Disabled => {
            process.arg("--network").arg("none");
        }
        SandboxNetworkPolicy::Enabled => {
            if let Some(network_name) = network_name {
                process.arg("--network").arg(network_name);
            }
        }
    }
}

fn configure_mount_args(process: &mut Command, mounts: &[SandboxMount]) {
    for mount in mounts {
        let mut volume = format!("{}:{}", mount.host_path.display(), mount.guest_path);
        if matches!(mount.access, SandboxMountAccess::ReadOnly) {
            volume.push_str(":ro");
        }
        process.arg("--volume").arg(volume);
    }
}

fn configure_env_args(process: &mut Command, env: &HashMap<String, String>) {
    for (key, value) in env {
        process.arg("--env").arg(format!("{key}={value}"));
    }
}

async fn cleanup_named_container(
    container_bin: &Path,
    cli: ContainerCliFlavor,
    name: &str,
) -> Result<()> {
    match cli {
        ContainerCliFlavor::AppleContainer => {
            kill_named_container_if_present(container_bin, name).await?;
            let delete = run_container_admin_command(
                container_bin,
                WARM_SANDBOX_CLEANUP_TIMEOUT,
                ["delete", name],
            )
            .await?;
            if !delete.status.success() {
                let stderr = String::from_utf8_lossy(&delete.stderr).trim().to_string();
                if !is_missing_container_error(&stderr) {
                    return Err(anyhow!(
                        "failed to delete warm sandbox {}: {}",
                        name,
                        stderr
                    ));
                }
            }
        }
        ContainerCliFlavor::Docker => {
            // `docker rm -f` is SIGKILL + remove in one shot. Avoids racing
            // the daemon's default 10s SIGTERM grace against our cleanup
            // timeout, which otherwise leaves Exited containers behind.
            let rm = run_container_admin_command(
                container_bin,
                WARM_SANDBOX_CLEANUP_TIMEOUT,
                ["rm", "-f", name],
            )
            .await?;
            if !rm.status.success() {
                let stderr = String::from_utf8_lossy(&rm.stderr).trim().to_string();
                if !is_missing_container_error(&stderr) {
                    return Err(anyhow!(
                        "failed to delete warm sandbox {}: {}",
                        name,
                        stderr
                    ));
                }
            }
        }
    }

    Ok(())
}

async fn kill_named_container_if_present(container_bin: &Path, name: &str) -> Result<()> {
    let kill =
        run_container_admin_command(container_bin, WARM_SANDBOX_CLEANUP_TIMEOUT, ["kill", name])
            .await?;
    if !kill.status.success() {
        let stderr = String::from_utf8_lossy(&kill.stderr).trim().to_string();
        if !is_missing_container_error(&stderr) && !is_container_not_running_error(&stderr) {
            return Err(anyhow!("failed to kill warm sandbox {}: {}", name, stderr));
        }
    }
    Ok(())
}

async fn reap_orphaned_warm_sandboxes(container_bin: &Path, cli: ContainerCliFlavor) -> Result<()> {
    match cli {
        ContainerCliFlavor::AppleContainer => {
            reap_orphaned_apple_warm_sandboxes(container_bin).await
        }
        ContainerCliFlavor::Docker => reap_orphaned_docker_warm_sandboxes(container_bin).await,
    }
}

async fn reap_orphaned_apple_warm_sandboxes(container_bin: &Path) -> Result<()> {
    let output = run_container_admin_command(
        container_bin,
        WARM_SANDBOX_CLEANUP_TIMEOUT,
        ["list", "--format", "json"],
    )
    .await?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to list warm sandboxes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let now = apple_absolute_time_now()?;
    let containers: Vec<ContainerListItem> = serde_json::from_slice(&output.stdout)?;
    for container in containers {
        if !container
            .configuration
            .labels
            .contains_key(WARM_SANDBOX_KEY_LABEL)
        {
            continue;
        }
        if let Some(owner_pid) = container
            .configuration
            .labels
            .get(WARM_SANDBOX_OWNER_PID_LABEL)
            && owner_pid_is_alive(owner_pid)
        {
            continue;
        }
        if !container
            .status
            .as_deref()
            .is_some_and(is_running_container_status)
        {
            continue;
        }
        let Some(started_date) = container.started_date else {
            continue;
        };
        if now - started_date < ORPHANED_WARM_SANDBOX_MIN_AGE.as_secs_f64() {
            continue;
        }
        if let Err(error) = cleanup_named_container(
            container_bin,
            ContainerCliFlavor::AppleContainer,
            &container.configuration.id,
        )
        .await
        {
            tracing::warn!(
                container_id = %container.configuration.id,
                %error,
                "failed to clean up orphaned warm sandbox"
            );
        }
    }

    Ok(())
}

async fn reap_orphaned_docker_warm_sandboxes(container_bin: &Path) -> Result<()> {
    let output = run_container_admin_command(
        container_bin,
        WARM_SANDBOX_CLEANUP_TIMEOUT,
        [
            "ps",
            "-aq",
            "--no-trunc",
            "--filter",
            "label=exo.sandbox.key",
        ],
    )
    .await?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to list warm sandboxes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let ids = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Ok(());
    }

    let mut inspect_args = Vec::with_capacity(ids.len() + 1);
    inspect_args.push("inspect".to_string());
    inspect_args.extend(ids);
    let inspect = run_container_admin_command(
        container_bin,
        WARM_SANDBOX_CLEANUP_TIMEOUT,
        inspect_args.iter().map(String::as_str),
    )
    .await?;
    if !inspect.status.success() {
        return Err(anyhow!(
            "failed to inspect warm sandboxes: {}",
            String::from_utf8_lossy(&inspect.stderr).trim()
        ));
    }

    let now = Utc::now();
    let min_age = chrono::Duration::from_std(ORPHANED_WARM_SANDBOX_MIN_AGE)
        .expect("orphaned warm sandbox age fits in chrono duration");
    let containers: Vec<DockerInspectItem> = serde_json::from_slice(&inspect.stdout)?;
    for container in containers {
        if !container.config.labels.contains_key(WARM_SANDBOX_KEY_LABEL) {
            continue;
        }
        if let Some(owner_pid) = container.config.labels.get(WARM_SANDBOX_OWNER_PID_LABEL)
            && owner_pid_is_alive(owner_pid)
        {
            continue;
        }
        if container.state.status.as_deref() != Some("running") {
            continue;
        }
        let Some(started_at) = container.state.started_at.as_deref() else {
            continue;
        };
        let started_at = parse_docker_timestamp(started_at)?;
        if now.signed_duration_since(started_at) < min_age {
            continue;
        }
        if let Err(error) =
            cleanup_named_container(container_bin, ContainerCliFlavor::Docker, &container.id).await
        {
            tracing::warn!(
                container_id = %container.id,
                %error,
                "failed to clean up orphaned warm sandbox"
            );
        }
    }

    Ok(())
}

fn apple_absolute_time_now() -> Result<f64> {
    let unix_now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(unix_now.as_secs_f64() - APPLE_ABSOLUTE_TIME_UNIX_OFFSET_SECONDS)
}

fn parse_docker_timestamp(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn owner_pid_is_alive(pid: &str) -> bool {
    if pid.parse::<u32>().is_err() {
        return false;
    }
    std::process::Command::new("ps")
        .arg("-p")
        .arg(pid)
        .arg("-o")
        .arg("pid=")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn schedule_cleanup_named_container(container_bin: PathBuf, cli: ContainerCliFlavor, name: String) {
    tokio::spawn(async move {
        if let Err(error) = cleanup_named_container(&container_bin, cli, &name).await {
            tracing::warn!(sandbox = %name, %error, "failed to clean up warm sandbox");
        }
    });
}

fn missing_container_cli_message(cli: ContainerCliFlavor, container_bin: &Path) -> String {
    match cli {
        ContainerCliFlavor::AppleContainer => format!(
            "apple-container sandbox backend requires the `{}` CLI; install Apple container CLI or use `--sandbox-backend local-process`",
            container_bin.display()
        ),
        ContainerCliFlavor::Docker => format!(
            "docker sandbox backend requires the `{}` CLI; install Docker or use `--sandbox-backend local-process`",
            container_bin.display()
        ),
    }
}

async fn run_container_admin_command<I, S>(
    container_bin: &Path,
    timeout: Duration,
    args: I,
) -> Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<OsString>>();
    let display_args = args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = Command::new(container_bin);
    command.args(&args).kill_on_drop(true);
    match time::timeout(timeout, command.output()).await {
        Ok(output) => Ok(output?),
        Err(_) => Err(anyhow!(
            "container {} timed out after {}s",
            display_args,
            timeout.as_secs()
        )),
    }
}

fn is_missing_container_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("not found") || lower.contains("no such")
}

fn is_container_not_running_error(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("container is not running")
}

fn is_running_container_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("running")
}

fn is_already_exists_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("already exists")
}

pub(crate) fn sandbox_spec_hash(spec: &SandboxSpec) -> String {
    let mut hasher = DefaultHasher::new();
    spec.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn render_command_error(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}

fn network_name_for_policy(policy: SandboxNetworkPolicy) -> Option<&'static str> {
    matches!(policy, SandboxNetworkPolicy::Enabled).then_some(DEFAULT_ENABLED_NETWORK_NAME)
}

fn new_warm_container_name(key: &SandboxKey) -> String {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    let generation = Uuid::new_v4().simple().to_string();
    format!("exo-{hash:016x}-{}", &generation[..8])
}

/// Capture a docker container's filesystem state as a SnapshotPayload.
///
/// Pipeline:
///   docker commit -p <container>  exo-snap-<uuid>     // image from container fs
///   docker save exo-snap-<uuid>                       // tarball on stdout
///   docker image rm exo-snap-<uuid>                   // local image no longer needed
///
/// `commit -p` pauses the container during commit to ensure a consistent
/// filesystem capture (no half-written files).
async fn docker_snapshot_container(
    container_bin: &Path,
    container_name: &str,
) -> Result<SnapshotPayload> {
    let snap_tag = format!("exo-snap-{}", Uuid::new_v4().simple());

    let commit_output = Command::new(container_bin)
        .arg("commit")
        .arg("-p")
        .arg(container_name)
        .arg(&snap_tag)
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running `docker commit` for {container_name}"))?;
    if !commit_output.status.success() {
        bail!(
            "docker commit {container_name} {snap_tag} failed: {}",
            render_command_error(&commit_output.stderr)
        );
    }

    let save_output = Command::new(container_bin)
        .arg("save")
        .arg(&snap_tag)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running `docker save {snap_tag}`"))?;
    if !save_output.status.success() {
        // Best-effort cleanup of the tag we just created.
        let _ = Command::new(container_bin)
            .arg("image")
            .arg("rm")
            .arg(&snap_tag)
            .kill_on_drop(true)
            .output()
            .await;
        bail!(
            "docker save {snap_tag} failed: {}",
            render_command_error(&save_output.stderr)
        );
    }
    let bytes = Bytes::from(save_output.stdout);

    // Remove the local image tag now that the bytes are captured — the
    // canonical store of the snapshot is exoharness, not the docker daemon.
    let rm_output = Command::new(container_bin)
        .arg("image")
        .arg("rm")
        .arg(&snap_tag)
        .kill_on_drop(true)
        .output()
        .await;
    if let Ok(output) = &rm_output
        && !output.status.success()
    {
        tracing::warn!(
            image_tag = %snap_tag,
            stderr = %render_command_error(&output.stderr),
            "failed to remove ephemeral snapshot image"
        );
    }

    Ok(SnapshotPayload {
        kind: SnapshotKind::DockerImageTar,
        bytes,
    })
}

/// Load a docker-save tarball back into the local docker daemon and return
/// the image reference docker assigned to it. The reference is what
/// subsequent `docker run` invocations use to start a container from this
/// snapshot's state.
async fn docker_load_image(container_bin: &Path, payload: &Bytes) -> Result<String> {
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new(container_bin)
        .arg("load")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning `docker load`")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("docker load: failed to acquire stdin"))?;
    stdin
        .write_all(payload)
        .await
        .context("writing snapshot bytes to `docker load` stdin")?;
    stdin.shutdown().await.ok();
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .context("waiting on `docker load`")?;
    if !output.status.success() {
        bail!(
            "docker load failed: {}",
            render_command_error(&output.stderr)
        );
    }

    // docker load prints lines like:
    //   Loaded image: <ref>
    //   Loaded image ID: sha256:<digest>
    // Prefer the named-tag line; fall back to image-ID.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Loaded image: ") {
            return Ok(rest.trim().to_string());
        }
    }
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Loaded image ID: ") {
            return Ok(rest.trim().to_string());
        }
    }
    bail!("docker load completed but no image reference found in output: {stdout}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn docker_warm_sandbox_lookup_uses_docker_ps_filters() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let script_path = temp_dir.path().join("docker");
        let args_path = temp_dir.path().join("args");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\n{{\n  for arg in \"$@\"; do\n    printf '%s\\n' \"$arg\"\n  done\n}} > '{}'\nprintf 'warm-name\\n'\n",
                args_path.display()
            ),
        )
        .expect("write fake docker script");
        let mut permissions = fs::metadata(&script_path)
            .expect("fake docker metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("chmod fake docker");

        let request = SandboxRequest {
            key: SandboxKey::ConversationSandbox {
                conversation_id: "conversation".to_string(),
                sandbox_id: "sandbox".to_string(),
            },
            spec: SandboxSpec {
                image: "docker.io/library/ubuntu:24.04".to_string(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                network: SandboxNetworkPolicy::Disabled,
                default_workdir: "/".to_string(),
            },
            lifecycle: SandboxLifecycleConfig {
                idle_ttl: Some(Duration::from_secs(60)),
            },
            provider_state: None,
        };
        let spec_hash = sandbox_spec_hash(&request.spec);

        let name = find_running_warm_sandbox(&script_path, ContainerCliFlavor::Docker, &request)
            .await
            .expect("find warm sandbox");
        assert_eq!(name.as_deref(), Some("warm-name"));

        let key_filter = format!("label={WARM_SANDBOX_KEY_LABEL}={}", request.key);
        let spec_filter = format!("label={WARM_SANDBOX_SPEC_HASH_LABEL}={spec_hash}");
        let args = fs::read_to_string(&args_path).expect("read fake docker args");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "ps",
                "--filter",
                key_filter.as_str(),
                "--filter",
                spec_filter.as_str(),
                "--filter",
                "status=running",
                "--format",
                "{{.Names}}",
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn docker_prepare_request_reaps_orphaned_warm_sandboxes_with_real_cli_commands() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let script_path = temp_dir.path().join("docker");
        let args_path = temp_dir.path().join("args");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nfor arg in \"$@\"; do\n  printf '%s\\n' \"$arg\" >> '{}'\ndone\nprintf '%s\\n' '---' >> '{}'\ncase \"$1\" in\n  ps)\n    printf 'docker-container-id\\n'\n    ;;\n  inspect)\n    printf '%s\\n' '[{{\"Id\":\"docker-container-id\",\"Config\":{{\"Labels\":{{\"{}\":\"conversation:conversation:sandbox\"}}}},\"State\":{{\"Status\":\"running\",\"StartedAt\":\"2024-01-01T00:00:00Z\"}}}}]'\n    ;;\n  rm)\n    ;;\nesac\n",
                args_path.display(),
                args_path.display(),
                WARM_SANDBOX_KEY_LABEL,
            ),
        )
        .expect("write fake docker script");
        let mut permissions = fs::metadata(&script_path)
            .expect("fake docker metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("chmod fake docker");

        let backend = CliContainerSandboxBackend {
            cli: ContainerCliFlavor::Docker,
            container_bin: script_path,
            durable_file_system_root: None,
            system_started: Mutex::new(false),
            network_created: Mutex::new(false),
            warm_sandboxes: Arc::new(Mutex::new(HashMap::new())),
        };
        let request = SandboxRequest {
            key: SandboxKey::ConversationSandbox {
                conversation_id: "conversation".to_string(),
                sandbox_id: "sandbox".to_string(),
            },
            spec: SandboxSpec {
                image: "docker.io/library/ubuntu:24.04".to_string(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                network: SandboxNetworkPolicy::Disabled,
                default_workdir: "/".to_string(),
            },
            lifecycle: SandboxLifecycleConfig {
                idle_ttl: Some(Duration::from_secs(60)),
            },
            provider_state: None,
        };

        backend
            .prepare_request(request)
            .await
            .expect("prepare request");

        let args = fs::read_to_string(&args_path).expect("read fake docker args");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "ps",
                "-aq",
                "--no-trunc",
                "--filter",
                "label=exo.sandbox.key",
                "---",
                "inspect",
                "docker-container-id",
                "---",
                "rm",
                "-f",
                "docker-container-id",
                "---",
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apple_container_prepare_request_reaps_orphaned_warm_sandboxes_with_kill_delete() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let script_path = temp_dir.path().join("container");
        let args_path = temp_dir.path().join("args");
        fs::write(
            &script_path,
            format!(
                r#"#!/bin/sh
for arg in "$@"; do
  printf '%s\n' "$arg" >> '{}'
done
printf '%s\n' '---' >> '{}'
case "$1" in
  list)
    printf '%s\n' '[{{"configuration":{{"id":"apple-container-id","labels":{{"{}":"conversation:conversation:sandbox","{}":"999999"}}}},"status":"running","startedDate":0}}]'
    ;;
  kill)
    ;;
  delete)
    ;;
esac
"#,
                args_path.display(),
                args_path.display(),
                WARM_SANDBOX_KEY_LABEL,
                WARM_SANDBOX_OWNER_PID_LABEL,
            ),
        )
        .expect("write fake container script");
        let mut permissions = fs::metadata(&script_path)
            .expect("fake container metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("chmod fake container");

        let backend = CliContainerSandboxBackend {
            cli: ContainerCliFlavor::AppleContainer,
            container_bin: script_path,
            durable_file_system_root: None,
            system_started: Mutex::new(false),
            network_created: Mutex::new(false),
            warm_sandboxes: Arc::new(Mutex::new(HashMap::new())),
        };
        let request = SandboxRequest {
            key: SandboxKey::ConversationSandbox {
                conversation_id: "conversation".to_string(),
                sandbox_id: "sandbox".to_string(),
            },
            spec: SandboxSpec {
                image: "docker.io/library/ubuntu:24.04".to_string(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                network: SandboxNetworkPolicy::Disabled,
                default_workdir: "/".to_string(),
            },
            lifecycle: SandboxLifecycleConfig {
                idle_ttl: Some(Duration::from_secs(60)),
            },
            provider_state: None,
        };

        backend
            .prepare_request(request)
            .await
            .expect("prepare request");

        let args = fs::read_to_string(&args_path).expect("read fake container args");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "system",
                "start",
                "---",
                "list",
                "--format",
                "json",
                "---",
                "kill",
                "apple-container-id",
                "---",
                "delete",
                "apple-container-id",
                "---",
            ]
        );
    }

    #[tokio::test]
    async fn apple_container_backend_names_missing_container_cli() {
        let missing_bin = std::env::temp_dir().join(format!(
            "exo-missing-container-cli-{}",
            Uuid::new_v4().simple()
        ));
        let backend = CliContainerSandboxBackend {
            cli: ContainerCliFlavor::AppleContainer,
            container_bin: missing_bin,
            durable_file_system_root: None,
            system_started: Mutex::new(false),
            network_created: Mutex::new(false),
            warm_sandboxes: Arc::new(Mutex::new(HashMap::new())),
        };

        let error = backend.ensure_system_started().await.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("apple-container sandbox backend requires"));
        assert!(message.contains("install Apple container CLI"));
        assert!(message.contains("--sandbox-backend local-process"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn borrowed_docker_sandbox_execs_without_taking_container_ownership() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let script_path = temp_dir.path().join("docker");
        let args_path = temp_dir.path().join("args");
        fs::write(
            &script_path,
            format!(
                r#"#!/bin/sh
for arg in "$@"; do
  printf '%s\n' "$arg" >> '{}'
done
printf '%s\n' '---' >> '{}'
case "$1" in
  inspect)
    printf '%s\n' '[{{"Id":"canonical-id","Config":{{"Image":"task-image","WorkingDir":"/task","Labels":{{}}}},"State":{{"Status":"running"}}}}]'
    ;;
  exec)
    printf 'borrowed output'
    ;;
esac
"#,
                args_path.display(),
                args_path.display(),
            ),
        )
        .expect("write fake docker script");
        let mut permissions = fs::metadata(&script_path)
            .expect("fake docker metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("chmod fake docker");

        let backend = CliContainerSandboxBackend {
            cli: ContainerCliFlavor::Docker,
            container_bin: script_path,
            durable_file_system_root: None,
            system_started: Mutex::new(false),
            network_created: Mutex::new(false),
            warm_sandboxes: Arc::new(Mutex::new(HashMap::new())),
        };
        let request = SandboxRequest {
            key: SandboxKey::ConversationSandbox {
                conversation_id: "conversation".to_string(),
                sandbox_id: "sandbox".to_string(),
            },
            spec: SandboxSpec {
                image: "task-image".to_string(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                network: SandboxNetworkPolicy::Enabled,
                default_workdir: "/task".to_string(),
            },
            lifecycle: SandboxLifecycleConfig::default(),
            provider_state: None,
        };
        let handle = backend
            .attach(
                request,
                SandboxAttachment::DockerContainer {
                    container_id: "harbor-task".to_string(),
                },
            )
            .await
            .expect("attach container");
        let output = handle
            .exec(&SandboxCommand {
                argv: vec!["sh".to_string(), "-lc".to_string(), "pwd".to_string()],
                env: HashMap::new(),
                display_argv: None,
                cwd: None,
                timeout: None,
            })
            .await
            .expect("exec in borrowed container");
        assert_eq!(output.stdout, "borrowed output");
        assert!(handle.stop().await.is_err());
        assert_eq!(
            handle.detach().await.expect("detach borrowed container"),
            SandboxAttachment::DockerContainer {
                container_id: "canonical-id".to_string(),
            }
        );

        let args = fs::read_to_string(&args_path).expect("read fake docker args");
        assert!(args.contains("inspect\nharbor-task\n---"));
        assert!(args.contains("exec\n--workdir\n/task\ncanonical-id"));
        assert!(!args.lines().any(|arg| arg == "rm"));
        assert!(!args.lines().any(|arg| arg == "stop"));
        assert!(!args.lines().any(|arg| arg == "kill"));
    }
}

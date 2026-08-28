//! smolvm local microVM sandbox backend.
//!
//! Each sandbox is a VM with its own guest kernel (libkrun on Hypervisor.framework
//! / KVM / WHP) — the only local backend needing no daemon that runs on macOS,
//! Linux and Windows.
//!
//! [`SmolvmExecutionMode::Auto`] picks per request from `lifecycle.idle_ttl`, the
//! signal the Docker backend already uses for warm reuse: warm holds a named
//! machine and can snapshot, one-shot boots an ephemeral VM per exec and leaves
//! durable state to the host mounts.
//!
//! Snapshots are bytes-by-reference like E2B/Daytona: the payload is a manifest
//! pointing at a `.smolmachine` pack on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bytes::Bytes;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::OnceCell;

use crate::SandboxAttachment;
use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput, SandboxKey,
    SandboxMountAccess, SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotKind,
    SnapshotPayload, WARM_SANDBOX_KEY_LABEL, WARM_SANDBOX_OWNER_PID_LABEL, owner_pid_is_alive,
    run_command, spawn_sandbox_process,
};

/// Default binary name; overridable with `SMOLVM_BIN` for a non-PATH install.
const SMOLVM_BIN_ENV: &str = "SMOLVM_BIN";
const DEFAULT_SMOLVM_BIN: &str = "smolvm";
/// Also the variable smolvm itself reads, which is why the name is not ours to
/// choose; `--smolvm-boot-binary` is the discoverable way to set it.
const SMOLVM_BOOT_BIN_ENV: &str = "SMOLVM_BOOT_BINARY";

/// First smolvm release whose client drains the agent's `Progress` frames during
/// a detached start, which is what warm mode needs; see [`SmolvmExecutionMode::Warm`].
const MIN_WARM_VERSION: Version = Version::new(1, 7, 2);

/// Probed from `--help`, not the version: a build carrying `--label` still
/// reported 1.7.5, so a version gate would refuse a flag that is right there.
const LABEL_FLAG: &str = "--label";

/// What the installed smolvm supports. Probed once per backend.
#[derive(Debug, Clone, Copy)]
struct Capabilities {
    /// An image-backed machine can be started.
    warm: bool,
    /// `machine create --label`; without it, reaping cannot cross processes.
    labels: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SmolvmExecutionMode {
    /// Warm when the request and the installed smolvm both allow it, else one-shot.
    #[default]
    Auto,
    /// One ephemeral microVM per exec. Works on every smolvm release.
    OneShot,
    /// One persistent machine per sandbox key, joined by later execs.
    ///
    /// Needs smolvm >= 1.7.2: on 1.7.0/1.7.1 an image-backed start fails with
    /// `run container detached: unexpected response type`.
    Warm,
}

/// Everything the backend can be tuned with, so a caller configures it by
/// building a struct rather than by setting variables the type never mentions.
/// The clap args that fill this (`--smolvm-binary`, `--smolvm-boot-binary`)
/// carry `env` attributes, which keeps the historical `SMOLVM_*` variables
/// working while still listing them in `--help`.
#[derive(Debug, Clone, Default)]
pub struct SmolvmBackendConfig {
    pub mode: SmolvmExecutionMode,
    /// `smolvm` itself. `None` falls back to `SMOLVM_BIN`, then bare `smolvm`
    /// resolved through `PATH`.
    pub binary: Option<PathBuf>,
    /// The binary handed to smolvm as `SMOLVM_BOOT_BINARY`. `None` derives one
    /// from `binary` on first use; see [`resolve_boot_binary`].
    pub boot_binary: Option<PathBuf>,
}

/// Backend driving the `smolvm` CLI.
pub struct SmolvmSandboxBackend {
    binary: PathBuf,
    /// Configured boot binary, if the caller pinned one.
    boot_binary_override: Option<PathBuf>,
    /// Serves `_boot-vm`; arms the parent-death watchdog for ephemeral VMs.
    /// Derived on first use rather than in the constructor: deriving it walks
    /// `PATH` and stats candidates, and a constructor cannot await.
    boot_binary: OnceCell<Option<PathBuf>>,
    mode: SmolvmExecutionMode,
    /// Probed once: re-asking per `acquire` would spawn a process per sandbox.
    capabilities: OnceCell<Capabilities>,
    /// Last use of each warm machine this process created, for TTL reaping.
    warm_seen: Mutex<HashMap<String, Instant>>,
}

impl SmolvmSandboxBackend {
    /// Delegates to [`Default`], which holds the body — a reader looking for how
    /// an unconfigured backend is built finds it under the trait they expect.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_mode(mode: SmolvmExecutionMode) -> Self {
        Self::from_config(SmolvmBackendConfig {
            mode,
            ..Default::default()
        })
    }

    /// The env lookups below are the fallback for callers that build a backend
    /// without a config; anything routed through the CLI arrives on the struct.
    pub fn from_config(config: SmolvmBackendConfig) -> Self {
        let binary = config
            .binary
            .or_else(|| std::env::var_os(SMOLVM_BIN_ENV).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SMOLVM_BIN));
        let boot_binary_override = config
            .boot_binary
            .or_else(|| std::env::var_os(SMOLVM_BOOT_BIN_ENV).map(PathBuf::from));
        Self {
            binary,
            boot_binary_override,
            boot_binary: OnceCell::new(),
            mode: config.mode,
            capabilities: OnceCell::new(),
            warm_seen: Mutex::new(HashMap::new()),
        }
    }

    /// Resolved once and cached: every ephemeral `acquire` needs it, and the
    /// resolution touches the filesystem.
    async fn boot_binary(&self) -> &Option<PathBuf> {
        self.boot_binary
            .get_or_init(|| async {
                match &self.boot_binary_override {
                    Some(explicit) => Some(explicit.clone()),
                    None => resolve_boot_binary(&self.binary).await,
                }
            })
            .await
    }

    /// The configured mode, which may still be `Auto` until a request resolves it.
    pub fn mode(&self) -> SmolvmExecutionMode {
        self.mode
    }

    /// Probe the installed binary once and cache what it supports.
    async fn capabilities(&self) -> &Capabilities {
        self.capabilities
            .get_or_init(|| async {
                Capabilities {
                    warm: self
                        .probe_version()
                        .await
                        .is_some_and(|version| version >= MIN_WARM_VERSION),
                    labels: self.probe_flag("machine", "create", LABEL_FLAG).await,
                }
            })
            .await
    }

    /// An unreadable version counts as "no"; a missing binary then fails on the
    /// first real command, which reports it properly.
    pub async fn warm_supported(&self) -> bool {
        self.capabilities().await.warm
    }

    /// Whether the installed smolvm can label machines, which cross-process reaping needs.
    pub async fn labels_supported(&self) -> bool {
        self.capabilities().await.labels
    }

    /// Whether a subcommand advertises `flag` in its own `--help`.
    async fn probe_flag(&self, group: &str, subcommand: &str, flag: &str) -> bool {
        let Ok(output) = Command::new(&self.binary)
            .args([group, subcommand, "--help"])
            .output()
            .await
        else {
            return false;
        };
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains(flag)
    }

    /// The mode this request will actually run under: `idle_ttl` decides, and
    /// the installed smolvm is only a capability gate on top.
    async fn resolve_mode(&self, request: &SandboxRequest) -> SmolvmExecutionMode {
        match self.mode {
            // Explicit is a decision, not a hint.
            SmolvmExecutionMode::OneShot => return SmolvmExecutionMode::OneShot,
            SmolvmExecutionMode::Warm => return SmolvmExecutionMode::Warm,
            SmolvmExecutionMode::Auto => {}
        }
        if request.lifecycle.idle_ttl.is_none() {
            // No warm lifetime asked for: a persistent VM would outlive the caller.
            return SmolvmExecutionMode::OneShot;
        }
        if self.warm_supported().await {
            SmolvmExecutionMode::Warm
        } else {
            SmolvmExecutionMode::OneShot
        }
    }

    async fn probe_version(&self) -> Option<Version> {
        let output = Command::new(&self.binary)
            .arg("--version")
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_version(&String::from_utf8_lossy(&output.stdout))
    }

    /// Boot the machine backing `name`, creating it first when absent.
    ///
    /// Idempotent by *result*, not by pre-check: two `acquire`s for one key race,
    /// and the winner's machine is exactly what the loser wanted.
    async fn ensure_machine_started(
        &self,
        name: &str,
        spec: &SandboxSpec,
        key: &SandboxKey,
    ) -> Result<()> {
        let mut create = Command::new(&self.binary);
        create.arg("machine").arg("create").arg("--name").arg(name);
        create.arg("--image").arg(&spec.image);
        self.stamp_labels(&mut create, key).await;
        configure_spec_args(&mut create, spec);
        // Keepalive so the machine stays up between execs, as the Docker backend does.
        create.arg("--").arg("sleep").arg("infinity");
        let output = create
            .output()
            .await
            .context("spawn smolvm machine create")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !cli_says::already_exists(&stderr) {
                bail!("smolvm machine create failed: {}", stderr.trim());
            }
        }

        let mut start = Command::new(&self.binary);
        start.arg("machine").arg("start").arg("--name").arg(name);
        let output = start.output().await.context("spawn smolvm machine start")?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Already up is the caller's intent, not an error.
        if cli_says::already_running(&stderr) {
            return Ok(());
        }
        bail!(
            "smolvm machine start failed for '{name}': {}",
            stderr.trim()
        );
    }

    /// Drop warm machines this process created that are idle past `idle_ttl`.
    /// Idle age lives in memory, so [`Self::reap_abandoned_machines`] covers
    /// machines stranded by an earlier process.
    async fn reap_idle_machines(&self, request: &SandboxRequest, current: &str) {
        let Some(ttl) = request.lifecycle.idle_ttl else {
            return;
        };
        let expired: Vec<String> = {
            // Scoped so the guard is dropped before any await.
            let Ok(mut seen) = self.warm_seen.lock() else {
                return;
            };
            let now = Instant::now();
            seen.insert(current.to_string(), now);
            let expired: Vec<String> = seen
                .iter()
                .filter(|(name, last)| name.as_str() != current && now.duration_since(**last) > ttl)
                .map(|(name, _)| name.clone())
                .collect();
            for name in &expired {
                seen.remove(name);
            }
            expired
        };
        for name in expired {
            match self.delete_machine_if_present(&name).await {
                Ok(()) => tracing::info!(machine = %name, "reaped idle smolvm machine"),
                Err(error) => {
                    tracing::warn!(machine = %name, %error, "failed to reap idle smolvm machine")
                }
            }
        }
    }

    /// Record which sandbox a machine serves and which process owns it, under the
    /// same keys the Docker backend uses. A no-op without `--label`.
    async fn stamp_labels(&self, command: &mut Command, key: &SandboxKey) {
        if !self.labels_supported().await {
            return;
        }
        command
            .arg("--label")
            .arg(format!("{WARM_SANDBOX_KEY_LABEL}={key}"))
            .arg("--label")
            .arg(format!(
                "{WARM_SANDBOX_OWNER_PID_LABEL}={}",
                std::process::id()
            ));
    }

    /// Reclaim labelled machines whose owning process is gone — a crash or a
    /// restart leaves nobody to expire them. A live owner is left alone: two
    /// harnesses may share a host, and reaping a peer's sandbox mid-turn is worse
    /// than leaking one.
    async fn reap_abandoned_machines(&self, current: &str) {
        let machines = match self.labelled_machines().await {
            Ok(machines) => machines,
            Err(error) => {
                tracing::debug!(%error, "could not list smolvm machines for reaping");
                return;
            }
        };
        for (name, owner) in machines {
            if name == current || owner_pid_is_alive(&owner) {
                continue;
            }
            match self.delete_machine_if_present(&name).await {
                Ok(()) => tracing::info!(machine = %name, owner, "reaped abandoned smolvm machine"),
                Err(error) => {
                    tracing::warn!(machine = %name, owner, %error, "failed to reap abandoned machine")
                }
            }
        }
    }

    /// `(name, owner pid)` for machines carrying this backend's labels. Reads
    /// `--json`: the table view truncates names and omits labels entirely.
    async fn labelled_machines(&self) -> Result<Vec<(String, String)>> {
        let output = Command::new(&self.binary)
            .args(["machine", "ls", "--json"])
            .output()
            .await
            .context("spawn smolvm machine ls --json")?;
        if !output.status.success() {
            bail!(
                "smolvm machine ls failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let parsed: Value =
            serde_json::from_slice(&output.stdout).context("parse smolvm machine ls --json")?;
        let items = parsed
            .as_array()
            .cloned()
            .or_else(|| parsed.get("machines")?.as_array().cloned())
            .unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(|item| {
                let labels = item.get("labels")?;
                // The key label is what marks a machine as ours.
                labels.get(WARM_SANDBOX_KEY_LABEL)?;
                let name = item.get("name")?.as_str()?.to_string();
                let owner = labels
                    .get(WARM_SANDBOX_OWNER_PID_LABEL)?
                    .as_str()?
                    .to_string();
                Some((name, owner))
            })
            .collect())
    }

    /// Delete if present, tolerating "not found". Deliberately not a `machine ls`
    /// pre-check: that view truncates names at 15 chars and ours are 20, so the
    /// match could never hit — and asking outright has no check-then-act race.
    async fn delete_machine_if_present(&self, name: &str) -> Result<()> {
        let output = Command::new(&self.binary)
            .args(["machine", "delete", "--name", name, "--force"])
            .output()
            .await
            .context("spawn smolvm machine delete")?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if cli_says::no_such_machine(&stderr) {
            return Ok(());
        }
        bail!("smolvm machine delete failed: {}", stderr.trim())
    }
}

impl Default for SmolvmSandboxBackend {
    fn default() -> Self {
        Self::with_mode(SmolvmExecutionMode::default())
    }
}

#[async_trait]
impl ManagedSandboxBackend for SmolvmSandboxBackend {
    fn is_local(&self) -> bool {
        true
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_unsupported_spec(&request.spec)?;
        match self.resolve_mode(&request).await {
            SmolvmExecutionMode::Warm => {
                let machine = machine_name(&request.key);
                self.ensure_machine_started(&machine, &request.spec, &request.key)
                    .await?;
                self.reap_idle_machines(&request, &machine).await;
                if self.labels_supported().await {
                    self.reap_abandoned_machines(&machine).await;
                }
                Ok(Arc::new(SmolvmWarmHandle {
                    id: format!("smolvm:{machine}"),
                    binary: self.binary.clone(),
                    machine,
                    request,
                }))
            }
            // `Auto` is resolved by `resolve_mode`, so it never reaches here.
            _ => Ok(Arc::new(SmolvmOneShotHandle {
                id: format!("smolvm-oneshot:{}", request.key),
                binary: self.binary.clone(),
                boot_binary: self.boot_binary().await.clone(),
                request,
            })),
        }
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        // `SandboxAttachment` only models Docker containers today.
        bail!("smolvm sandboxes cannot be attached")
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        if payload.kind != SnapshotKind::SmolMachinePack {
            bail!(
                "smolvm backend cannot restore snapshot kind {:?}",
                payload.kind
            );
        }
        reject_unsupported_spec(&request.spec)?;
        if self.resolve_mode(&request).await != SmolvmExecutionMode::Warm {
            bail!(
                "smolvm snapshots require warm mode (one-shot VMs hold no state to restore); \
                 warm needs smolvm >= {MIN_WARM_VERSION}"
            );
        }

        let manifest: SmolvmSnapshotManifest =
            serde_json::from_slice(&payload.bytes).context("parse smolvm snapshot manifest")?;
        if !Path::new(&manifest.pack_path).exists() {
            bail!(
                "smolvm snapshot pack is missing at {} (packs are referenced by path, not embedded)",
                manifest.pack_path
            );
        }

        let machine = machine_name(&request.key);
        // Unconditional: delete already tolerates "not found".
        self.delete_machine_if_present(&machine).await?;

        let mut create = Command::new(&self.binary);
        create
            .arg("machine")
            .arg("create")
            .arg("--name")
            .arg(&machine)
            .arg("--from")
            .arg(&manifest.pack_path);
        // A restored machine is ours too, or reaping would never see it.
        self.stamp_labels(&mut create, &request.key).await;
        configure_spec_args(&mut create, &request.spec);
        run_checked(create, "smolvm machine create --from").await?;

        let mut start = Command::new(&self.binary);
        start
            .arg("machine")
            .arg("start")
            .arg("--name")
            .arg(&machine);
        run_checked(start, "smolvm machine start").await?;

        Ok(Arc::new(SmolvmWarmHandle {
            id: format!("smolvm:{machine}"),
            binary: self.binary.clone(),
            machine,
            request,
        }))
    }
}

/// Ephemeral-VM handle: one `smolvm machine run` per command.
struct SmolvmOneShotHandle {
    id: String,
    binary: PathBuf,
    boot_binary: Option<PathBuf>,
    request: SandboxRequest,
}

impl SmolvmOneShotHandle {
    fn build(&self, command: &SandboxCommand, cwd: &str) -> Command {
        let mut process = Command::new(&self.binary);
        process.arg("machine").arg("run");
        process.arg("--image").arg(&self.request.spec.image);
        configure_spec_args(&mut process, &self.request.spec);
        configure_command_args(&mut process, command, cwd);
        // Arms smolvm's parent-death watchdog so the VM dies with a SIGKILLed CLI
        // rather than reparenting to init. Ephemeral runs only.
        if let Some(boot) = &self.boot_binary {
            process.env("SMOLVM_BOOT_BINARY", boot);
        }
        process.arg("--");
        process.args(&command.argv);
        process.kill_on_drop(true);
        process
    }
}

#[async_trait]
impl ManagedSandboxHandle for SmolvmOneShotHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn effective_image(&self) -> Option<String> {
        Some(self.request.spec.image.clone())
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let cwd = resolve_cwd(command, &self.request.spec);
        let process = self.build(command, &cwd);
        run_command(process, &with_backstop_timeout(command), cwd).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        let cwd = resolve_cwd(command, &self.request.spec);
        let process = self.build(command, &cwd);
        spawn_sandbox_process(process, command).await
    }

    async fn stop(&self) -> Result<()> {
        // Ephemeral VMs are reclaimed when their command exits.
        Ok(())
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("one-shot smolvm sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!(
            "snapshot needs a persistent VM; construct the backend with SmolvmExecutionMode::Warm"
        )
    }
}

/// Persistent-machine handle: execs join a machine that stays booted.
struct SmolvmWarmHandle {
    id: String,
    binary: PathBuf,
    machine: String,
    request: SandboxRequest,
}

impl SmolvmWarmHandle {
    fn build(&self, command: &SandboxCommand, cwd: &str, interactive: bool) -> Command {
        let mut process = Command::new(&self.binary);
        process
            .arg("machine")
            .arg("exec")
            .arg("--name")
            .arg(&self.machine);
        if interactive {
            process.arg("--interactive");
        }
        configure_command_args(&mut process, command, cwd);
        process.arg("--");
        process.args(&command.argv);
        process.kill_on_drop(true);
        process
    }
}

#[async_trait]
impl ManagedSandboxHandle for SmolvmWarmHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn effective_image(&self) -> Option<String> {
        Some(self.request.spec.image.clone())
    }

    fn provider_state(&self) -> Option<Value> {
        Some(json!({ "machine": self.machine }))
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let cwd = resolve_cwd(command, &self.request.spec);
        let process = self.build(command, &cwd, false);
        run_command(process, &with_backstop_timeout(command), cwd).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        let cwd = resolve_cwd(command, &self.request.spec);
        let process = self.build(command, &cwd, true);
        spawn_sandbox_process(process, command).await
    }

    async fn stop(&self) -> Result<()> {
        let mut stop = Command::new(&self.binary);
        stop.arg("machine")
            .arg("stop")
            .arg("--name")
            .arg(&self.machine);
        run_checked(stop, "smolvm machine stop").await.map(|_| ())
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("smolvm sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        // `pack create --from-vm` re-pulls by manifest, so a VM built from a local
        // archive can never be packed: smolvm flattens those at boot.
        if is_local_image_ref(&self.request.spec.image) {
            bail!(
                "smolvm cannot snapshot a VM created from a local image ({}): \
                 `pack create --from-vm` needs a registry reference to re-pull. \
                 Use a registry image for sandboxes you intend to snapshot.",
                self.request.spec.image
            );
        }

        // `pack create --from-vm` reads a *stopped* VM's disks, so quiesce first.
        let mut stop = Command::new(&self.binary);
        stop.arg("machine")
            .arg("stop")
            .arg("--name")
            .arg(&self.machine);
        run_checked(stop, "smolvm machine stop").await?;

        let dir = snapshot_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create snapshot dir {}", dir.display()))?;
        // `-o` names the executable stub; smolvm writes `<stub>.smolmachine`
        // beside it and rejects being handed the sidecar path.
        let stub_path = dir.join(&self.machine);
        let pack_path = dir.join(format!("{}.smolmachine", self.machine));

        let mut pack = Command::new(&self.binary);
        pack.arg("pack")
            .arg("create")
            .arg("--from-vm")
            .arg(&self.machine)
            .arg("-o")
            .arg(&stub_path);
        run_checked(pack, "smolvm pack create --from-vm").await?;
        if !pack_path.exists() {
            bail!(
                "smolvm pack reported success but {} is missing",
                pack_path.display()
            );
        }

        let manifest = SmolvmSnapshotManifest {
            machine: self.machine.clone(),
            pack_path: pack_path.to_string_lossy().to_string(),
        };
        let bytes = serde_json::to_vec(&manifest).context("serialize smolvm snapshot manifest")?;

        // Leave the sandbox usable after snapshotting.
        let mut start = Command::new(&self.binary);
        start
            .arg("machine")
            .arg("start")
            .arg("--name")
            .arg(&self.machine);
        run_checked(start, "smolvm machine start").await?;

        Ok(SnapshotPayload {
            kind: SnapshotKind::SmolMachinePack,
            bytes: Bytes::from(bytes),
        })
    }
}

/// Bytes-by-reference snapshot manifest; the pack itself stays on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SmolvmSnapshotManifest {
    machine: String,
    pack_path: String,
}

fn snapshot_dir() -> PathBuf {
    std::env::temp_dir().join("exo-smolvm-snapshots")
}

fn resolve_cwd(command: &SandboxCommand, spec: &SandboxSpec) -> String {
    command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone())
}

/// Mounts and network policy, shared by the create/run paths.
fn configure_spec_args(process: &mut Command, spec: &SandboxSpec) {
    if spec.network == SandboxNetworkPolicy::Enabled {
        process.arg("--net");
    }
    for mount in &spec.mounts {
        let mut value = format!("{}:{}", mount.host_path.display(), mount.guest_path);
        if mount.access == SandboxMountAccess::ReadOnly {
            value.push_str(":ro");
        }
        process.arg("--volume").arg(value);
    }
}

/// Workdir, environment and timeout, shared by the run/exec paths.
///
/// The timeout goes down to smolvm because exo enforces deadlines by SIGKILLing
/// the CLI, which does not stop the VM it launched — every timed-out exec used to
/// strand a microVM. Enforced in-guest, the CLI exits and tears the VM down.
fn configure_command_args(process: &mut Command, command: &SandboxCommand, cwd: &str) {
    if !cwd.is_empty() {
        process.arg("--workdir").arg(cwd);
    }
    for (key, value) in &command.env {
        process.arg("--env").arg(format!("{key}={value}"));
    }
    if let Some(timeout) = command.timeout {
        process
            .arg("--timeout")
            .arg(format!("{}s", timeout.as_secs().max(1)));
    }
}

/// Pushes exo's backstop out so smolvm's in-guest timeout fires first.
const TIMEOUT_BACKSTOP_GRACE: Duration = Duration::from_secs(10);

fn with_backstop_timeout(command: &SandboxCommand) -> SandboxCommand {
    let mut backstop = command.clone();
    backstop.timeout = command.timeout.map(|t| t + TIMEOUT_BACKSTOP_GRACE);
    backstop
}

/// The binary to hand smolvm as `SMOLVM_BOOT_BINARY`, arming the boot child's
/// parent-death watchdog. Ephemeral runs only: a persistent machine is meant to
/// outlive the `machine start` that created it.
///
/// Prefers the sibling `smolvm-bin`, since a packaged `smolvm` may be a wrapper
/// script that cannot be exec'd as the boot binary.
///
/// An explicitly configured path short-circuits this in [`SmolvmSandboxBackend::boot_binary`].
async fn resolve_boot_binary(binary: &Path) -> Option<PathBuf> {
    let resolved = which_binary(binary).await?;
    let sibling = resolved.with_file_name("smolvm-bin");
    if is_file(&sibling).await {
        return Some(sibling);
    }
    Some(resolved)
}

/// Absolute path for a command that may be a bare name resolved through `PATH`.
async fn which_binary(binary: &Path) -> Option<PathBuf> {
    if binary.components().count() > 1 {
        return tokio::fs::canonicalize(binary).await.ok();
    }
    let path = std::env::var_os("PATH")?;
    // Sequential rather than concurrent: `PATH` order *is* the precedence rule,
    // and the first hit almost always wins on the first entry or two.
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if is_file(&candidate).await
            && let Ok(canonical) = tokio::fs::canonicalize(&candidate).await
        {
            return Some(canonical);
        }
    }
    None
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
}

/// smolvm has no named durable filesystem; refuse rather than hand back a sandbox
/// missing storage the caller asked for, as the Daytona backend does.
fn reject_unsupported_spec(spec: &SandboxSpec) -> Result<()> {
    if !spec.durable_file_systems.is_empty() {
        let names: Vec<&str> = spec
            .durable_file_systems
            .iter()
            .map(|fs| fs.name.as_str())
            .collect();
        bail!(
            "the smolvm backend cannot provide durable file systems ({}); \
             use spec mounts, which map to smolvm volumes",
            names.join(", ")
        );
    }
    // smolvm resolves registry references over the machine's own network and
    // refuses this combination even for a cached image. Caught here so the caller
    // gets the two real remedies, not a failure deep in the CLI output.
    if spec.network == SandboxNetworkPolicy::Disabled && !is_local_image_ref(&spec.image) {
        bail!(
            "smolvm cannot use registry image '{}' in a network-disabled sandbox: \
             it resolves registry references over the machine's network, even for \
             cached images. Either set SandboxNetworkPolicy::Enabled, or supply the \
             image locally (a `docker save` tar path or an unpacked rootfs dir), \
             which keeps the sandbox fully network-isolated.",
            spec.image
        );
    }
    Ok(())
}

/// The one place this backend depends on smolvm's error *wording*, kept together
/// so a reworded message is a one-line fix rather than a scattered hunt.
mod cli_says {
    /// A create that lost a race: "already exists or is being created".
    pub fn already_exists(stderr: &str) -> bool {
        stderr.contains("already exists") || stderr.contains("is being created")
    }

    /// Already up, or coming up under a racing create.
    pub fn already_running(stderr: &str) -> bool {
        stderr.contains("already running") || stderr.contains("is being created")
    }

    /// A delete for a machine that is not there — the desired end state.
    pub fn no_such_machine(stderr: &str) -> bool {
        stderr.contains("not found") || stderr.contains("does not exist")
    }
}

/// The version reported by `smolvm --version`; `None` means "assume old".
///
/// Comparison is delegated to `semver` rather than hand-rolled, so precedence
/// follows the spec — notably that a prerelease sorts BELOW its release, which a
/// tuple comparison silently got wrong (`1.7.2-rc.1` used to satisfy a `>= 1.7.2`
/// gate even though it predates the fix that gate exists to require).
///
/// The input still needs a little normalizing before `semver` will take it:
/// `--version` prints `smolvm 1.7.5`, sometimes with a `v` prefix, and a
/// two-component `1.7` is not valid semver but is worth accepting as `1.7.0`.
fn parse_version(output: &str) -> Option<Version> {
    let token = output
        .split_whitespace()
        .map(|word| word.trim_start_matches('v'))
        .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    Version::parse(token)
        .ok()
        // `1.7` / `1` are not semver; retry with the missing components as zero
        // rather than reporting an unreadable version and disabling warm mode.
        .or_else(|| {
            let core = token.split(['-', '+']).next().unwrap_or(token);
            let mut parts = core.split('.').map(|p| p.parse::<u64>());
            let major = parts.next()?.ok()?;
            let minor = parts.next().transpose().ok()?.unwrap_or(0);
            let patch = parts.next().transpose().ok()?.unwrap_or(0);
            Some(Version::new(major, minor, patch))
        })
}

/// Whether an image reference names local disk rather than a registry: a tar, an
/// OCI archive or a rootfs dir, all flattened at boot with no manifest to re-pull.
fn is_local_image_ref(image: &str) -> bool {
    image == "-"
        || image.starts_with('/')
        || image.starts_with("./")
        || image.starts_with("../")
        || image.ends_with(".tar")
        || image.ends_with(".tar.gz")
        || image.ends_with(".tgz")
        || Path::new(image).exists()
}

/// Stable, filesystem-safe machine name for a sandbox key. FNV-1a rather than
/// `DefaultHasher`, whose output is not stable across processes or releases.
fn machine_name(key: &SandboxKey) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.to_string().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("exo-{hash:016x}")
}

async fn run_checked(mut process: Command, what: &str) -> Result<String> {
    let output = process
        .output()
        .await
        .with_context(|| format!("spawn {what}"))?;
    if !output.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxLifecycleConfig;

    /// A configured boot binary is used as given. The point is what does *not*
    /// happen: no `PATH` walk, no `stat`, so a path that exists only on the host
    /// this config was written for still round-trips instead of being silently
    /// replaced by whatever resolution finds.
    #[tokio::test]
    async fn a_configured_boot_binary_is_used_verbatim() {
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            mode: SmolvmExecutionMode::OneShot,
            binary: Some(PathBuf::from("/nowhere/smolvm")),
            boot_binary: Some(PathBuf::from("/nowhere/smolvm-bin")),
        });
        assert_eq!(
            backend.boot_binary().await.as_deref(),
            Some(Path::new("/nowhere/smolvm-bin"))
        );
    }

    /// Resolution is memoized, which is what keeps it off the per-sandbox path:
    /// `acquire` asks for this every ephemeral run, and the answer costs a `PATH`
    /// walk plus a `canonicalize`.
    #[tokio::test]
    async fn boot_binary_resolution_is_cached_after_the_first_ask() {
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            mode: SmolvmExecutionMode::OneShot,
            binary: Some(PathBuf::from("/nowhere/smolvm")),
            boot_binary: None,
        });
        assert!(
            !backend.boot_binary.initialized(),
            "construction must not resolve; that is the work being deferred"
        );
        // Not asserted against a fixed value: an inherited `SMOLVM_BOOT_BINARY`
        // legitimately changes the answer, and what is under test is that the
        // answer is computed once, not what it is.
        let first = backend.boot_binary().await.clone();
        assert!(backend.boot_binary.initialized());
        assert_eq!(
            backend.boot_binary().await,
            &first,
            "the second ask must read the cell, not the filesystem"
        );
    }

    #[test]
    fn parses_the_version_line_smolvm_actually_prints() {
        assert_eq!(parse_version("smolvm 1.7.5\n"), Some(Version::new(1, 7, 5)));
        assert_eq!(
            parse_version("smolvm 1.7.5-rc.1"),
            Some(Version::parse("1.7.5-rc.1").unwrap())
        );
        assert_eq!(
            parse_version("smolvm v1.10.0"),
            Some(Version::new(1, 10, 0))
        );
        assert_eq!(parse_version("smolvm 1.7"), Some(Version::new(1, 7, 0)));
        assert_eq!(parse_version("not a version"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn warm_threshold_matches_the_releases_that_carry_the_fix() {
        let supports = |v: &str| parse_version(v).is_some_and(|got| got >= MIN_WARM_VERSION);
        assert!(!supports("smolvm 1.7.0"));
        assert!(!supports("smolvm 1.7.1"));
        assert!(supports("smolvm 1.7.2"));
        assert!(supports("smolvm 1.7.5"));
        assert!(supports("smolvm 2.0.0"));
        assert!(!supports("smolvm 1.6.13"));
        // Semver precedence, which the previous tuple comparison got wrong: a
        // prerelease sorts BELOW its release, so 1.7.2-rc.1 predates the fix the
        // gate requires and must not enable warm mode.
        assert!(!supports("smolvm 1.7.2-rc.1"));
        assert!(supports("smolvm 1.7.3-rc.1"));
    }

    fn test_request(idle_ttl: Option<Duration>) -> SandboxRequest {
        SandboxRequest {
            key: SandboxKey::AgentSandbox {
                agent_id: "a".into(),
                sandbox_id: "s".into(),
            },
            spec: SandboxSpec {
                image: "alpine".into(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                network: SandboxNetworkPolicy::Disabled,
                default_workdir: "/".into(),
            },
            lifecycle: SandboxLifecycleConfig { idle_ttl },
            provider_state: None,
        }
    }

    /// Never downgraded by version probing or by the request's lifecycle.
    #[tokio::test]
    async fn explicit_modes_are_honoured_without_probing() {
        let warm = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::Warm);
        assert_eq!(
            warm.resolve_mode(&test_request(None)).await,
            SmolvmExecutionMode::Warm
        );

        let one_shot = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::OneShot);
        assert_eq!(
            one_shot
                .resolve_mode(&test_request(Some(Duration::from_secs(60))))
                .await,
            SmolvmExecutionMode::OneShot
        );
    }

    #[tokio::test]
    async fn auto_without_idle_ttl_is_one_shot() {
        let backend = SmolvmSandboxBackend::new();
        assert_eq!(
            backend.resolve_mode(&test_request(None)).await,
            SmolvmExecutionMode::OneShot
        );
    }

    /// Must not panic or hang; the first real command reports the failure.
    #[tokio::test]
    async fn auto_falls_back_to_one_shot_when_smolvm_is_absent() {
        // SAFETY: single-threaded test process, set before any probe runs.
        unsafe { std::env::set_var(SMOLVM_BIN_ENV, "/nonexistent/smolvm-does-not-exist") };
        let backend = SmolvmSandboxBackend::new();
        assert!(!backend.warm_supported().await);
        assert_eq!(
            backend
                .resolve_mode(&test_request(Some(Duration::from_secs(60))))
                .await,
            SmolvmExecutionMode::OneShot
        );
        unsafe { std::env::remove_var(SMOLVM_BIN_ENV) };
    }

    #[test]
    fn durable_file_systems_are_rejected_not_ignored() {
        let mut spec = test_request(None).spec;
        spec.durable_file_systems = vec![crate::DurableFileSystem {
            name: "cache".into(),
            mount_path: "/cache".into(),
            mode: crate::FileSystemMountMode::ReadWrite,
        }];
        let err = reject_unsupported_spec(&spec).unwrap_err().to_string();
        assert!(err.contains("cache"), "error should name the fs: {err}");
    }

    /// Must fail at acquire with guidance, not deep inside the CLI.
    #[test]
    fn registry_image_without_network_is_rejected() {
        let mut spec = test_request(None).spec;
        spec.image = "docker.io/library/ubuntu:24.04".into();
        spec.network = SandboxNetworkPolicy::Disabled;
        let err = reject_unsupported_spec(&spec).unwrap_err().to_string();
        assert!(err.contains("network-disabled"), "unexpected error: {err}");

        // Fine once the sandbox is allowed network...
        spec.network = SandboxNetworkPolicy::Enabled;
        assert!(reject_unsupported_spec(&spec).is_ok());

        // ...and a local archive is fine while staying isolated.
        spec.network = SandboxNetworkPolicy::Disabled;
        spec.image = "/tmp/alpine.tar".into();
        assert!(reject_unsupported_spec(&spec).is_ok());
    }

    #[test]
    fn local_image_refs_are_recognised() {
        assert!(is_local_image_ref("/tmp/alpine.tar"));
        assert!(is_local_image_ref("./rootfs"));
        assert!(is_local_image_ref("image.tar.gz"));
        assert!(is_local_image_ref("-"));
        assert!(!is_local_image_ref("alpine"));
        assert!(!is_local_image_ref("docker.io/library/ubuntu:24.04"));
    }

    /// exo's deadline must fire after smolvm's, or the CLI dies before cleanup.
    #[test]
    fn backstop_timeout_is_later_than_the_requested_one() {
        let mut command = SandboxCommand {
            argv: vec!["true".into()],
            env: Default::default(),
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(30)),
        };
        assert_eq!(
            with_backstop_timeout(&command).timeout,
            Some(Duration::from_secs(30) + TIMEOUT_BACKSTOP_GRACE)
        );
        command.timeout = None;
        assert_eq!(with_backstop_timeout(&command).timeout, None);
    }

    #[test]
    fn machine_name_is_stable_and_key_specific() {
        let a = SandboxKey::AgentSandbox {
            agent_id: "agent-1".into(),
            sandbox_id: "sandbox-1".into(),
        };
        let b = SandboxKey::ConversationSandbox {
            conversation_id: "agent-1".into(),
            sandbox_id: "sandbox-1".into(),
        };
        assert_eq!(machine_name(&a), machine_name(&a));
        assert_ne!(machine_name(&a), machine_name(&b));
        assert!(machine_name(&a).starts_with("exo-"));
        // These go on the CLI and into paths: keep them boring.
        assert!(
            machine_name(&a)
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        );
    }

    #[test]
    fn read_only_mounts_get_the_ro_suffix() {
        let spec = SandboxSpec {
            image: "alpine".into(),
            mounts: vec![
                crate::sandbox::SandboxMount {
                    host_path: PathBuf::from("/host/rw"),
                    guest_path: "/guest/rw".into(),
                    access: SandboxMountAccess::ReadWrite,
                    internal: false,
                },
                crate::sandbox::SandboxMount {
                    host_path: PathBuf::from("/host/ro"),
                    guest_path: "/guest/ro".into(),
                    access: SandboxMountAccess::ReadOnly,
                    internal: false,
                },
            ],
            durable_file_systems: Vec::new(),
            network: SandboxNetworkPolicy::Disabled,
            default_workdir: "/work".into(),
        };

        let mut process = Command::new("smolvm");
        configure_spec_args(&mut process, &spec);
        let rendered: Vec<String> = process
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        assert!(rendered.contains(&"/host/rw:/guest/rw".to_string()));
        assert!(rendered.contains(&"/host/ro:/guest/ro:ro".to_string()));
        // Disabled is smolvm's default, so no flag is emitted.
        assert!(!rendered.contains(&"--net".to_string()));
    }
}

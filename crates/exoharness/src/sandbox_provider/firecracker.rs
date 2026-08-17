//! Linux Firecracker sandbox backend.
//!
//! Security-sensitive choices follow Firecracker's upstream production guidance:
//! https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md

use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions, Permissions};
use std::future::Future;
use std::io::{BufRead, BufReader as StdBufReader, Read, Seek, SeekFrom, Write};
use std::net::Ipv4Addr;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use ipnet::Ipv4Net;
#[cfg(target_os = "linux")]
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{FlockOperation, flock};
#[cfg(target_os = "linux")]
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::sandbox::{
    BoxSandboxTcpStream, ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand,
    SandboxCommandOutput, SandboxKey, SandboxNetworkPolicy, SandboxRequest, SandboxSpec,
    SnapshotKind, SnapshotPayload, sandbox_spec_hash,
};
use crate::sandbox_provider::process_bridge;
use crate::{FileSystemMountMode, SandboxAttachment, SandboxProcessParts};

use super::firecracker_image::resolve_image;
#[cfg(test)]
use super::firecracker_image::validate_ext4_image;

const GUEST_READY_TIMEOUT: Duration = Duration::from_secs(30);
const PID_FILE_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const GUEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(40);
const MAX_GUEST_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_GUEST_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_MACHINE_ID: &str = "fc-0000000000000000-00000000";
// sockaddr_un.sun_path is 108 bytes including the trailing NUL on Linux.
// The machine id length is budgeted against it: the jailed API socket lives at
// {state_root}/jailer/{bin}/{machine_id}/root/run/firecracker.socket, and with
// the documented default state root a 16-hex-character id fits with one byte
// to spare while a longer id does not. Growing the id breaks every install
// using the README defaults — see default_state_root_fits_all_jailed_socket_paths.
// https://github.com/torvalds/linux/blob/master/include/uapi/linux/un.h
const UNIX_SOCKET_PATH_CAPACITY: usize = 108;
// The guest agent drops to UID 10001 before binding, so keep its AF_VSOCK port
// above Linux's privileged range. The kernel enforces that in vsock_bind().
// https://github.com/torvalds/linux/blob/master/net/vmw_vsock/af_vsock.c
const GUEST_AGENT_PORT: u32 = 10_052;
// A guest-initiated vsock connection becomes an event-driven ready signal on
// the host, avoiding repeated CONNECT probes that contend with early boot.
// https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md#guest-initiated-connections
const GUEST_READY_HOST_PORT: u32 = 10_053;
const MAX_RESOURCE_SLOTS: u32 = 32_768;
const NETWORK_BASE: Ipv4Addr = Ipv4Addr::new(10, 240, 0, 0);
const EXO_NETWORK_CIDR: &str = "10.240.0.0/14";
pub const DEFAULT_FIRECRACKER_BINARY: &str = "/usr/local/bin/firecracker";
pub const DEFAULT_FIRECRACKER_JAILER: &str = "/usr/local/bin/jailer";
pub const DEFAULT_FIRECRACKER_KERNEL: &str = "/var/lib/exo/firecracker/vmlinux";
pub const DEFAULT_FIRECRACKER_INITRAMFS: &str =
    "/var/lib/exo/firecracker/exo-firecracker-initramfs.cpio";
pub const DEFAULT_FIRECRACKER_STATE_ROOT: &str = "/var/lib/exo/firecracker/state";
pub const DEFAULT_WORKSPACE_SIZE_GIB: u64 = 20;
pub const DEFAULT_IMAGE_SIZE_GIB: u64 = 8;
pub const DEFAULT_NETWORK_BYTES_PER_SECOND: u64 = 100 * 1024 * 1024;
pub const DEFAULT_JAILER_UID_BASE: u32 = 100_000;
pub const DEFAULT_VCPU_COUNT: u8 = 2;
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_MEMORY_MIB: u32 = 4096;
#[cfg(target_os = "macos")]
pub const DEFAULT_MEMORY_MIB: u32 = 1024;
const SNAPSHOT_FORMAT_VERSION: u32 = 1;
// Upstream warns that a compromised guest kernel can reactivate the serial
// device even with 8250.nr_uarts=0, and unbounded console output written to a
// host file is their named disk-fill DoS. VMM output therefore goes to
// /dev/null; importantly, that sink also remains valid after the controller
// process exits so a later controller can adopt the running VM.
// https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md#8250-serial-device
// Every Unix socket the jail binds, in one place. These in-jail absolute
// paths are exactly what Firecracker receives, they resolve to host paths via
// jailed_path_on_host, and validate_jailed_socket_paths budgets each of them
// (including the "_<port>" ready-listener suffix Firecracker appends to the
// vsock path) against sun_path. Machine-id length, state-root length, and
// these names all trade against the same 107 usable bytes.
const JAILED_API_SOCKET: &str = "/run/firecracker.socket";
const JAILED_VSOCK: &str = "/run/exo.vsock";
// The API socket peer is the jailed VMM, which counts as untrusted once a
// guest compromises it. Bounding what root reads back keeps a hostile VMM
// from ballooning this process's memory with endless status lines, headers,
// or an absurd Content-Length.
const FIRECRACKER_API_MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const FIRECRACKER_API_TIMEOUT: Duration = Duration::from_secs(5);
const FIRECRACKER_SNAPSHOT_CREATE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
// Firecracker forwards guest packets without filtering them. Keep special-use,
// link-local, host-private, and cross-VM ranges closed unless explicitly admitted.
// https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md#filtering-guest-egress-network-traffic
const BLOCKED_EGRESS_CIDRS: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "224.0.0.0/4",
    "240.0.0.0/4",
];
static ONE_SHOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
// One counter serves every temporary-file name in the state root: the names
// already differ by purpose and pid, so all the counter adds is uniqueness
// within this process.
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FirecrackerConfig {
    pub firecracker_bin: PathBuf,
    pub jailer_bin: PathBuf,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub state_root: PathBuf,
    pub vcpu_count: u8,
    pub memory_mib: u32,
    pub image_size_gib: u64,
    pub workspace_size_gib: u64,
    pub jailer_uid_base: u32,
    pub dns_server: Ipv4Addr,
    pub allowed_egress_cidrs: Vec<Ipv4Net>,
    pub allowed_local_images: Vec<PathBuf>,
    // Registry entry points the root-run materializer may contact; empty =
    // unrestricted. Permitted registries are trusted for process availability
    // and any cross-host blob redirects they return.
    pub allowed_registries: Vec<String>,
    pub network_bytes_per_second: u64,
    /// Hard ceiling for VMs owned by this backend. Firecracker deliberately
    /// leaves host CPU/memory oversubscription policy to its caller, and each
    /// Firecracker process owns exactly one microVM, so callers should derive
    /// this from scheduler capacity plus reserved fork-source VMs.
    /// https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md#L23-L24
    /// https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md#L71-L72
    pub max_machines: Option<NonZeroUsize>,
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        Self {
            firecracker_bin: PathBuf::from(DEFAULT_FIRECRACKER_BINARY),
            jailer_bin: PathBuf::from(DEFAULT_FIRECRACKER_JAILER),
            kernel: PathBuf::from(DEFAULT_FIRECRACKER_KERNEL),
            initramfs: PathBuf::from(DEFAULT_FIRECRACKER_INITRAMFS),
            state_root: PathBuf::from(DEFAULT_FIRECRACKER_STATE_ROOT),
            vcpu_count: DEFAULT_VCPU_COUNT,
            memory_mib: DEFAULT_MEMORY_MIB,
            image_size_gib: DEFAULT_IMAGE_SIZE_GIB,
            workspace_size_gib: DEFAULT_WORKSPACE_SIZE_GIB,
            jailer_uid_base: DEFAULT_JAILER_UID_BASE,
            dns_server: Ipv4Addr::new(1, 1, 1, 1),
            allowed_egress_cidrs: Vec::new(),
            allowed_local_images: vec![PathBuf::from(super::default_firecracker_image())],
            allowed_registries: Vec::new(),
            network_bytes_per_second: DEFAULT_NETWORK_BYTES_PER_SECOND,
            max_machines: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FirecrackerProviderState {
    machine_id: String,
    spec_hash: String,
    // `prepare_network` installs a host route to this per-VM address. Returning
    // the address lets the host controller reach an explicitly selected guest
    // service without publishing it outside the host or weakening guest-to-host
    // and guest-to-guest firewall rules.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/network-setup.md#host-network-setup
    guest_ip: Option<Ipv4Addr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FirecrackerSnapshotManifest {
    format_version: u32,
    template_key: String,
    spec_hash: String,
    source_network_slot: u32,
}

impl FirecrackerSnapshotManifest {
    fn from_payload(payload: SnapshotPayload) -> Result<Self> {
        if payload.kind != SnapshotKind::FirecrackerSnapshot {
            bail!(
                "Firecracker sandbox backend can only restore a FirecrackerSnapshot payload, got {:?}",
                payload.kind
            );
        }
        let manifest: Self = serde_json::from_slice(&payload.bytes)
            .context("decoding FirecrackerSnapshot manifest")?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn into_payload(self) -> Result<SnapshotPayload> {
        let bytes =
            serde_json::to_vec(&self).context("serializing FirecrackerSnapshot manifest")?;
        Ok(SnapshotPayload {
            kind: SnapshotKind::FirecrackerSnapshot,
            bytes: Bytes::from(bytes),
        })
    }

    fn validate(&self) -> Result<()> {
        if self.format_version != SNAPSHOT_FORMAT_VERSION {
            bail!(
                "unsupported Firecracker snapshot format version {}; expected {}",
                self.format_version,
                SNAPSHOT_FORMAT_VERSION
            );
        }
        validate_snapshot_key(&self.template_key)?;
        if self.source_network_slot >= MAX_RESOURCE_SLOTS {
            bail!(
                "invalid Firecracker snapshot source network slot: {}",
                self.source_network_slot
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotTemplateReference {
    key: String,
    lifecycle: SnapshotTemplateLifecycle,
}

struct SnapshotMachineRecord {
    template: SnapshotTemplateReference,
    source_network_slot: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotTemplateLifecycle {
    /// Point-in-time `fork` snapshots exist for exactly one target machine.
    Machine,
    /// Explicit snapshots outlive every machine restored from them.
    Snapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MachineRecord {
    machine_id: String,
    spec_hash: String,
    resolved_image: String,
    slot: u32,
    network_enabled: bool,
    workspace_id: Option<String>,
    // The lease mtime is refreshed on use; keeping the TTL in the immutable
    // manifest lets a later CLI process reap a VM without process-local state.
    idle_ttl_seconds: Option<u64>,
    snapshot_template: Option<SnapshotTemplateReference>,
    snapshot_network_slot: Option<u32>,
}

enum GuestReadiness {
    Signal(StdUnixListener),
    Probe,
}

#[derive(Serialize)]
struct FirecrackerVmState<'a> {
    state: &'a str,
}

#[derive(Serialize)]
struct FirecrackerSnapshotCreate<'a> {
    snapshot_type: &'a str,
    snapshot_path: &'a str,
    mem_file_path: &'a str,
}

#[derive(Serialize)]
struct FirecrackerSnapshotLoad<'a> {
    snapshot_path: &'a str,
    mem_backend: FirecrackerMemoryBackend<'a>,
    track_dirty_pages: bool,
    resume_vm: bool,
}

#[derive(Serialize)]
struct FirecrackerMemoryBackend<'a> {
    backend_path: &'a str,
    backend_type: &'a str,
}

#[derive(Debug, Clone)]
struct NetworkConfig {
    namespace: String,
    host_veth: String,
    namespace_veth: String,
    nft_table: String,
    transit_host: Ipv4Addr,
    transit_namespace: Ipv4Addr,
    guest_gateway: Ipv4Addr,
    guest_ip: Ipv4Addr,
    guest_cidr: String,
    guest_mac: String,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct FirecrackerVmConfiguration {
    boot_source: FirecrackerBootSource,
    drives: Vec<FirecrackerDrive>,
    machine_config: FirecrackerMachineConfiguration,
    network_interfaces: Vec<FirecrackerNetworkInterface>,
    vsock: FirecrackerVsock,
    entropy: FirecrackerEntropy,
}

#[derive(Serialize)]
struct FirecrackerEntropy {
    rate_limiter: FirecrackerRateLimiter,
}

#[derive(Serialize)]
struct FirecrackerBootSource {
    kernel_image_path: &'static str,
    initrd_path: &'static str,
    boot_args: String,
}

#[derive(Serialize)]
struct FirecrackerDrive {
    drive_id: &'static str,
    path_on_host: &'static str,
    is_root_device: bool,
    is_read_only: bool,
    cache_type: &'static str,
    io_engine: &'static str,
}

#[derive(Serialize)]
struct FirecrackerMachineConfiguration {
    vcpu_count: u8,
    mem_size_mib: u32,
    smt: bool,
    track_dirty_pages: bool,
}

#[derive(Clone, Serialize)]
struct FirecrackerRateLimiter {
    bandwidth: FirecrackerTokenBucket,
}

#[derive(Clone, Serialize)]
struct FirecrackerTokenBucket {
    size: u64,
    refill_time: u64,
}

#[derive(Serialize)]
struct FirecrackerNetworkInterface {
    iface_id: &'static str,
    guest_mac: String,
    host_dev_name: &'static str,
    rx_rate_limiter: FirecrackerRateLimiter,
    tx_rate_limiter: FirecrackerRateLimiter,
}

#[derive(Serialize)]
struct FirecrackerVsock {
    guest_cid: u32,
    uds_path: &'static str,
}

#[derive(Debug, Clone)]
struct Machine {
    record: MachineRecord,
    vsock_path: PathBuf,
}

#[derive(Debug, Clone)]
struct WarmMachineEntry {
    machine_id: String,
    spec_hash: String,
    idle_ttl: Option<Duration>,
    last_used_at: Instant,
}

struct Shared {
    config: FirecrackerConfig,
    // Serialize controllers for a state root. A later controller adopts live,
    // matching machines after the prior controller releases this lock; the
    // lock prevents concurrent controllers from racing that reconciliation.
    _state_lock: File,
    warm_machines: Mutex<HashMap<SandboxKey, WarmMachineEntry>>,
    lifecycle_lock: Mutex<()>,
}

pub struct FirecrackerSandboxBackend {
    shared: Arc<Shared>,
}

impl FirecrackerSandboxBackend {
    pub async fn new(config: FirecrackerConfig) -> Result<Self> {
        tokio::task::spawn_blocking(move || Self::new_blocking(config))
            .await
            .context("joining Firecracker backend construction")?
    }

    fn new_blocking(mut config: FirecrackerConfig) -> Result<Self> {
        validate_host_blocking(&config)?;
        fs::create_dir_all(&config.state_root).with_context(|| {
            format!(
                "creating Firecracker state root {}",
                config.state_root.display()
            )
        })?;
        fs::set_permissions(&config.state_root, Permissions::from_mode(0o700))?;
        for directory in [
            "artifacts",
            "cows",
            "jailer",
            "leases",
            "manifests",
            "slots",
            "snapshots",
            "workspaces",
        ] {
            let path = config.state_root.join(directory);
            fs::create_dir_all(&path)?;
            fs::set_permissions(path, Permissions::from_mode(0o700))?;
        }
        config.firecracker_bin = fs::canonicalize(&config.firecracker_bin)?;
        config.jailer_bin = fs::canonicalize(&config.jailer_bin)?;
        config.kernel = fs::canonicalize(&config.kernel)?;
        config.initramfs = fs::canonicalize(&config.initramfs)?;
        config.state_root = fs::canonicalize(&config.state_root)?;
        validate_private_root(&config.state_root)?;
        let state_lock_path = config.state_root.join("backend.lock");
        let state_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&state_lock_path)
            .with_context(|| {
                format!(
                    "opening Firecracker state-root lock {}",
                    state_lock_path.display()
                )
            })?;
        fs::set_permissions(&state_lock_path, Permissions::from_mode(0o600))?;
        flock(&state_lock, FlockOperation::NonBlockingLockExclusive).with_context(|| {
            format!(
                "locking Firecracker state root {}; another backend may still own it",
                config.state_root.display()
            )
        })?;
        // Best-effort reclamation of temporaries stranded by crashed prior
        // processes. Every publish in the state root stages under a dotted or
        // .tmp name and renames atomically, so anything still carrying a
        // staging name after a day is garbage that would otherwise accumulate
        // on the root-owned volume forever.
        super::firecracker_image::sweep_stale_image_artifacts(&config.state_root);
        for directory in ["artifacts", "snapshots"] {
            super::firecracker_image::sweep_stale_temporaries(
                &config.state_root.join(directory),
                |name| name.starts_with('.'),
            );
        }
        for directory in ["manifests", "leases"] {
            super::firecracker_image::sweep_stale_temporaries(
                &config.state_root.join(directory),
                |name| name.ends_with(".tmp"),
            );
        }
        config.kernel = cache_immutable_artifact(&config.state_root, "kernel", &config.kernel)?;
        config.initramfs =
            cache_immutable_artifact(&config.state_root, "initramfs", &config.initramfs)?;
        validate_jailed_socket_paths(&config)?;

        Ok(Self {
            shared: Arc::new(Shared {
                config,
                _state_lock: state_lock,
                warm_machines: Mutex::new(HashMap::new()),
                lifecycle_lock: Mutex::new(()),
            }),
        })
    }

    // Entry point for the Lima bridge. Persisted leases are authoritative
    // across bridge/controller restarts.
    pub(super) async fn reap_expired(&self) -> Result<()> {
        let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
        self.reap_expired_machines().await
    }

    async fn reap_expired_machines(&self) -> Result<()> {
        let now = Instant::now();
        let mut expired = {
            let mut machines = self.shared.warm_machines.lock().await;
            let keys = machines
                .iter()
                .filter_map(|(key, entry)| {
                    entry
                        .idle_ttl
                        .filter(|ttl| entry.last_used_at + *ttl <= now)
                        .map(|_| key.clone())
                })
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| machines.remove(&key))
                .map(|entry| entry.machine_id)
                .collect::<Vec<_>>()
        };
        let state_root = self.shared.config.state_root.clone();
        let persisted = tokio::task::spawn_blocking(move || {
            expired_machine_ids(&state_root, SystemTime::now())
        })
        .await
        .context("joining Firecracker lease scan")??;
        expired.extend(persisted);

        let expired = expired.into_iter().collect::<HashSet<_>>();
        if !expired.is_empty() {
            self.shared
                .warm_machines
                .lock()
                .await
                .retain(|_, entry| !expired.contains(&entry.machine_id));
        }
        for machine_id in expired {
            self.shared.cleanup_machine(&machine_id, true).await?;
        }
        Ok(())
    }

    async fn resolve_request(&self, request: SandboxRequest) -> Result<SandboxRequest> {
        let mut request = prepare_request(request)?;
        let image = resolve_image(
            &self.shared.config.state_root,
            &request.spec.image,
            self.shared.config.image_size_gib,
            &self.shared.config.allowed_local_images,
            &self.shared.config.allowed_registries,
        )
        .await?;
        request.spec.image = image.to_string_lossy().into_owned();
        Ok(request)
    }

    async fn capture_snapshot(
        shared: &Arc<Shared>,
        request: &SandboxRequest,
        source_machine_id: &str,
        source_spec_hash: &str,
        template_key: String,
    ) -> Result<FirecrackerSnapshotManifest> {
        if !request.spec.durable_file_systems.is_empty() {
            bail!("Firecracker snapshotting does not support durable filesystems")
        }
        validate_snapshot_key(&template_key)?;

        let _lifecycle_guard = shared.lifecycle_lock.lock().await;
        let source = shared
            .load_machine_record(source_machine_id)
            .await?
            .context("Firecracker snapshot source manifest is missing")?;
        if source.spec_hash != source_spec_hash
            || sandbox_spec_hash(&request.spec) != source_spec_hash
        {
            bail!("Firecracker snapshot source specification changed")
        }
        if !process_running(&shared.pid_path(&source.machine_id)) {
            bail!("Firecracker snapshot source is not running")
        }

        GuestClient::new(
            Arc::clone(shared),
            machine_from_record(&shared.config, source.clone()).vsock_path,
        )
        .sync_filesystem(&request.spec.default_workdir)
        .await
        .context("syncing Firecracker snapshot source filesystem")?;

        let config = shared.config.clone();
        let source_for_snapshot = source.clone();
        let key_for_snapshot = template_key.clone();
        tokio::task::spawn_blocking(move || {
            capture_snapshot_template(&config, &source_for_snapshot, &key_for_snapshot)
        })
        .await
        .context("joining Firecracker snapshot creation")??;

        Ok(FirecrackerSnapshotManifest {
            format_version: SNAPSHOT_FORMAT_VERSION,
            template_key,
            spec_hash: source.spec_hash,
            source_network_slot: source.slot,
        })
    }

    async fn restore_snapshot(
        &self,
        request: SandboxRequest,
        manifest: FirecrackerSnapshotManifest,
        lifecycle: SnapshotTemplateLifecycle,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let template_key = manifest.template_key.clone();
        let restore = async {
            manifest.validate()?;
            if request.lifecycle.idle_ttl.is_none() {
                bail!("Firecracker snapshot restore requires a warm sandbox lifecycle")
            }
            if !request.spec.durable_file_systems.is_empty() {
                bail!("Firecracker snapshot restore does not support durable filesystems")
            }
            let spec_hash = sandbox_spec_hash(&request.spec);
            if spec_hash != manifest.spec_hash {
                bail!("Firecracker snapshot specification does not match the requested sandbox")
            }

            let machine_id = machine_id(&request.key, &spec_hash);
            let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
            self.shared.ensure_machine_capacity(&machine_id).await?;
            let config = self.shared.config.clone();
            let key = template_key.clone();
            let template_ready =
                tokio::task::spawn_blocking(move || snapshot_template_ready(&config, &key))
                    .await
                    .context("joining Firecracker snapshot template validation")??;
            if !template_ready {
                bail!("Firecracker snapshot {template_key} is not available on this host")
            }
            self.shared
                .new_machine_record(
                    &request,
                    &machine_id,
                    &spec_hash,
                    Some(SnapshotMachineRecord {
                        template: SnapshotTemplateReference {
                            key: template_key.clone(),
                            lifecycle,
                        },
                        source_network_slot: manifest.source_network_slot,
                    }),
                )
                .await?;
            drop(_lifecycle_guard);

            match self.acquire_resolved(request).await {
                Ok(handle) => Ok(handle),
                Err(error) => {
                    let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
                    if let Err(cleanup_error) = self.shared.cleanup_machine(&machine_id, true).await
                    {
                        tracing::warn!(
                            machine_id,
                            error = format!("{cleanup_error:#}"),
                            "failed cleaning up unsuccessful Firecracker snapshot restore"
                        );
                    }
                    Err(error)
                }
            }
        }
        .await;

        match restore {
            Err(error) if lifecycle == SnapshotTemplateLifecycle::Machine => {
                let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
                if let Err(cleanup_error) =
                    self.shared.remove_snapshot_template(&template_key).await
                {
                    return Err(error).context(format!(
                        "cleaning up failed single-use Firecracker snapshot also failed: {cleanup_error:#}"
                    ));
                }
                Err(error)
            }
            result => result,
        }
    }

    async fn acquire_resolved(
        &self,
        request: SandboxRequest,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
        let spec_hash = sandbox_spec_hash(&request.spec);
        let stable_machine_id = machine_id(&request.key, &spec_hash);
        let machine_key_prefix = format!("fc-{}-", stable_id(&request.key.to_string()));

        if let Some(state) = request
            .provider_state
            .as_ref()
            .map(parse_provider_state)
            .transpose()?
            && (state.machine_id != stable_machine_id || state.spec_hash != spec_hash)
        {
            if !valid_machine_id(&state.machine_id)
                || !state.machine_id.starts_with(&machine_key_prefix)
            {
                bail!("Firecracker provider state does not match the requested sandbox key");
            }
            self.shared.cleanup_machine(&state.machine_id, true).await?;
        }

        let one_shot = request.lifecycle.idle_ttl.is_none();
        let machine_id = if one_shot {
            let sequence = ONE_SHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            one_shot_machine_id(&request.key, &spec_hash, sequence)
        } else {
            stable_machine_id
        };

        if !one_shot {
            let replaced = {
                let mut machines = self.shared.warm_machines.lock().await;
                match machines.get(&request.key) {
                    Some(entry) if entry.spec_hash == spec_hash => None,
                    Some(_) => machines.remove(&request.key),
                    None => None,
                }
            };
            if let Some(entry) = replaced {
                self.shared.cleanup_machine(&entry.machine_id, true).await?;
            }
        }

        let machine = self
            .shared
            .ensure_machine(&request, &machine_id, &spec_hash)
            .await?;
        if !one_shot {
            self.shared.touch_machine_lease(&machine_id).await?;
            self.shared.warm_machines.lock().await.insert(
                request.key.clone(),
                WarmMachineEntry {
                    machine_id: machine_id.clone(),
                    spec_hash: spec_hash.clone(),
                    idle_ttl: request.lifecycle.idle_ttl,
                    last_used_at: Instant::now(),
                },
            );
        }

        let id = if one_shot {
            format!("firecracker-oneshot:{machine_id}")
        } else {
            format!("firecracker:{machine_id}")
        };
        Ok(Arc::new(FirecrackerSandboxHandle {
            id,
            machine,
            request,
            spec_hash,
            shared: Arc::clone(&self.shared),
            one_shot,
        }))
    }
}

#[async_trait]
impl ManagedSandboxBackend for FirecrackerSandboxBackend {
    fn is_local(&self) -> bool {
        true
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        {
            let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
            self.reap_expired_machines().await?;
        }
        let request = self.resolve_request(request).await?;
        self.acquire_resolved(request).await
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Firecracker sandboxes do not support external attachments")
    }

    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
        let persisted_machine_id = request
            .provider_state
            .as_ref()
            .map(parse_provider_state)
            .transpose()?
            .map(|state| state.machine_id);
        if let Some(machine_id) = persisted_machine_id.as_deref() {
            let machine_key_prefix = format!("fc-{}-", stable_id(&request.key.to_string()));
            if !valid_machine_id(machine_id) || !machine_id.starts_with(&machine_key_prefix) {
                bail!("Firecracker provider state does not match the terminated sandbox key");
            }
        }
        let machine_id = self
            .shared
            .warm_machines
            .lock()
            .await
            .remove(&request.key)
            .map(|entry| entry.machine_id)
            .or(persisted_machine_id)
            .unwrap_or_else(|| {
                let spec_hash = sandbox_spec_hash(&request.spec);
                machine_id(&request.key, &spec_hash)
            });
        self.shared.cleanup_machine(&machine_id, true).await
    }

    async fn fork_sandbox(
        &self,
        source: SandboxRequest,
        target: SandboxRequest,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let mut source = prepare_request(source)?;
        let target = self.resolve_request(target).await?;
        let source_record = {
            let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
            let source_entry = self
                .shared
                .warm_machines
                .lock()
                .await
                .get(&source.key)
                .cloned()
                .context("Firecracker fork source is not active")?;
            self.shared
                .load_machine_record(&source_entry.machine_id)
                .await?
                .context("Firecracker fork source manifest is missing")?
        };
        source.spec.image = source_record.resolved_image.clone();
        if sandbox_spec_hash(&source.spec) != source_record.spec_hash {
            bail!("Firecracker fork source specification does not match the running machine")
        }
        if source.spec != target.spec {
            bail!("Firecracker fork source and target specifications must match")
        }

        let target_spec_hash = sandbox_spec_hash(&target.spec);
        let target_machine_id = machine_id(&target.key, &target_spec_hash);
        {
            let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
            self.shared
                .ensure_machine_capacity(&target_machine_id)
                .await?;
        }
        let template_key = fork_snapshot_template_key(&source_record, &target_machine_id);
        let manifest = Self::capture_snapshot(
            &self.shared,
            &source,
            &source_record.machine_id,
            &source_record.spec_hash,
            template_key,
        )
        .await?;
        self.restore_snapshot(target, manifest, SnapshotTemplateLifecycle::Machine)
            .await
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let manifest = FirecrackerSnapshotManifest::from_payload(payload)?;
        let request = self.resolve_request(request).await?;
        self.restore_snapshot(request, manifest, SnapshotTemplateLifecycle::Snapshot)
            .await
    }
}

struct FirecrackerSandboxHandle {
    id: String,
    machine: Machine,
    request: SandboxRequest,
    spec_hash: String,
    shared: Arc<Shared>,
    one_shot: bool,
}

#[async_trait]
impl ManagedSandboxHandle for FirecrackerSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn provider_state(&self) -> Option<Value> {
        (!self.one_shot).then(|| {
            serde_json::to_value(FirecrackerProviderState {
                machine_id: self.machine.record.machine_id.clone(),
                spec_hash: self.spec_hash.clone(),
                guest_ip: self
                    .machine
                    .record
                    .network_enabled
                    .then(|| self.machine.record.network().guest_ip),
            })
            .expect("Firecracker provider state should serialize")
        })
    }

    fn effective_image(&self) -> Option<String> {
        Some(self.request.spec.image.clone())
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let output = GuestClient::new(Arc::clone(&self.shared), self.machine.vsock_path.clone())
            .exec(&self.request.spec, command)
            .await;
        if self.one_shot {
            let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
            let cleanup = self
                .shared
                .cleanup_machine(&self.machine.record.machine_id, true)
                .await;
            return match (output, cleanup) {
                (Ok(output), Ok(())) => Ok(output),
                (Ok(_), Err(error)) | (Err(error), _) => Err(error),
            };
        }
        {
            let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
            touch_machine(
                &self.shared,
                &self.shared.warm_machines,
                &self.request.key,
                &self.machine.record.machine_id,
            )
            .await?;
        }
        output
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        let cleanup_machine_id = self
            .one_shot
            .then(|| self.machine.record.machine_id.clone());
        let process = GuestClient::new(Arc::clone(&self.shared), self.machine.vsock_path.clone())
            .start_process(&self.request.spec, command, cleanup_machine_id)
            .await?;
        if !self.one_shot {
            let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
            touch_machine(
                &self.shared,
                &self.shared.warm_machines,
                &self.request.key,
                &self.machine.record.machine_id,
            )
            .await?;
        }
        Ok(process)
    }

    fn supports_tcp(&self) -> bool {
        true
    }

    async fn connect_tcp(&self, port: u16) -> Result<Option<BoxSandboxTcpStream>> {
        if !self.machine.record.network_enabled {
            bail!("Firecracker sandbox does not have networking enabled");
        }
        let address = (self.machine.record.network().guest_ip, port);
        Ok(Some(Box::pin(TcpStream::connect(address).await?)))
    }

    async fn stop(&self) -> Result<()> {
        let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
        if self.machine.record.workspace_id.is_some()
            && process_running(&self.shared.pid_path(&self.machine.record.machine_id))
        {
            // Firecracker's clean-shutdown API is x86-only. On every architecture,
            // sync the durable filesystem through the guest before terminating the
            // VMM so completed writes are not stranded in the guest page cache.
            // https://github.com/firecracker-microvm/firecracker/blob/main/docs/api_requests/actions.md#intel-and-amd-only-sendctrlaltdel
            GuestClient::new(Arc::clone(&self.shared), self.machine.vsock_path.clone())
                .sync_filesystem(&self.request.spec.default_workdir)
                .await
                .context("syncing Firecracker durable filesystem before stop")?;
        }
        self.shared
            .warm_machines
            .lock()
            .await
            .retain(|_, entry| entry.machine_id != self.machine.record.machine_id);
        self.shared
            .cleanup_machine(&self.machine.record.machine_id, true)
            .await
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("Firecracker sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        let manifest = FirecrackerSandboxBackend::capture_snapshot(
            &self.shared,
            &self.request,
            &self.machine.record.machine_id,
            &self.spec_hash,
            explicit_snapshot_template_key(&self.machine.record, Uuid::new_v4()),
        )
        .await?;

        // The generic Exo payload stays small: copying multi-gigabyte RAM and
        // disk images through conversation storage would defeat local snapshot
        // restores. The reference is intentionally usable only by a backend
        // sharing this private Firecracker state root.
        manifest.into_payload()
    }
}

impl Shared {
    async fn ensure_machine_capacity(&self, machine_id: &str) -> Result<()> {
        let Some(max_machines) = self.config.max_machines else {
            return Ok(());
        };
        let state_root = self.config.state_root.clone();
        let machine_id = machine_id.to_string();
        let active_machine_ids =
            tokio::task::spawn_blocking(move || manifest_machine_ids(&state_root))
                .await
                .context("joining Firecracker capacity scan")??;
        ensure_machine_capacity(
            max_machines,
            active_machine_ids.len(),
            active_machine_ids.iter().any(|id| id == &machine_id),
        )
    }

    async fn ensure_machine(
        self: &Arc<Self>,
        request: &SandboxRequest,
        machine_id: &str,
        spec_hash: &str,
    ) -> Result<Machine> {
        let existing = self.load_machine_record(machine_id).await?;
        if let Some(record) = existing.as_ref() {
            if record.spec_hash != spec_hash {
                self.cleanup_machine(machine_id, true).await?;
            } else {
                let machine = machine_from_record(&self.config, record.clone());
                if process_running(&self.pid_path(machine_id))
                    && GuestClient::new(Arc::clone(self), machine.vsock_path.clone())
                        .ping()
                        .await
                        .is_ok()
                {
                    return Ok(machine);
                }
                self.cleanup_machine(machine_id, false).await?;
            }
        }

        let record = match existing {
            Some(record) if record.spec_hash == spec_hash => record,
            _ => {
                self.ensure_machine_capacity(machine_id).await?;
                self.new_machine_record(request, machine_id, spec_hash, None)
                    .await?
            }
        };
        let readiness = match self.prepare_and_launch(request, &record).await {
            Ok(readiness) => readiness,
            Err(error) => {
                if let Err(cleanup_error) = self.cleanup_machine(machine_id, true).await {
                    tracing::warn!(%cleanup_error, machine_id, "failed cleaning up unsuccessful Firecracker launch");
                }
                return Err(error);
            }
        };
        let machine = machine_from_record(&self.config, record);
        let ready = match readiness {
            GuestReadiness::Signal(listener) => wait_for_guest(self, machine_id, listener).await,
            GuestReadiness::Probe => wait_for_restored_guest(self, &machine).await,
        };
        if let Err(error) = ready {
            if let Err(cleanup_error) = self.cleanup_machine(machine_id, true).await {
                tracing::warn!(%cleanup_error, machine_id, "failed cleaning up unsuccessful Firecracker guest boot");
            }
            return Err(error);
        }
        if machine.record.network_enabled
            && machine
                .record
                .snapshot_network_slot
                .is_some_and(|slot| slot != machine.record.slot)
        {
            // A clone that keeps the source's in-memory IP would be unreachable
            // at its own address and could impersonate the source's inside its
            // own namespace. Tear the machine down on failure like every other
            // launch step rather than leaving a half-configured VM running.
            if let Err(error) = self.reconfigure_restored_network(&machine).await {
                if let Err(cleanup_error) = self.cleanup_machine(machine_id, true).await {
                    tracing::warn!(%cleanup_error, machine_id, "failed cleaning up Firecracker clone after network reconfiguration failure");
                }
                return Err(error);
            }
        }
        Ok(machine)
    }

    async fn reconfigure_restored_network(self: &Arc<Self>, machine: &Machine) -> Result<()> {
        // Snapshot memory contains the source guest's configured IP, while each
        // clone gets a distinct host TAP/resource slot. Reset the address before
        // exposing the clone's TCP endpoint.
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/network-setup.md
        let network = machine.record.network();
        GuestClient::new(Arc::clone(self), machine.vsock_path.clone())
            .configure_network(network.guest_ip, network.guest_gateway, 30)
            .await
    }

    async fn load_machine_record(&self, machine_id: &str) -> Result<Option<MachineRecord>> {
        let path = self.manifest_path(machine_id);
        tokio::task::spawn_blocking(move || {
            if !path.try_exists()? {
                return Ok(None);
            }
            let bytes = fs::read(&path)
                .with_context(|| format!("reading Firecracker manifest {}", path.display()))?;
            let record = serde_json::from_slice::<MachineRecord>(&bytes)
                .with_context(|| format!("decoding Firecracker manifest {}", path.display()))?;
            Ok(Some(record))
        })
        .await
        .context("joining Firecracker manifest read")?
    }

    async fn touch_machine_lease(&self, machine_id: &str) -> Result<()> {
        let state_root = self.config.state_root.clone();
        let machine_id = machine_id.to_string();
        tokio::task::spawn_blocking(move || touch_machine_lease(&state_root, &machine_id))
            .await
            .context("joining Firecracker lease update")?
    }

    async fn new_machine_record(
        &self,
        request: &SandboxRequest,
        machine_id: &str,
        spec_hash: &str,
        snapshot: Option<SnapshotMachineRecord>,
    ) -> Result<MachineRecord> {
        let state_root = self.config.state_root.clone();
        let machine_id = machine_id.to_string();
        let spec_hash = spec_hash.to_string();
        let resolved_image = request.spec.image.clone();
        let network_enabled = request.spec.network == SandboxNetworkPolicy::Enabled;
        let workspace_id = if snapshot.is_none() {
            request
                .spec
                .durable_file_systems
                .first()
                .map(|file_system| stable_id(&format!("{}\n{}", request.key, file_system.name)))
        } else {
            None
        };
        let idle_ttl_seconds = request.lifecycle.idle_ttl.map(|ttl| ttl.as_secs());
        tokio::task::spawn_blocking(move || {
            let (slot, snapshot_template, snapshot_network_slot) = match snapshot {
                Some(snapshot) => {
                    validate_snapshot_key(&snapshot.template.key)?;
                    (
                        allocate_resource_slot_from(&state_root, &machine_id, 0)?,
                        Some(snapshot.template),
                        Some(snapshot.source_network_slot),
                    )
                }
                None => (
                    allocate_resource_slot(&state_root, &machine_id)?,
                    None,
                    None,
                ),
            };
            let record = MachineRecord {
                machine_id,
                spec_hash,
                resolved_image,
                slot,
                network_enabled,
                workspace_id,
                idle_ttl_seconds,
                snapshot_template,
                snapshot_network_slot,
            };
            if let Err(error) = write_manifest(&state_root, &record) {
                if let Err(release_error) =
                    release_resource_slot(&state_root, record.slot, &record.machine_id)
                {
                    return Err(error.context(format!(
                        "also failed to release resource slot: {release_error:#}"
                    )));
                }
                return Err(error);
            }
            Ok(record)
        })
        .await
        .context("joining Firecracker machine allocation")?
    }

    async fn prepare_and_launch(
        &self,
        request: &SandboxRequest,
        record: &MachineRecord,
    ) -> Result<GuestReadiness> {
        let config = self.config.clone();
        let request = request.clone();
        let record = record.clone();
        tokio::task::spawn_blocking(move || {
            let network = record.network();
            let result = (|| {
                if record.network_enabled {
                    prepare_network(&config, &network, jailer_uid(&config, &record)?)?;
                }
                prepare_and_launch_blocking(&config, &request, &record)
            })();
            if result.is_err() && record.network_enabled {
                cleanup_network_blocking(&network);
            }
            result
        })
        .await
        .context("joining Firecracker launch task")?
    }

    async fn stop_machine_process(&self, machine_id: &str) -> Result<()> {
        let pid_path = self.pid_path(machine_id);
        let machine_id = machine_id.to_string();
        tokio::task::spawn_blocking(move || stop_machine_process_blocking(&machine_id, &pid_path))
            .await
            .context("joining Firecracker stop task")?
    }

    async fn cleanup_network(&self, network: &NetworkConfig) {
        let network = network.clone();
        if let Err(error) =
            tokio::task::spawn_blocking(move || cleanup_network_blocking(&network)).await
        {
            tracing::warn!(%error, "failed to join Firecracker network cleanup task");
        }
    }

    async fn cleanup_machine(&self, machine_id: &str, delete_rootfs: bool) -> Result<()> {
        if !valid_machine_id(machine_id) {
            bail!("invalid Firecracker machine id: {machine_id}");
        }
        let record = self.load_machine_record(machine_id).await?;
        self.stop_machine_process(machine_id).await?;
        if delete_rootfs && let Some(record) = record.as_ref() {
            let cow = snapshot_cow_path(&self.config, &record.machine_id);
            tokio::task::spawn_blocking(move || {
                if cow.try_exists()? {
                    fs::remove_file(&cow).with_context(|| {
                        format!("removing Firecracker snapshot overlay {}", cow.display())
                    })?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await
            .context("joining Firecracker snapshot overlay cleanup")??;
        }
        if let Some(record) = record.as_ref().filter(|record| record.network_enabled) {
            self.cleanup_network(&record.network()).await;
        }
        let cgroup_dir = firecracker_cgroup_dir(&self.config, machine_id);
        tokio::task::spawn_blocking(move || remove_firecracker_cgroup(&cgroup_dir))
            .await
            .context("joining Firecracker cgroup cleanup")??;
        if !delete_rootfs
            && record
                .as_ref()
                .is_some_and(|record| record.snapshot_template.is_some())
        {
            let jail_dir = self.jail_dir(machine_id);
            tokio::task::spawn_blocking(move || {
                if jail_dir.try_exists()? {
                    fs::remove_dir_all(&jail_dir).with_context(|| {
                        format!("removing Firecracker jail {}", jail_dir.display())
                    })?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await
            .context("joining Firecracker snapshot jail cleanup")??;
        }
        if delete_rootfs
            && let Some(template) = record.as_ref().and_then(|record| {
                record
                    .snapshot_template
                    .as_ref()
                    .filter(|template| template.lifecycle == SnapshotTemplateLifecycle::Machine)
            })
        {
            self.remove_snapshot_template(&template.key).await?;
        }
        if delete_rootfs {
            let jail_dir = self.jail_dir(machine_id);
            let manifest = self.manifest_path(machine_id);
            let lease = lease_path(&self.config.state_root, machine_id);
            let slot_claim = record
                .as_ref()
                .map(|record| (record.slot, record.machine_id.clone()));
            let state_root = self.config.state_root.clone();
            tokio::task::spawn_blocking(move || {
                if jail_dir.try_exists()? {
                    fs::remove_dir_all(&jail_dir).with_context(|| {
                        format!("removing Firecracker jail {}", jail_dir.display())
                    })?;
                }
                if manifest.try_exists()? {
                    fs::remove_file(&manifest).with_context(|| {
                        format!("removing Firecracker manifest {}", manifest.display())
                    })?;
                }
                if lease.try_exists()? {
                    fs::remove_file(&lease).with_context(|| {
                        format!("removing Firecracker lease {}", lease.display())
                    })?;
                }
                if let Some((slot, machine_id)) = slot_claim {
                    release_resource_slot(&state_root, slot, &machine_id)?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await
            .context("joining Firecracker file cleanup")??;
        }
        Ok(())
    }

    async fn remove_snapshot_template(&self, key: &str) -> Result<()> {
        let snapshot = snapshot_template_dir(&self.config, key)?;
        tokio::task::spawn_blocking(move || remove_directory_if_present(&snapshot))
            .await
            .context("joining Firecracker snapshot cleanup")?
    }

    fn manifest_path(&self, machine_id: &str) -> PathBuf {
        manifest_path(&self.config.state_root, machine_id)
    }

    fn jail_dir(&self, machine_id: &str) -> PathBuf {
        jail_dir(&self.config, machine_id)
    }

    fn pid_path(&self, machine_id: &str) -> PathBuf {
        jail_root(&self.config, machine_id).join("firecracker.pid")
    }
}

fn prepare_request(mut request: SandboxRequest) -> Result<SandboxRequest> {
    if request.spec.image.trim().is_empty() {
        request.spec.image = super::default_firecracker_image();
    }
    if !request.spec.mounts.is_empty() {
        bail!(
            "Firecracker does not support host bind mounts; use a durable block device or another provider"
        );
    }
    match request.spec.durable_file_systems.as_slice() {
        [] => {}
        [file_system] => {
            if file_system.mode == FileSystemMountMode::ReadOnly {
                bail!("Firecracker durable workspace must be read-write");
            }
            if file_system.mount_path != request.spec.default_workdir {
                bail!(
                    "Firecracker durable workspace path {:?} must match the default workdir {:?}",
                    file_system.mount_path,
                    request.spec.default_workdir
                );
            }
        }
        _ => bail!("Firecracker supports at most one durable file system"),
    }
    if request.spec.default_workdir.split_whitespace().count() != 1
        || !request.spec.default_workdir.starts_with('/')
    {
        bail!("Firecracker workdir must be an absolute path without whitespace");
    }
    Ok(request)
}

fn parse_provider_state(value: &Value) -> Result<FirecrackerProviderState> {
    serde_json::from_value(value.clone()).context("invalid Firecracker provider state")
}

fn validate_host_blocking(config: &FirecrackerConfig) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("Firecracker sandbox execution is only supported on Linux");
    }
    if fs::metadata("/proc/self")?.uid() != 0 {
        bail!("Firecracker sandbox execution must run as root so jailer can set up isolation");
    }
    validate_trusted_file("Firecracker binary", &config.firecracker_bin)?;
    validate_trusted_file("Firecracker jailer", &config.jailer_bin)?;
    validate_trusted_file("Firecracker guest kernel", &config.kernel)?;
    validate_trusted_file("Firecracker guest initramfs", &config.initramfs)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .context("Firecracker requires read/write access to /dev/kvm")?;
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
        bail!("Firecracker sandbox execution requires cgroup v2");
    }
    let controllers = fs::read_to_string("/sys/fs/cgroup/cgroup.controllers")?;
    for required in ["cpu", "memory"] {
        if !controllers
            .split_whitespace()
            .any(|value| value == required)
        {
            bail!("Firecracker sandbox execution requires the cgroup v2 {required} controller");
        }
    }
    for program in ["cp", "ip", "iptables", "mkfs.ext4", "nft", "sysctl"] {
        trusted_host_command(program)
            .with_context(|| format!("required trusted host command {program}"))?;
    }
    if config.vcpu_count == 0 || config.vcpu_count > 32 {
        bail!("Firecracker vCPU count must be between 1 and 32");
    }
    if config.memory_mib < 128 {
        bail!("Firecracker memory must be at least 128 MiB");
    }
    if config.image_size_gib == 0 {
        bail!("Firecracker OCI image size must be positive");
    }
    gib_bytes(config.image_size_gib, "image")?;
    if config.workspace_size_gib == 0 {
        bail!("Firecracker workspace size must be positive");
    }
    gib_bytes(config.workspace_size_gib, "workspace")?;
    if config.network_bytes_per_second == 0 {
        bail!("Firecracker network rate limit must be positive");
    }
    if config.jailer_uid_base < 65_536 {
        bail!("Firecracker jailer UID base must be at least 65536");
    }
    config
        .jailer_uid_base
        .checked_add(MAX_RESOURCE_SLOTS)
        .context("Firecracker jailer UID range overflows u32")?;
    let firecracker_version = binary_version(&config.firecracker_bin)?;
    let jailer_version = binary_version(&config.jailer_bin)?;
    if firecracker_version != jailer_version {
        bail!(
            "Firecracker and jailer versions must match: {firecracker_version} != {jailer_version}"
        );
    }
    Ok(())
}

fn validate_file(label: &str, path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!(
            "{label} does not exist or is not a file: {}",
            path.display()
        );
    }
    Ok(())
}

pub(super) fn validate_trusted_file(label: &str, path: &Path) -> Result<()> {
    validate_file(label, path)?;
    let path = fs::canonicalize(path)?;
    // Jailer treats its executable and path arguments as trusted input. A writable
    // parent would let a local unprivileged user replace what root executes.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md#observations
    for component in path.ancestors() {
        let metadata = fs::metadata(component)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!(
                "{label} and every parent must be root-owned and not group/world-writable: {}",
                component.display()
            );
        }
    }
    Ok(())
}

fn cache_immutable_artifact(state_root: &Path, label: &str, source: &Path) -> Result<PathBuf> {
    let digest = super::firecracker_image::sha256_hex_of_file(source)?;
    let artifacts = state_root.join("artifacts");
    let cached = artifacts.join(format!("{label}-{digest}"));
    if !cached.try_exists()? {
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = artifacts.join(format!(".{label}.{}.{}", std::process::id(), sequence));
        fs::copy(source, &temporary).with_context(|| {
            format!("staging immutable Firecracker {label} {}", source.display())
        })?;
        fs::set_permissions(&temporary, Permissions::from_mode(0o444))?;
        File::open(&temporary)?.sync_all()?;
        match fs::hard_link(&temporary, &cached) {
            Ok(()) => {}
            Err(error) if cached.try_exists()? => {
                tracing::debug!(%error, path = %cached.display(), "another process cached the Firecracker artifact first");
            }
            Err(error) => {
                fs::remove_file(&temporary)?;
                return Err(error).with_context(|| {
                    format!(
                        "publishing immutable Firecracker {label} {}",
                        cached.display()
                    )
                });
            }
        }
        fs::remove_file(&temporary)?;
    }
    let metadata = fs::metadata(&cached)?;
    if metadata.uid() != 0 || metadata.mode() & 0o222 != 0 || metadata.mode() & 0o004 == 0 {
        bail!(
            "cached Firecracker {label} must be root-owned, immutable, and readable in its jail: {}",
            cached.display()
        );
    }
    Ok(cached)
}

fn validate_private_root(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)?;
    if metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        bail!(
            "Firecracker state root must be root-owned with mode 0700: {}",
            path.display()
        );
    }
    for parent in path.ancestors().skip(1) {
        let metadata = fs::metadata(parent)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!(
                "Firecracker state root parents must be root-owned and not group/world-writable: {}",
                parent.display()
            );
        }
    }
    Ok(())
}

fn validate_jailed_socket_paths(config: &FirecrackerConfig) -> Result<()> {
    let root = jail_root(config, MAX_MACHINE_ID);
    for jailed in [
        JAILED_API_SOCKET.to_string(),
        // Firecracker binds the vsock UDS itself and the host binds the
        // guest-ready listener at the port-suffixed variant; the suffixed form
        // is the longer of the two.
        format!("{JAILED_VSOCK}_{GUEST_READY_HOST_PORT}"),
    ] {
        let path = jailed_path_on_host(&root, &jailed);
        if path.as_os_str().as_bytes().len() >= UNIX_SOCKET_PATH_CAPACITY {
            bail!(
                "Firecracker state root is too long for the jailed Unix socket {jailed}: {}",
                config.state_root.display()
            );
        }
    }
    Ok(())
}

fn jailed_path_on_host(jail_root: &Path, jailed: &str) -> PathBuf {
    jail_root.join(
        jailed
            .strip_prefix('/')
            .expect("jailed socket paths are absolute"),
    )
}

fn find_executable(program: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        })
        .ok_or_else(|| anyhow!("{program} is not executable on PATH"))
}

pub(super) fn trusted_host_command(program: &str) -> Result<PathBuf> {
    let executable = find_executable(program)?;
    let file_name = executable
        .file_name()
        .context("trusted host command has no file name")?;
    let parent = fs::canonicalize(
        executable
            .parent()
            .context("trusted host command has no parent")?,
    )?;
    let executable = parent.join(file_name);
    validate_trusted_file(&format!("host command {program}"), &executable)?;
    // Not redundant with validate_trusted_file: that canonicalizes, so for a
    // symlinked command (eg. iptables -> xtables-nft-multi) it walks the
    // TARGET's parents. This walk covers the INVOCATION path's parents — the
    // directory holding the symlink must be equally untamperable, or a local
    // user could repoint the name root executes.
    for component in executable.ancestors().skip(1) {
        let metadata = fs::metadata(component)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!(
                "host command {program} parent must be root-owned and not group/world-writable: {}",
                component.display()
            );
        }
    }
    // Preserve the invoked name for trusted multicall binaries such as
    // iptables -> xtables-nft-multi. Canonicalizing the final symlink changes
    // argv[0], so xtables cannot select its iptables frontend.
    Ok(executable)
}

pub(super) fn copy_sparse_reflink(source: &Path, destination: &Path) -> Result<()> {
    let executable = trusted_host_command("cp")?;
    let output = Command::new(executable)
        .args(["--sparse=always", "--reflink=auto", "--"])
        .arg(source)
        .arg(destination)
        .output()
        .with_context(|| format!("copying {} to {}", source.display(), destination.display()))?;
    if !output.status.success() {
        bail!(
            "copying {} to {} failed with {}: {}",
            source.display(),
            destination.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn binary_version(path: &Path) -> Result<String> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", path.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let version_output = String::from_utf8_lossy(&output.stdout);
    version_output
        .split_whitespace()
        .rev()
        .find(|part| part.starts_with('v') && part[1..].contains('.'))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("could not parse version from {}", path.display()))
}

fn machine_id(key: &SandboxKey, spec_hash: &str) -> String {
    format!("fc-{}-{}", stable_id(&key.to_string()), &spec_hash[..8])
}

fn one_shot_machine_id(key: &SandboxKey, spec_hash: &str, sequence: u64) -> String {
    format!(
        "fc-{}-{}",
        stable_id(&format!("{key}\n{}\n{sequence}", std::process::id())),
        &spec_hash[..8]
    )
}

fn valid_machine_id(machine_id: &str) -> bool {
    machine_id.starts_with("fc-")
        && machine_id.len() <= 64
        && machine_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

// 16 hex characters (64 bits) of SHA-256. Sufficient because the backend caps
// concurrent machines at MAX_RESOURCE_SLOTS (32,768) and sandbox ids are
// generated UUIDv7s, so the random collision probability among live ids is
// about n²/2⁶⁵ ≈ 3×10⁻¹¹ — and short enough that every jailed socket path
// stays inside sun_path with the default state root. See the MAX_MACHINE_ID
// comment before growing this.
fn stable_id(input: &str) -> String {
    format!("{:x}", Sha256::digest(input.as_bytes()))[..16].to_string()
}

fn hash_snapshot_string(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_le_bytes());
    hasher.update(value.as_bytes());
}

fn machine_from_record(config: &FirecrackerConfig, record: MachineRecord) -> Machine {
    let vsock_path = jailed_path_on_host(&jail_root(config, &record.machine_id), JAILED_VSOCK);
    Machine { record, vsock_path }
}

impl MachineRecord {
    fn network(&self) -> NetworkConfig {
        network_config(self.slot)
    }
}

fn manifest_path(state_root: &Path, machine_id: &str) -> PathBuf {
    state_root
        .join("manifests")
        .join(format!("{machine_id}.json"))
}

fn lease_path(state_root: &Path, machine_id: &str) -> PathBuf {
    state_root.join("leases").join(machine_id)
}

fn touch_machine_lease(state_root: &Path, machine_id: &str) -> Result<()> {
    if !valid_machine_id(machine_id) {
        bail!("invalid Firecracker machine id: {machine_id}");
    }
    let path = lease_path(state_root, machine_id);
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("{}.{sequence}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("creating Firecracker lease {}", temporary.display()))?;
    fs::set_permissions(&temporary, Permissions::from_mode(0o600))?;
    file.write_all(machine_id.as_bytes())
        .with_context(|| format!("writing Firecracker lease {}", temporary.display()))?;
    file.sync_all()?;
    // A rename gives readers either the old or new mtime and also replaces a
    // stale lease without opening a path supplied outside the private state root.
    if let Err(error) = fs::rename(&temporary, &path) {
        if let Err(cleanup_error) = fs::remove_file(&temporary) {
            return Err(error).with_context(|| {
                format!(
                    "publishing Firecracker lease {}; also failed to remove temporary lease: {cleanup_error}",
                    path.display()
                )
            });
        }
        return Err(error)
            .with_context(|| format!("publishing Firecracker lease {}", path.display()));
    }
    Ok(())
}

fn expired_machine_ids(state_root: &Path, now: SystemTime) -> Result<Vec<String>> {
    let mut expired = Vec::new();
    for entry in fs::read_dir(state_root.join("manifests"))? {
        let entry = entry?;
        if !entry.file_type()?.is_file()
            || entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("json")
        {
            continue;
        }
        let path = entry.path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed reading Firecracker manifest during lease scan");
                continue;
            }
        };
        let record = match serde_json::from_slice::<MachineRecord>(&bytes) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed decoding Firecracker manifest during lease scan");
                continue;
            }
        };
        if !valid_machine_id(&record.machine_id)
            || manifest_path(state_root, &record.machine_id) != path
        {
            tracing::warn!(path = %path.display(), "ignored mismatched Firecracker manifest during lease scan");
            continue;
        }
        let Some(idle_ttl_seconds) = record.idle_ttl_seconds else {
            continue;
        };
        let last_used = match fs::metadata(lease_path(state_root, &record.machine_id)) {
            Ok(metadata) => metadata.modified()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                entry.metadata()?.modified()?
            }
            Err(error) => return Err(error.into()),
        };
        if last_used
            .checked_add(Duration::from_secs(idle_ttl_seconds))
            .is_some_and(|deadline| deadline <= now)
        {
            expired.push(record.machine_id);
        }
    }
    Ok(expired)
}

fn manifest_machine_ids(state_root: &Path) -> Result<Vec<String>> {
    let mut machine_ids = Vec::new();
    for entry in fs::read_dir(state_root.join("manifests"))? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("json")
        {
            continue;
        }
        let record = fs::read(&path)
            .with_context(|| format!("reading Firecracker manifest {}", path.display()))
            .and_then(|bytes| {
                serde_json::from_slice::<MachineRecord>(&bytes)
                    .with_context(|| format!("decoding Firecracker manifest {}", path.display()))
            })?;
        if !valid_machine_id(&record.machine_id)
            || manifest_path(state_root, &record.machine_id) != path
        {
            bail!("mismatched Firecracker manifest {}", path.display());
        }
        machine_ids.push(record.machine_id);
    }
    Ok(machine_ids)
}

fn ensure_machine_capacity(
    max_machines: NonZeroUsize,
    active_machines: usize,
    target_already_exists: bool,
) -> Result<()> {
    if target_already_exists || active_machines < max_machines.get() {
        return Ok(());
    }
    bail!(
        "Firecracker host VM capacity exhausted: {active_machines} active, limit {}; reduce the caller's claimed-conversation capacity or terminate an idle sandbox",
        max_machines.get()
    )
}

fn write_manifest(state_root: &Path, record: &MachineRecord) -> Result<()> {
    let path = manifest_path(state_root, &record.machine_id);
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("{}.{sequence}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("creating Firecracker manifest {}", temporary.display()))?;
    fs::set_permissions(&temporary, Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec(record)?)
        .with_context(|| format!("writing Firecracker manifest {}", temporary.display()))?;
    file.sync_all()?;
    if let Err(error) = fs::hard_link(&temporary, &path) {
        if let Err(cleanup_error) = fs::remove_file(&temporary) {
            return Err(error).with_context(|| {
                format!(
                    "publishing Firecracker manifest {}; also failed to remove temporary manifest: {cleanup_error}",
                    path.display()
                )
            });
        }
        return Err(error)
            .with_context(|| format!("publishing Firecracker manifest {}", path.display()));
    }
    if let Err(error) = fs::remove_file(&temporary) {
        tracing::warn!(%error, path = %temporary.display(), "failed to remove temporary Firecracker manifest");
    }
    Ok(())
}

fn allocate_resource_slot(state_root: &Path, machine_id: &str) -> Result<u32> {
    let digest = Sha256::digest(machine_id.as_bytes());
    let first =
        u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]) % MAX_RESOURCE_SLOTS;
    allocate_resource_slot_from(state_root, machine_id, first)
}

fn allocate_resource_slot_from(state_root: &Path, machine_id: &str, first: u32) -> Result<u32> {
    for offset in 0..MAX_RESOURCE_SLOTS {
        let slot = (first + offset) % MAX_RESOURCE_SLOTS;
        let path = resource_slot_path(state_root, slot);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut claim) => {
                fs::set_permissions(&path, Permissions::from_mode(0o600))?;
                writeln!(claim, "{machine_id}")?;
                claim.sync_all()?;
                return Ok(slot);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("claiming Firecracker resource slot {slot}"));
            }
        }
    }
    bail!("no Firecracker resource slots are available")
}

fn resource_slot_path(state_root: &Path, slot: u32) -> PathBuf {
    state_root.join("slots").join(format!("{slot:08x}"))
}

fn release_resource_slot(state_root: &Path, slot: u32, machine_id: &str) -> Result<()> {
    let path = resource_slot_path(state_root, slot);
    if !path.try_exists()? {
        return Ok(());
    }
    let owner = fs::read_to_string(&path)?;
    if owner.trim() != machine_id {
        bail!(
            "refusing to release Firecracker resource slot {slot}: it belongs to {}",
            owner.trim()
        );
    }
    fs::remove_file(&path).with_context(|| format!("releasing Firecracker resource slot {slot}"))
}

fn network_config(slot: u32) -> NetworkConfig {
    let transit_block = slot * 2;
    let guest_block = transit_block + 1;
    let transit_base = ipv4_add(NETWORK_BASE, transit_block * 4);
    let guest_base = ipv4_add(NETWORK_BASE, guest_block * 4);
    let short = format!("{slot:08x}");
    let mac_high = ((slot >> 8) & 0xff) as u8;
    let mac_low = (slot & 0xff) as u8;
    NetworkConfig {
        namespace: format!("fc-{short}"),
        host_veth: format!("fch{short}"),
        namespace_veth: format!("fcn{short}"),
        nft_table: format!("exo_fc_{short}"),
        transit_host: ipv4_add(transit_base, 1),
        transit_namespace: ipv4_add(transit_base, 2),
        guest_gateway: ipv4_add(guest_base, 1),
        guest_ip: ipv4_add(guest_base, 2),
        guest_cidr: format!("{guest_base}/30"),
        guest_mac: format!("06:00:fc:00:{mac_high:02x}:{mac_low:02x}"),
    }
}

fn ipv4_add(address: Ipv4Addr, offset: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(address) + offset)
}

fn prepare_network(
    config: &FirecrackerConfig,
    network: &NetworkConfig,
    jailer_uid: u32,
) -> Result<()> {
    // Firecracker intentionally delegates TAP routing and firewalling to the host.
    // nftables is the upstream-recommended production firewall, and a namespace per
    // VM keeps identical guest interface names and routes from colliding.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/network-setup.md
    if fs::read_to_string("/proc/sys/net/ipv4/ip_forward")?.trim() != "1" {
        bail!("Firecracker networking requires net.ipv4.ip_forward=1 on the host");
    }
    let sysctl = trusted_host_command("sysctl")?;
    let sysctl = sysctl
        .to_str()
        .context("trusted sysctl path is not valid UTF-8")?;
    cleanup_network_blocking(network);
    run_checked("ip", &["netns", "add", &network.namespace])?;
    run_checked(
        "ip",
        &[
            "link",
            "add",
            &network.host_veth,
            "type",
            "veth",
            "peer",
            "name",
            &network.namespace_veth,
        ],
    )?;
    run_checked(
        "ip",
        &[
            "link",
            "set",
            &network.namespace_veth,
            "netns",
            &network.namespace,
        ],
    )?;
    run_checked(
        "ip",
        &[
            "addr",
            "add",
            &format!("{}/30", network.transit_host),
            "dev",
            &network.host_veth,
        ],
    )?;
    run_checked("ip", &["link", "set", &network.host_veth, "up"])?;
    run_checked(
        "ip",
        &[
            "-n",
            &network.namespace,
            "addr",
            "add",
            &format!("{}/30", network.transit_namespace),
            "dev",
            &network.namespace_veth,
        ],
    )?;
    run_checked(
        "ip",
        &[
            "-n",
            &network.namespace,
            "link",
            "set",
            &network.namespace_veth,
            "up",
        ],
    )?;
    run_checked(
        "ip",
        &[
            "-n",
            &network.namespace,
            "tuntap",
            "add",
            "dev",
            "tap0",
            "mode",
            "tap",
            "user",
            &jailer_uid.to_string(),
        ],
    )?;
    run_checked(
        "ip",
        &[
            "-n",
            &network.namespace,
            "addr",
            "add",
            &format!("{}/30", network.guest_gateway),
            "dev",
            "tap0",
        ],
    )?;
    run_checked(
        "ip",
        &["-n", &network.namespace, "link", "set", "tap0", "up"],
    )?;
    run_checked(
        "ip",
        &[
            "netns",
            "exec",
            &network.namespace,
            sysctl,
            "-q",
            "-w",
            "net.ipv4.ip_forward=1",
        ],
    )?;
    run_checked(
        "ip",
        &[
            "-n",
            &network.namespace,
            "route",
            "add",
            "default",
            "via",
            &network.transit_host.to_string(),
        ],
    )?;
    run_checked(
        "ip",
        &[
            "route",
            "add",
            &network.guest_cidr,
            "via",
            &network.transit_namespace.to_string(),
            "dev",
            &network.host_veth,
        ],
    )?;

    install_network_firewall(config, network)?;
    // Docker and similar host services commonly leave the compatibility
    // FORWARD chain at DROP. An accept verdict in our nftables base chain does
    // not override a later base-chain drop, so admit only this VM's veth there.
    // The nftables rules above still enforce source validation, destination
    // filtering, return-traffic state, and cross-VM isolation.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/network-setup.md#host-network-setup
    run_checked(
        "iptables",
        &[
            "-w",
            "-I",
            "FORWARD",
            "1",
            "-i",
            &network.host_veth,
            "-j",
            "ACCEPT",
        ],
    )?;
    run_checked(
        "iptables",
        &[
            "-w",
            "-I",
            "FORWARD",
            "1",
            "-o",
            &network.host_veth,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ],
    )?;
    Ok(())
}

fn install_network_firewall(config: &FirecrackerConfig, network: &NetworkConfig) -> Result<()> {
    let mut rules = String::new();
    let table = &network.nft_table;
    let interface = &network.host_veth;
    writeln!(rules, "add table inet {table}")?;
    writeln!(
        rules,
        "add chain inet {table} input {{ type filter hook input priority filter; policy accept; }}"
    )?;
    writeln!(
        rules,
        "add chain inet {table} forward {{ type filter hook forward priority filter; policy accept; }}"
    )?;
    writeln!(
        rules,
        "add chain inet {table} postrouting {{ type nat hook postrouting priority srcnat; policy accept; }}"
    )?;
    writeln!(
        rules,
        "add rule inet {table} input iifname {interface} ct state established,related counter accept"
    )?;
    writeln!(
        rules,
        "add rule inet {table} input iifname {interface} counter drop"
    )?;
    // Every forward rule below matches with `ip ...` selectors, which only
    // match IPv4. Without this drop, an IPv6 frame from the guest would fall
    // through them all to the final accept; nothing routes IPv6 today, but the
    // IPv4-only egress property should hold by rule, not by topology.
    writeln!(
        rules,
        "add rule inet {table} forward iifname {interface} meta nfproto ipv6 counter drop"
    )?;
    writeln!(
        rules,
        "add rule inet {table} forward iifname {interface} ip saddr != {} counter drop",
        network.guest_cidr
    )?;
    writeln!(
        rules,
        "add rule inet {table} forward iifname {interface} ip daddr {EXO_NETWORK_CIDR} counter reject"
    )?;
    for cidr in &config.allowed_egress_cidrs {
        writeln!(
            rules,
            "add rule inet {table} forward iifname {interface} ip daddr {cidr} counter accept"
        )?;
    }
    writeln!(
        rules,
        "add rule inet {table} forward iifname {interface} ip daddr {{ {} }} counter reject",
        BLOCKED_EGRESS_CIDRS.join(", ")
    )?;
    writeln!(
        rules,
        "add rule inet {table} forward iifname {interface} counter accept"
    )?;
    writeln!(
        rules,
        "add rule inet {table} forward oifname {interface} ct state established,related counter accept"
    )?;
    writeln!(
        rules,
        "add rule inet {table} forward oifname {interface} counter drop"
    )?;
    writeln!(
        rules,
        "add rule inet {table} postrouting ip saddr {} counter masquerade",
        network.guest_cidr
    )?;
    run_checked_input("nft", &["-f", "-"], rules.as_bytes())
}

fn cleanup_network_blocking(network: &NetworkConfig) {
    remove_all_matching_rules(
        "iptables",
        &[
            "-w",
            "-D",
            "FORWARD",
            "-i",
            &network.host_veth,
            "-j",
            "ACCEPT",
        ],
    );
    remove_all_matching_rules(
        "iptables",
        &[
            "-w",
            "-D",
            "FORWARD",
            "-o",
            &network.host_veth,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ],
    );
    run_ignoring_status("nft", &["delete", "table", "inet", &network.nft_table]);
    run_ignoring_status("ip", &["route", "del", &network.guest_cidr]);
    run_ignoring_status("ip", &["link", "del", &network.host_veth]);
    run_ignoring_status("ip", &["netns", "del", &network.namespace]);
}

fn run_checked(program: &str, arguments: &[&str]) -> Result<()> {
    let executable = trusted_host_command(program)?;
    let output = Command::new(&executable)
        .args(arguments)
        .output()
        .with_context(|| format!("running {}", executable.display()))?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{} failed with status {}: {}",
        program,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn run_checked_input(program: &str, arguments: &[&str], input: &[u8]) -> Result<()> {
    let executable = trusted_host_command(program)?;
    let mut child = Command::new(&executable)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", executable.display()))?;
    child
        .stdin
        .as_mut()
        .context("opening command stdin")?
        .write_all(input)?;
    child.stdin = None;
    let output = child.wait_with_output()?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{} failed with status {}: {}",
        program,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn run_ignoring_status(program: &str, arguments: &[&str]) {
    let executable = match trusted_host_command(program) {
        Ok(executable) => executable,
        Err(error) => {
            tracing::debug!(program, %error, "Firecracker cleanup command is not trusted");
            return;
        }
    };
    if let Err(error) = Command::new(executable).args(arguments).output() {
        tracing::debug!(program, %error, "Firecracker cleanup command could not start");
    }
}

fn remove_all_matching_rules(program: &str, arguments: &[&str]) {
    let executable = match trusted_host_command(program) {
        Ok(executable) => executable,
        Err(error) => {
            tracing::debug!(program, %error, "Firecracker cleanup command is not trusted");
            return;
        }
    };
    for _ in 0..64 {
        match Command::new(&executable).args(arguments).output() {
            Ok(output) if output.status.success() => {}
            _ => return,
        }
    }
    tracing::warn!(
        program,
        "stopped after removing 64 duplicate Firecracker firewall rules"
    );
}

fn jail_dir(config: &FirecrackerConfig, machine_id: &str) -> PathBuf {
    let executable_name = config
        .firecracker_bin
        .file_name()
        .expect("validated Firecracker binary should have a file name");
    config
        .state_root
        .join("jailer")
        .join(executable_name)
        .join(machine_id)
}

fn jail_root(config: &FirecrackerConfig, machine_id: &str) -> PathBuf {
    jail_dir(config, machine_id).join("root")
}

fn firecracker_cgroup_dir(config: &FirecrackerConfig, machine_id: &str) -> PathBuf {
    let executable_name = config
        .firecracker_bin
        .file_name()
        .expect("validated Firecracker binary should have a file name");
    PathBuf::from("/sys/fs/cgroup")
        .join(executable_name)
        .join(machine_id)
}

fn remove_firecracker_cgroup(path: &Path) -> Result<()> {
    let started = Instant::now();
    loop {
        match fs::remove_dir(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error)
                if error.raw_os_error() == Some(libc::EBUSY)
                    && started.elapsed() < PROCESS_STOP_TIMEOUT =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("removing Firecracker cgroup {}", path.display()));
            }
        }
    }
}

fn prepare_and_launch_blocking(
    config: &FirecrackerConfig,
    request: &SandboxRequest,
    record: &MachineRecord,
) -> Result<GuestReadiness> {
    if let Some(template) = record.snapshot_template.as_ref()
        && prepare_snapshot_overlay(config, record, &template.key)?
    {
        return launch_snapshot_clone(config, request, record, &template.key);
    }
    let root = jail_root(config, &record.machine_id);
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, Permissions::from_mode(0o700))?;
    let ready_listener = prepare_ready_listener(&root)?;
    let rootfs = root.join("rootfs.ext4");
    if !rootfs.try_exists()? {
        fs::hard_link(&request.spec.image, &rootfs).with_context(|| {
            format!(
                "linking immutable Firecracker base image {} into jail {}",
                request.spec.image,
                rootfs.display()
            )
        })?;
    }
    let rootfs_metadata = fs::metadata(&rootfs)
        .with_context(|| format!("reading Firecracker rootfs metadata {}", rootfs.display()))?;
    if rootfs_metadata.mode() & 0o222 != 0 || rootfs_metadata.mode() & 0o004 == 0 {
        bail!(
            "Firecracker cached base image must be immutable and readable in its jail: {}",
            rootfs.display()
        );
    }
    let overlay = root.join("overlay.ext4");
    let created_overlay = !overlay.try_exists()?;
    if created_overlay {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&overlay)?;
        file.set_len(gib_bytes(config.image_size_gib, "image")?)?;
        run_checked(
            "mkfs.ext4",
            &[
                "-q",
                "-F",
                "-E",
                "lazy_itable_init=1,lazy_journal_init=1",
                &overlay.to_string_lossy(),
            ],
        )?;
    }
    let kernel = root.join("vmlinux");
    fs::hard_link(&config.kernel, &kernel).with_context(|| {
        format!(
            "linking cached Firecracker kernel {} into {}",
            config.kernel.display(),
            kernel.display()
        )
    })?;
    let initramfs = root.join("initramfs.cpio");
    fs::hard_link(&config.initramfs, &initramfs).with_context(|| {
        format!(
            "linking cached Firecracker initramfs {} into {}",
            config.initramfs.display(),
            initramfs.display()
        )
    })?;
    let host_uid = jailer_uid(config, record)?;
    prepare_api_run_dir(&root, host_uid)?;
    chown(&overlay, Some(host_uid), Some(host_uid))?;
    fs::set_permissions(&overlay, Permissions::from_mode(0o600))?;

    if let Some(workspace_id) = record.workspace_id.as_ref() {
        let workspace = config
            .state_root
            .join("workspaces")
            .join(format!("{workspace_id}.ext4"));
        if !workspace.try_exists()? {
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&workspace)
                .with_context(|| {
                    format!(
                        "creating Firecracker workspace disk {}",
                        workspace.display()
                    )
                })?;
            file.set_len(gib_bytes(config.workspace_size_gib, "workspace")?)?;
            run_checked("mkfs.ext4", &["-q", "-F", &workspace.to_string_lossy()])?;
            fs::set_permissions(&workspace, Permissions::from_mode(0o600))?;
        }
        let jailed_workspace = root.join("workspace.ext4");
        if jailed_workspace.try_exists()? {
            fs::remove_file(&jailed_workspace)?;
        }
        fs::hard_link(&workspace, &jailed_workspace).with_context(|| {
            format!(
                "linking Firecracker workspace {} into {}",
                workspace.display(),
                jailed_workspace.display()
            )
        })?;
        chown(&jailed_workspace, Some(host_uid), Some(host_uid))?;
    }

    let vm_config = firecracker_vm_configuration(config, request, record);
    let vm_config_path = root.join("vm-config.json");
    fs::write(&vm_config_path, serde_json::to_vec(&vm_config)?)?;
    chown(&vm_config_path, Some(host_uid), Some(host_uid))?;
    fs::set_permissions(&vm_config_path, Permissions::from_mode(0o400))?;

    // Keep the API available so fork() can pause and snapshot the running VM.
    // The config file still starts the VM atomically without a sequence of API
    // setup requests.
    spawn_jailed_firecracker(config, record, &root, &["--config-file", "/vm-config.json"])?;
    Ok(GuestReadiness::Signal(ready_listener))
}

fn spawn_jailed_firecracker(
    config: &FirecrackerConfig,
    record: &MachineRecord,
    jail_root: &Path,
    firecracker_arguments: &[&str],
) -> Result<()> {
    let host_uid = jailer_uid(config, record)?;
    let memory_max = u64::from(
        config
            .memory_mib
            .checked_add(256)
            .context("Firecracker cgroup memory limit overflow")?,
    ) * 1024
        * 1024;
    let cpu_max = format!("{} 100000", u32::from(config.vcpu_count) * 100_000);
    // Always use the matching jailer: it creates the mount/PID namespaces and
    // cgroup, then drops to a unique unprivileged UID before execing Firecracker.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md#jailer-operation
    let mut command = Command::new(&config.jailer_bin);
    command
        .arg("--id")
        .arg(&record.machine_id)
        .arg("--exec-file")
        .arg(&config.firecracker_bin)
        .arg("--uid")
        .arg(host_uid.to_string())
        .arg("--gid")
        .arg(host_uid.to_string())
        .arg("--chroot-base-dir")
        .arg(config.state_root.join("jailer"));
    if record.network_enabled {
        command
            .arg("--netns")
            .arg(PathBuf::from("/var/run/netns").join(&record.network().namespace));
    }
    command
        .arg("--new-pid-ns")
        .arg("--cgroup-version")
        .arg("2")
        .arg("--cgroup")
        .arg(format!("memory.max={memory_max}"))
        .arg("--cgroup")
        .arg(format!("cpu.max={cpu_max}"));
    // The API socket path is injected here, next to the constant that
    // validate_jailed_socket_paths budgets, so no call site can drift from
    // the path the rest of the code waits on.
    let api_socket_arguments = ["--api-sock", JAILED_API_SOCKET];
    // Never route VMM output to a growable file: a compromised guest kernel
    // can reactivate the serial device despite 8250.nr_uarts=0. /dev/null also
    // avoids tying the VMM to a pipe reader in this controller process, which
    // is what lets the next controller adopt it after this process exits.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md#8250-serial-device
    command
        .arg("--resource-limit")
        .arg("no-file=4096")
        .arg("--")
        .args(api_socket_arguments)
        .args(firecracker_arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let log_path = jail_root.join("firecracker.stderr");
    File::create(&log_path)?;
    fs::set_permissions(&log_path, Permissions::from_mode(0o600))?;
    // Deliberately do not pass --no-seccomp or a custom filter. Release builds'
    // embedded default filters are Firecracker's recommended production setting.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/seccomp.md#default-filters-recommended
    let mut child = command
        .spawn()
        .context("launching Firecracker through jailer")?;
    let machine_id = record.machine_id.clone();
    std::thread::spawn(move || match child.wait() {
        Ok(status) if !status.success() => {
            tracing::warn!(machine_id, %status, "Firecracker process exited unsuccessfully");
        }
        Err(error) => {
            tracing::warn!(machine_id, %error, "failed waiting for Firecracker process");
        }
        _ => {}
    });
    Ok(())
}

fn firecracker_vm_configuration(
    config: &FirecrackerConfig,
    request: &SandboxRequest,
    record: &MachineRecord,
) -> FirecrackerVmConfiguration {
    let network = record.network();
    // Disable the guest serial driver; VMM output is additionally discarded in
    // spawn_jailed_firecracker because upstream documents that a guest can
    // reactivate the serial device.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md#8250-serial-device
    let mut boot_args =
        String::from("reboot=k panic=1 pci=off rdinit=/init 8250.nr_uarts=0 quiet loglevel=1");
    if record.network_enabled {
        boot_args.push_str(&format!(
            " exo_guest_ip={} exo_gateway={} exo_prefix=30 exo_dns={}",
            network.guest_ip, network.guest_gateway, config.dns_server
        ));
    }
    if record.workspace_id.is_some() {
        boot_args.push_str(" exo_workspace=");
        boot_args.push_str(&request.spec.default_workdir);
    }
    let mut drives = vec![
        FirecrackerDrive {
            drive_id: "rootfs",
            path_on_host: "/rootfs.ext4",
            is_root_device: false,
            is_read_only: true,
            cache_type: "Unsafe",
            io_engine: "Sync",
        },
        FirecrackerDrive {
            drive_id: "overlay",
            path_on_host: "/overlay.ext4",
            is_root_device: false,
            is_read_only: false,
            cache_type: "Writeback",
            io_engine: "Sync",
        },
    ];
    if record.workspace_id.is_some() {
        // Writeback advertises virtio-blk FLUSH to the guest and turns a guest
        // flush into fsync(2) on the backing file. Combined with the explicit
        // guest sync during stop, this makes the workspace a durability boundary.
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/api_requests/block-caching.md#writeback-mode
        drives.push(FirecrackerDrive {
            drive_id: "workspace",
            path_on_host: "/workspace.ext4",
            is_root_device: false,
            is_read_only: false,
            cache_type: "Writeback",
            io_engine: "Sync",
        });
    }
    // The control channel is vsock rather than TCP: networking-disabled sandboxes
    // still support exec, and the guest agent is never reachable through egress.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md#setting-up-the-virtio-vsock-device
    let network_interfaces = if record.network_enabled {
        let bucket = FirecrackerRateLimiter {
            bandwidth: FirecrackerTokenBucket {
                size: config.network_bytes_per_second,
                refill_time: 1000,
            },
        };
        vec![FirecrackerNetworkInterface {
            iface_id: "eth0",
            guest_mac: network.guest_mac,
            host_dev_name: "tap0",
            rx_rate_limiter: bucket.clone(),
            tx_rate_limiter: bucket,
        }]
    } else {
        Vec::new()
    };
    FirecrackerVmConfiguration {
        boot_source: FirecrackerBootSource {
            kernel_image_path: "/vmlinux",
            initrd_path: "/initramfs.cpio",
            boot_args,
        },
        drives,
        machine_config: FirecrackerMachineConfiguration {
            vcpu_count: config.vcpu_count,
            mem_size_mib: config.memory_mib,
            smt: false,
            track_dirty_pages: false,
        },
        network_interfaces,
        vsock: FirecrackerVsock {
            guest_cid: record.slot + 3,
            uds_path: JAILED_VSOCK,
        },
        // virtio-rng is upstream's recommended extra entropy source for
        // snapshot clones, alongside the VMGenID reseed: guests whose kernel
        // has CONFIG_HW_RANDOM_VIRTIO feed it into their CSPRNG, and the fork
        // path restores this device from the source's snapshot. Rate-limited
        // so a guest cannot spin the host's CSPRNG at line speed.
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/random-for-clones.md
        entropy: FirecrackerEntropy {
            rate_limiter: FirecrackerRateLimiter {
                bandwidth: FirecrackerTokenBucket {
                    size: 64 * 1024,
                    refill_time: 1000,
                },
            },
        },
    }
}

fn validate_snapshot_key(key: &str) -> Result<()> {
    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid Firecracker snapshot template key");
    }
    Ok(())
}

fn snapshot_template_dir(config: &FirecrackerConfig, key: &str) -> Result<PathBuf> {
    validate_snapshot_key(key)?;
    Ok(config.state_root.join("snapshots").join(key))
}

fn snapshot_cow_path(config: &FirecrackerConfig, machine_id: &str) -> PathBuf {
    config
        .state_root
        .join("cows")
        .join(format!("{machine_id}.img"))
}

fn replace_hard_link(source: &Path, destination: &Path) -> Result<()> {
    if destination.try_exists()? {
        fs::remove_file(destination)
            .with_context(|| format!("removing stale jail file {}", destination.display()))?;
    }
    fs::hard_link(source, destination).with_context(|| {
        format!(
            "linking Firecracker asset {} into {}",
            source.display(),
            destination.display()
        )
    })
}

fn prepare_snapshot_jail_files(
    config: &FirecrackerConfig,
    request: &SandboxRequest,
    record: &MachineRecord,
) -> Result<PathBuf> {
    let root = jail_root(config, &record.machine_id);
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, Permissions::from_mode(0o700))?;
    replace_hard_link(Path::new(&request.spec.image), &root.join("rootfs.ext4"))?;
    replace_hard_link(&config.kernel, &root.join("vmlinux"))?;
    replace_hard_link(&config.initramfs, &root.join("initramfs.cpio"))?;
    for path in [
        root.join("rootfs.ext4"),
        root.join("vmlinux"),
        root.join("initramfs.cpio"),
    ] {
        fs::set_permissions(path, Permissions::from_mode(0o444))?;
    }
    Ok(root)
}

fn prepare_api_run_dir(root: &Path, uid: u32) -> Result<()> {
    let run = root.join("run");
    fs::create_dir_all(&run)?;
    chown(&run, Some(uid), Some(uid))?;
    fs::set_permissions(run, Permissions::from_mode(0o700))
        .context("setting Firecracker API run directory permissions")
}

fn wait_for_firecracker_api(root: &Path, machine_id: &str) -> Result<PathBuf> {
    let socket = jailed_path_on_host(root, JAILED_API_SOCKET);
    let pid_path = root.join("firecracker.pid");
    let started = Instant::now();
    let mut observed_process = false;
    while started.elapsed() < PID_FILE_STARTUP_TIMEOUT {
        if socket.try_exists()? {
            match StdUnixStream::connect(&socket) {
                Ok(_) => return Ok(socket),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("checking Firecracker API readiness at {}", socket.display())
                    });
                }
            }
        }
        if process_running(&pid_path) {
            observed_process = true;
        } else if observed_process {
            bail!("Firecracker {machine_id} exited before its API became ready");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let stderr = tail_file(&root.join("firecracker.stderr"))?;
    bail!("Firecracker {machine_id} API did not become ready: {stderr}")
}

fn firecracker_api_patch<T: Serialize>(socket: &Path, path: &str, body: &T) -> Result<()> {
    firecracker_api_request(socket, "PATCH", path, body, FIRECRACKER_API_TIMEOUT)
}

fn firecracker_api_request<T: Serialize>(
    socket: &Path,
    method: &str,
    path: &str,
    body: &T,
    timeout: Duration,
) -> Result<()> {
    let body = serde_json::to_vec(body)?;
    let mut stream = StdUnixStream::connect(socket)
        .with_context(|| format!("connecting to Firecracker API {}", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    // Read::take bounds every byte read from the VMM, status line and headers
    // included; see FIRECRACKER_API_MAX_RESPONSE_BYTES.
    let mut reader = StdBufReader::new(stream.take(FIRECRACKER_API_MAX_RESPONSE_BYTES));
    let mut status_line = String::new();
    if reader.read_line(&mut status_line)? == 0 {
        bail!("Firecracker API returned an empty response");
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .context("Firecracker API response did not include a status")?
        .parse::<u16>()
        .context("Firecracker API returned an invalid status")?;
    let mut content_length = 0_usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            bail!("Firecracker API response ended before its headers");
        }
        if header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value
                .trim()
                .parse()
                .context("Firecracker API returned an invalid Content-Length")?;
        }
    }
    // Cap before allocating: the length is a VMM-supplied number and must not
    // size an allocation on its own.
    if content_length > FIRECRACKER_API_MAX_RESPONSE_BYTES as usize {
        bail!("Firecracker API response is too large: {content_length} bytes");
    }
    let mut response_body = vec![0_u8; content_length];
    reader.read_exact(&mut response_body)?;
    if (200..300).contains(&status) {
        return Ok(());
    }
    bail!(
        "Firecracker API {method} {path} failed with status {status}: {}",
        String::from_utf8_lossy(&response_body)
    )
}

fn validate_snapshot_template(config: &FirecrackerConfig, directory: &Path) -> Result<bool> {
    let complete = directory.join("complete");
    if !complete.try_exists()? {
        return Ok(false);
    }
    let expected = [
        (directory.join("state"), None),
        (
            directory.join("memory"),
            Some(u64::from(config.memory_mib) * 1024 * 1024),
        ),
        (
            directory.join("overlay.ext4"),
            Some(gib_bytes(config.image_size_gib, "image")?),
        ),
    ];
    for (path, expected_length) in expected {
        let metadata = fs::metadata(&path)
            .with_context(|| format!("reading Firecracker snapshot asset {}", path.display()))?;
        if !metadata.is_file()
            || expected_length.is_some_and(|length| metadata.len() != length)
            || metadata.uid() != 0
            || metadata.mode() & 0o222 != 0
            || metadata.mode() & 0o004 == 0
        {
            bail!(
                "Firecracker snapshot asset must be immutable and root-owned: {}",
                path.display()
            );
        }
    }
    Ok(true)
}

fn gib_bytes(size_gib: u64, label: &str) -> Result<u64> {
    size_gib
        .checked_mul(1024 * 1024 * 1024)
        .with_context(|| format!("Firecracker {label} size overflows bytes"))
}

fn snapshot_template_ready(config: &FirecrackerConfig, key: &str) -> Result<bool> {
    let directory = snapshot_template_dir(config, key)?;
    if !directory.try_exists()? {
        return Ok(false);
    }
    validate_snapshot_template(config, &directory)
}

fn fork_snapshot_template_key(source: &MachineRecord, target_machine_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SNAPSHOT_FORMAT_VERSION.to_le_bytes());
    hash_snapshot_string(&mut hasher, "point-in-time-fork");
    hash_snapshot_string(&mut hasher, &source.machine_id);
    hash_snapshot_string(&mut hasher, &source.spec_hash);
    hash_snapshot_string(&mut hasher, target_machine_id);
    format!("{:x}", hasher.finalize())
}

fn explicit_snapshot_template_key(source: &MachineRecord, snapshot_id: Uuid) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SNAPSHOT_FORMAT_VERSION.to_le_bytes());
    hash_snapshot_string(&mut hasher, "explicit-snapshot");
    hash_snapshot_string(&mut hasher, &source.machine_id);
    hash_snapshot_string(&mut hasher, &source.spec_hash);
    hasher.update(snapshot_id.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn remove_directory_if_present(path: &Path) -> Result<()> {
    if path.try_exists()? {
        fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

fn capture_snapshot_template(
    config: &FirecrackerConfig,
    source: &MachineRecord,
    template_key: &str,
) -> Result<()> {
    let destination = snapshot_template_dir(config, template_key)?;
    if destination.try_exists()? {
        remove_directory_if_present(&destination)?;
    }

    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = config.state_root.join("snapshots").join(format!(
        ".{template_key}.capture.{}.{}",
        std::process::id(),
        sequence
    ));
    fs::create_dir(&temporary)?;
    fs::set_permissions(&temporary, Permissions::from_mode(0o700))?;

    let root = jail_root(config, &source.machine_id);
    let output_name = format!("snapshot-{}", &template_key[..12]);
    let output = root.join(&output_name);
    remove_directory_if_present(&output)?;
    fs::create_dir(&output)?;
    let uid = jailer_uid(config, source)?;
    chown(&output, Some(uid), Some(uid))?;
    fs::set_permissions(&output, Permissions::from_mode(0o700))?;

    let api = wait_for_firecracker_api(&root, &source.machine_id)?;
    if let Err(error) = firecracker_api_patch(&api, "/vm", &FirecrackerVmState { state: "Paused" })
    {
        remove_directory_if_present(&temporary)?;
        remove_directory_if_present(&output)?;
        return Err(error).context("pausing Firecracker snapshot source");
    }
    let snapshot_path = format!("/{output_name}/state");
    let memory_path = format!("/{output_name}/memory");
    let paused_result = (|| {
        // A snapshot captures a full point-in-time device/RAM image once. Clones
        // map the immutable memory file privately and get independent COW disks.
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md#full-and-diff-snapshots
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md#memory-backend
        firecracker_api_request(
            &api,
            "PUT",
            "/snapshot/create",
            &FirecrackerSnapshotCreate {
                snapshot_type: "Full",
                snapshot_path: &snapshot_path,
                mem_file_path: &memory_path,
            },
            FIRECRACKER_SNAPSHOT_CREATE_TIMEOUT,
        )?;
        // Only the disk copy must happen inside the pause window: the overlay
        // has to match the memory image byte-for-byte, and the source starts
        // writing to it again the moment it resumes.
        copy_sparse_reflink(&root.join("overlay.ext4"), &temporary.join("overlay.ext4"))?;
        Ok::<(), anyhow::Error>(())
    })();
    let resume = firecracker_api_patch(&api, "/vm", &FirecrackerVmState { state: "Resumed" });
    let result = match (paused_result, resume) {
        (Err(error), Err(resume_error)) => Err(error).context(format!(
            "creating Firecracker snapshot; source resume also failed: {resume_error:#}"
        )),
        (Err(error), Ok(())) => Err(error).context("creating Firecracker snapshot"),
        (Ok(()), Err(error)) => Err(error).context("resuming Firecracker snapshot source"),
        (Ok(()), Ok(())) => Ok(()),
    };
    let result = result.and_then(|()| {
        // Copy -- never hard-link -- the snapshot out of the source VM's jail.
        // The state and memory files were created by the jailed VMM under its
        // unprivileged uid, so a compromised VMM can hold an open fd to them; a
        // hard link would leave the published template writable through that fd
        // even after the chown/chmod below, and Firecracker treats snapshots as
        // trusted VMM input when a clone loads them. The copy runs after the
        // source resumed, so it costs no pause time, and reflink makes it
        // metadata-only on XFS/btrfs.
        for name in ["state", "memory"] {
            copy_sparse_reflink(&output.join(name), &temporary.join(name))?;
        }
        for path in [
            temporary.join("state"),
            temporary.join("memory"),
            temporary.join("overlay.ext4"),
        ] {
            chown(&path, Some(0), Some(0))?;
            fs::set_permissions(&path, Permissions::from_mode(0o444))?;
            File::open(path)?.sync_all()?;
        }
        let complete = temporary.join("complete");
        File::create(&complete)?.sync_all()?;
        fs::set_permissions(&complete, Permissions::from_mode(0o444))?;
        Ok(())
    });
    let output_cleanup = remove_directory_if_present(&output);
    let result = match (result, output_cleanup) {
        (Err(error), Err(cleanup_error)) => Err(error).context(format!(
            "creating Firecracker snapshot; jail cleanup also failed: {cleanup_error:#}"
        )),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error).context("cleaning up Firecracker snapshot jail output"),
        (Ok(()), Ok(())) => Ok(()),
    };
    if let Err(error) = result {
        if let Err(cleanup_error) = remove_directory_if_present(&temporary) {
            return Err(error).context(format!(
                "cleaning up failed Firecracker snapshot also failed: {cleanup_error:#}"
            ));
        }
        return Err(error);
    }
    fs::rename(&temporary, &destination)
        .with_context(|| format!("publishing Firecracker snapshot {}", destination.display()))?;
    validate_snapshot_template(config, &destination)?;
    Ok(())
}

fn prepare_snapshot_overlay(
    config: &FirecrackerConfig,
    record: &MachineRecord,
    template_key: &str,
) -> Result<bool> {
    let cow = snapshot_cow_path(config, &record.machine_id);
    let created = !cow.try_exists()?;
    if created {
        let origin = snapshot_template_dir(config, template_key)?.join("overlay.ext4");
        copy_sparse_reflink(&origin, &cow)?;
        fs::set_permissions(&cow, Permissions::from_mode(0o600))?;
    }
    let root = jail_root(config, &record.machine_id);
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, Permissions::from_mode(0o700))?;
    let overlay = root.join("overlay.ext4");
    replace_hard_link(&cow, &overlay)?;
    let uid = jailer_uid(config, record)?;
    chown(&overlay, Some(uid), Some(uid))?;
    fs::set_permissions(&overlay, Permissions::from_mode(0o600))?;
    Ok(created)
}

fn launch_snapshot_clone(
    config: &FirecrackerConfig,
    request: &SandboxRequest,
    record: &MachineRecord,
    template_key: &str,
) -> Result<GuestReadiness> {
    let root = prepare_snapshot_jail_files(config, request, record)?;
    let host_uid = jailer_uid(config, record)?;
    prepare_api_run_dir(&root, host_uid)?;
    let snapshot = root.join("snapshot");
    fs::create_dir_all(&snapshot)?;
    let template = snapshot_template_dir(config, template_key)?;
    replace_hard_link(&template.join("state"), &snapshot.join("state"))?;
    replace_hard_link(&template.join("memory"), &snapshot.join("memory"))?;
    for path in [snapshot.join("state"), snapshot.join("memory")] {
        fs::set_permissions(path, Permissions::from_mode(0o444))?;
    }
    fs::set_permissions(&snapshot, Permissions::from_mode(0o555))?;
    spawn_jailed_firecracker(config, record, &root, &[])?;
    let api = wait_for_firecracker_api(&root, &record.machine_id)?;
    // Firecracker updates VMGenID before resuming vCPUs, so supported Linux
    // kernels reseed their CSPRNG before cloned workloads consume randomness.
    // No user process or secret is admitted before this restore completes.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/random-for-clones.md#linux-kernels-with-vmgenid-support
    firecracker_api_request(
        &api,
        "PUT",
        "/snapshot/load",
        &FirecrackerSnapshotLoad {
            snapshot_path: "/snapshot/state",
            mem_backend: FirecrackerMemoryBackend {
                backend_path: "/snapshot/memory",
                backend_type: "File",
            },
            track_dirty_pages: false,
            resume_vm: true,
        },
        FIRECRACKER_API_TIMEOUT,
    )?;
    Ok(GuestReadiness::Probe)
}

fn prepare_ready_listener(root: &Path) -> Result<StdUnixListener> {
    let run = root.join("run");
    fs::create_dir_all(&run)?;
    fs::set_permissions(&run, Permissions::from_mode(0o755))?;
    let path = jailed_path_on_host(root, &format!("{JAILED_VSOCK}_{GUEST_READY_HOST_PORT}"));
    if path.try_exists()? {
        fs::remove_file(&path)?;
    }
    let listener = StdUnixListener::bind(&path)
        .with_context(|| format!("binding Firecracker guest-ready socket {}", path.display()))?;
    fs::set_permissions(&path, Permissions::from_mode(0o666))?;
    Ok(listener)
}

fn jailer_uid(config: &FirecrackerConfig, record: &MachineRecord) -> Result<u32> {
    config
        .jailer_uid_base
        .checked_add(record.slot)
        .context("Firecracker jailer UID overflow")
}

fn process_running(pid_path: &Path) -> bool {
    let Ok(pid) = fs::read_to_string(pid_path) else {
        return false;
    };
    let Ok(pid) = pid.trim().parse::<u32>() else {
        return false;
    };
    PathBuf::from(format!("/proc/{pid}")).exists()
}

#[cfg(target_os = "linux")]
fn stop_machine_process_blocking(machine_id: &str, pid_path: &Path) -> Result<()> {
    let Ok(pid) = fs::read_to_string(pid_path) else {
        return Ok(());
    };
    let pid = pid
        .trim()
        .parse::<u32>()
        .context("invalid Firecracker pid")?;
    let rustix_pid = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .context("invalid Firecracker pid")?;
    let pidfd = match pidfd_open(rustix_pid, PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        Err(rustix::io::Errno::SRCH) => return Ok(()),
        Err(error) => return Err(error).context("opening Firecracker pidfd"),
    };
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    if !proc_dir.exists() {
        return Ok(());
    }
    let cmdline = fs::read(proc_dir.join("cmdline"))?;
    let arguments = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>();
    let is_firecracker = arguments
        .first()
        .and_then(|argument| Path::new(argument.as_ref()).file_name())
        .is_some_and(|name| name == "firecracker");
    let id_argument = format!("--id={machine_id}");
    let has_machine_id = arguments.iter().any(|argument| argument == &id_argument)
        || arguments
            .windows(2)
            .any(|arguments| arguments[0] == "--id" && arguments[1] == machine_id);
    let cgroup = fs::read_to_string(proc_dir.join("cgroup"))?;
    let in_machine_cgroup = cgroup.lines().any(|line| {
        line.rsplit_once(':')
            .is_some_and(|(_, path)| path.split('/').any(|component| component == machine_id))
    });
    if !is_firecracker || !has_machine_id || !in_machine_cgroup {
        bail!("refusing to stop pid {pid}: it does not match Firecracker machine {machine_id}");
    }
    if !signal_pidfd(&pidfd, Signal::TERM)? {
        return Ok(());
    }
    if wait_for_pidfd(&pidfd, PROCESS_STOP_TIMEOUT)? {
        return Ok(());
    }
    if !signal_pidfd(&pidfd, Signal::KILL)? {
        return Ok(());
    }
    if wait_for_pidfd(&pidfd, PROCESS_STOP_TIMEOUT)? {
        return Ok(());
    }
    bail!("Firecracker process {pid} remained after SIGKILL")
}

#[cfg(target_os = "linux")]
fn signal_pidfd(pidfd: &std::os::fd::OwnedFd, signal: Signal) -> Result<bool> {
    match pidfd_send_signal(pidfd, signal) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(error).context("signaling Firecracker process"),
    }
}

#[cfg(target_os = "linux")]
fn wait_for_pidfd(pidfd: &std::os::fd::OwnedFd, timeout: Duration) -> Result<bool> {
    let timeout = Timespec::try_from(timeout).context("Firecracker stop timeout is too large")?;
    let mut pollfd = [PollFd::new(pidfd, PollFlags::IN)];
    Ok(poll(&mut pollfd, Some(&timeout))? > 0)
}

#[cfg(not(target_os = "linux"))]
fn stop_machine_process_blocking(_machine_id: &str, _pid_path: &Path) -> Result<()> {
    bail!("Firecracker sandbox execution is only supported on Linux")
}

fn tail_file(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(8_000);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((length - start) as usize);
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Clone)]
struct GuestClient {
    shared: Arc<Shared>,
    vsock_path: PathBuf,
}

impl GuestClient {
    fn new(shared: Arc<Shared>, vsock_path: PathBuf) -> Self {
        Self { shared, vsock_path }
    }

    async fn ping(&self) -> Result<()> {
        self.ping_with_timeout(Duration::from_secs(2)).await
    }

    async fn ping_with_timeout(&self, timeout: Duration) -> Result<()> {
        let response: GuestResponse = self
            .invoke_with_timeout(&GuestRequest::Ping, timeout)
            .await?;
        response.into_result("Firecracker guest healthcheck failed")
    }

    async fn invoke<Response>(&self, request: &GuestRequest<'_>) -> Result<Response>
    where
        Response: DeserializeOwned,
    {
        self.invoke_with_timeout(request, GUEST_REQUEST_TIMEOUT)
            .await
    }

    async fn invoke_with_timeout<Response>(
        &self,
        request: &GuestRequest<'_>,
        timeout: Duration,
    ) -> Result<Response>
    where
        Response: DeserializeOwned,
    {
        let payload = serde_json::to_vec(request)?;
        if payload.len() > MAX_GUEST_REQUEST_BYTES {
            bail!(
                "Firecracker guest request is too large: {} bytes",
                payload.len()
            );
        }
        let response = tokio::time::timeout(timeout, vsock_request(&self.vsock_path, &payload))
            .await
            .context("Firecracker guest request timed out")??;
        serde_json::from_slice(&response).with_context(|| {
            format!(
                "decoding Firecracker guest response: {}",
                String::from_utf8_lossy(&response)
            )
        })
    }

    async fn exec(
        &self,
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
        let request_timeout = command
            .timeout
            .and_then(|timeout| timeout.checked_add(Duration::from_secs(5)))
            .unwrap_or(GUEST_REQUEST_TIMEOUT);
        let response: GuestResponse = self
            .invoke_with_timeout(
                &GuestRequest::Exec {
                    argv: &command.argv,
                    env: &command.env,
                    cwd: &cwd,
                    timeout_ms: command
                        .timeout
                        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
                },
                request_timeout,
            )
            .await?;
        let mut stderr = response.stderr.unwrap_or_default();
        if let Some(error) = response.error {
            if !stderr.is_empty() {
                stderr.push('\n');
            }
            stderr.push_str(&error);
        }
        Ok(SandboxCommandOutput {
            ok: response.ok,
            exit_code: response.exit_code,
            stdout: response.stdout.unwrap_or_default(),
            stderr,
            command: command
                .display_argv
                .clone()
                .unwrap_or_else(|| command.argv.clone()),
            cwd: response.cwd.unwrap_or(cwd),
        })
    }

    async fn start_process(
        &self,
        spec: &SandboxSpec,
        command: &SandboxCommand,
        cleanup_machine_id: Option<String>,
    ) -> Result<SandboxProcessParts> {
        if command.argv.is_empty() {
            bail!("sandbox command requires at least one argv entry");
        }
        let cwd = command
            .cwd
            .clone()
            .unwrap_or_else(|| spec.default_workdir.clone());
        let response: GuestResponse = self
            .invoke(&GuestRequest::StartProcess {
                argv: &command.argv,
                env: &command.env,
                cwd: &cwd,
            })
            .await?;
        if let Some(error) = response.error {
            bail!("Firecracker start_process failed: {error}");
        }
        let process_id = response
            .process_id
            .context("Firecracker start_process response did not include process_id")?;
        let bridge = Arc::new(FirecrackerProcessBridgeClient {
            guest: self.clone(),
            process_id: process_id.clone(),
        });
        let SandboxProcessParts {
            stdout,
            stderr,
            stdin,
            wait,
        } = process_bridge::process_parts(bridge);
        let cleanup = ProcessCleanup {
            guest: self.clone(),
            process_id,
            machine_id: cleanup_machine_id,
        };
        Ok(SandboxProcessParts {
            stdout,
            stderr,
            stdin,
            wait: Box::pin(ProcessWait {
                wait,
                cleanup: Some(cleanup),
            }),
        })
    }

    async fn kill_process(&self, process_id: &str) -> Result<()> {
        let response: GuestResponse = self
            .invoke(&GuestRequest::KillProcess { process_id })
            .await?;
        response.into_result("Firecracker kill_process failed")
    }

    async fn sync_filesystem(&self, path: &str) -> Result<()> {
        let response: GuestResponse = self
            .invoke_with_timeout(
                &GuestRequest::SyncFilesystem { path },
                GUEST_REQUEST_TIMEOUT,
            )
            .await?;
        response.into_result("Firecracker guest filesystem sync failed")
    }

    async fn configure_network(
        &self,
        address: Ipv4Addr,
        gateway: Ipv4Addr,
        prefix: u8,
    ) -> Result<()> {
        let response: GuestResponse = self
            .invoke(&GuestRequest::ConfigureNetwork {
                address,
                gateway,
                prefix,
            })
            .await?;
        response.into_result("Firecracker fork clone network reconfiguration failed")
    }
}

async fn vsock_request(path: &Path, payload: &[u8]) -> Result<Vec<u8>> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to Firecracker vsock {}", path.display()))?;
    let mut stream = AsyncBufReader::new(stream);
    // Firecracker's host-initiated protocol requires CONNECT <guest-port> before
    // the Unix stream is forwarded to the guest AF_VSOCK listener.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md#host-initiated-connections
    stream
        .get_mut()
        .write_all(format!("CONNECT {GUEST_AGENT_PORT}\n").as_bytes())
        .await?;
    stream.get_mut().flush().await?;
    let mut acknowledgement = Vec::new();
    // Bound the read itself, not just the post-read length check: the peer is
    // the VMM, and an endless ack line without a newline would otherwise grow
    // this buffer without limit. A truncated 129-byte read fails the checks.
    let mut limited = stream.take(129);
    limited.read_until(b'\n', &mut acknowledgement).await?;
    let stream = limited.into_inner();
    if acknowledgement.len() > 128
        || !acknowledgement.starts_with(b"OK ")
        || !acknowledgement.ends_with(b"\n")
    {
        bail!(
            "invalid Firecracker vsock acknowledgement: {:?}",
            String::from_utf8_lossy(&acknowledgement)
        );
    }
    let mut stream = stream.into_inner();
    let request_length = u32::try_from(payload.len()).context("guest request length overflow")?;
    stream.write_all(&request_length.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    let response_length = stream.read_u32().await? as usize;
    if response_length > MAX_GUEST_RESPONSE_BYTES {
        bail!("Firecracker guest response is too large: {response_length} bytes");
    }
    let mut response = vec![0; response_length];
    stream.read_exact(&mut response).await?;
    Ok(response)
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum GuestRequest<'a> {
    Ping,
    Exec {
        argv: &'a [String],
        env: &'a HashMap<String, String>,
        cwd: &'a str,
        timeout_ms: Option<u64>,
    },
    StartProcess {
        argv: &'a [String],
        env: &'a HashMap<String, String>,
        cwd: &'a str,
    },
    ProcessBridge {
        process_id: &'a str,
        request: process_bridge::Request,
    },
    KillProcess {
        process_id: &'a str,
    },
    SyncFilesystem {
        path: &'a str,
    },
    ConfigureNetwork {
        address: Ipv4Addr,
        gateway: Ipv4Addr,
        prefix: u8,
    },
}

#[derive(Deserialize)]
struct GuestResponse {
    ok: bool,
    process_id: Option<String>,
    exit_code: Option<i32>,
    stdout: Option<String>,
    stderr: Option<String>,
    cwd: Option<String>,
    error: Option<String>,
}

impl GuestResponse {
    fn into_result(self, context: &str) -> Result<()> {
        if self.ok {
            return Ok(());
        }
        bail!(
            "{context}: {}",
            self.error.unwrap_or_else(|| "unknown error".to_string())
        )
    }
}

struct FirecrackerProcessBridgeClient {
    guest: GuestClient,
    process_id: String,
}

#[async_trait]
impl process_bridge::Client for FirecrackerProcessBridgeClient {
    async fn request(&self, request: process_bridge::Request) -> Result<process_bridge::Response> {
        self.guest
            .invoke(&GuestRequest::ProcessBridge {
                process_id: &self.process_id,
                request,
            })
            .await
    }
}

struct ProcessCleanup {
    guest: GuestClient,
    process_id: String,
    machine_id: Option<String>,
}

impl Drop for ProcessCleanup {
    fn drop(&mut self) {
        let guest = self.guest.clone();
        let process_id = self.process_id.clone();
        let machine_id = self.machine_id.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if let Err(error) = guest.kill_process(&process_id).await {
                tracing::debug!(%error, process_id, "failed to clean up Firecracker guest process");
            }
            if let Some(machine_id) = machine_id
            {
                let _lifecycle_guard = guest.shared.lifecycle_lock.lock().await;
                if let Err(error) = guest.shared.cleanup_machine(&machine_id, true).await {
                    tracing::warn!(%error, machine_id, "failed to clean up one-shot Firecracker machine");
                }
            }
        });
    }
}

struct ProcessWait {
    wait: Pin<Box<dyn Future<Output = Result<i32>> + Send>>,
    cleanup: Option<ProcessCleanup>,
}

impl Future for ProcessWait {
    type Output = Result<i32>;

    fn poll(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Self::Output> {
        match self.wait.as_mut().poll(context) {
            Poll::Ready(result) => {
                self.cleanup.take();
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ProcessWait {
    fn drop(&mut self) {
        self.cleanup.take();
    }
}

async fn wait_for_guest(
    shared: &Shared,
    machine_id: &str,
    listener: StdUnixListener,
) -> Result<()> {
    let pid_path = shared.pid_path(machine_id);
    let stderr_path = jail_root(&shared.config, machine_id).join("firecracker.stderr");
    tokio::task::spawn_blocking(move || wait_for_guest_blocking(&pid_path, &stderr_path, listener))
        .await
        .context("joining Firecracker guest-ready wait")?
}

async fn wait_for_restored_guest(shared: &Arc<Shared>, machine: &Machine) -> Result<()> {
    let started = Instant::now();
    let pid_path = shared.pid_path(&machine.record.machine_id);
    let stderr_path =
        jail_root(&shared.config, &machine.record.machine_id).join("firecracker.stderr");
    let guest = GuestClient::new(Arc::clone(shared), machine.vsock_path.clone());
    let mut observed_process = false;
    while started.elapsed() < GUEST_READY_TIMEOUT {
        if process_running(&pid_path) {
            observed_process = true;
        } else if observed_process {
            bail!(
                "restored Firecracker process exited before the guest agent became ready: {}",
                tail_file(&stderr_path)?
            );
        } else if started.elapsed() >= PID_FILE_STARTUP_TIMEOUT {
            bail!(
                "restored Firecracker process did not become observable: {}",
                tail_file(&stderr_path)?
            );
        }
        if guest
            .ping_with_timeout(Duration::from_millis(100))
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    bail!(
        "restored Firecracker guest agent did not become ready: {}",
        tail_file(&stderr_path)?
    )
}

fn wait_for_guest_blocking(
    pid_path: &Path,
    stderr_path: &Path,
    listener: StdUnixListener,
) -> Result<()> {
    let started = Instant::now();
    // With --new-pid-ns the jailer clones Firecracker and only then writes the
    // child's host PID, so Command::spawn returning can precede firecracker.pid.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md#jailer-usage
    let mut observed_process = false;
    listener.set_nonblocking(true)?;
    while started.elapsed() < GUEST_READY_TIMEOUT {
        if process_running(pid_path) {
            observed_process = true;
        } else if observed_process {
            bail!("Firecracker process exited while waiting for the guest agent");
        } else if started.elapsed() >= PID_FILE_STARTUP_TIMEOUT {
            bail!(
                "Firecracker process did not become observable after launch; {}",
                tail_file(stderr_path)?
            );
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                let mut marker = [0_u8; 1];
                stream.read_exact(&mut marker)?;
                if marker != [1] {
                    bail!("invalid Firecracker guest-ready marker");
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("accepting Firecracker guest-ready signal"),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    bail!(
        "Firecracker guest agent did not become ready; {}",
        tail_file(stderr_path)?
    )
}

async fn touch_machine(
    shared: &Shared,
    machines: &Mutex<HashMap<SandboxKey, WarmMachineEntry>>,
    key: &SandboxKey,
    machine_id: &str,
) -> Result<()> {
    let touched = {
        let mut machines = machines.lock().await;
        if let Some(entry) = machines.get_mut(key)
            && entry.machine_id == machine_id
        {
            entry.last_used_at = Instant::now();
            true
        } else {
            false
        }
    };
    if touched {
        shared.touch_machine_lease(machine_id).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "firecracker_tests.rs"]
mod tests;

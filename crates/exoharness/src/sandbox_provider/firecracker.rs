//! Linux Firecracker sandbox backend.
//!
//! Security-sensitive choices follow Firecracker's upstream production guidance:
//! https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, Permissions};
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::Ipv4Addr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput, SandboxKey,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotPayload, sandbox_spec_hash,
};
use crate::sandbox_provider::process_bridge;
use crate::{FileSystemMountMode, SandboxAttachment, SandboxProcessParts};

use super::firecracker_image::resolve_image;
#[cfg(test)]
use super::firecracker_image::validate_ext4_image;

const API_READY_TIMEOUT: Duration = Duration::from_secs(10);
const GUEST_READY_TIMEOUT: Duration = Duration::from_secs(30);
const GUEST_READY_PROBE_TIMEOUT: Duration = Duration::from_millis(100);
const GUEST_READY_POLL_INTERVAL: Duration = Duration::from_millis(20);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const GUEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(40);
const MAX_GUEST_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_GUEST_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_MACHINE_ID: &str = "fc-0000000000000000-00000000";
// sockaddr_un.sun_path is 108 bytes including the trailing NUL on Linux.
// https://github.com/torvalds/linux/blob/master/include/uapi/linux/un.h
const UNIX_SOCKET_PATH_CAPACITY: usize = 108;
// The guest agent drops to UID 10001 before binding, so keep its AF_VSOCK port
// above Linux's privileged range. The kernel enforces that in vsock_bind().
// https://github.com/torvalds/linux/blob/master/net/vmw_vsock/af_vsock.c
const GUEST_AGENT_PORT: u32 = 10_052;
const MAX_RESOURCE_SLOTS: u32 = 32_768;
const NETWORK_BASE: Ipv4Addr = Ipv4Addr::new(10, 240, 0, 0);
const EXO_NETWORK_CIDR: &str = "10.240.0.0/14";
const DEFAULT_WORKSPACE_SIZE_GIB: u64 = 20;
const DEFAULT_IMAGE_SIZE_GIB: u64 = 8;
const DEFAULT_NETWORK_BYTES_PER_SECOND: u64 = 100 * 1024 * 1024;
const DEFAULT_JAILER_UID_BASE: u32 = 100_000;
const DEFAULT_VCPU_COUNT: u8 = 2;
const DEFAULT_MEMORY_MIB: u32 = 4096;
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
static MANIFEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static LEASE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn default_firecracker_image() -> String {
    "/var/lib/exo/firecracker/rootfs.ext4".to_string()
}

#[derive(Debug, Clone)]
pub struct FirecrackerConfig {
    pub firecracker_bin: PathBuf,
    pub jailer_bin: PathBuf,
    pub kernel: PathBuf,
    pub guest_runtime: PathBuf,
    pub state_root: PathBuf,
    pub vcpu_count: u8,
    pub memory_mib: u32,
    pub image_size_gib: u64,
    pub workspace_size_gib: u64,
    pub jailer_uid_base: u32,
    pub dns_server: Ipv4Addr,
    pub allowed_egress_cidrs: Vec<String>,
    pub network_bytes_per_second: u64,
}

impl FirecrackerConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            firecracker_bin: env_path("EXO_FIRECRACKER_BINARY", "/usr/local/bin/firecracker"),
            jailer_bin: env_path("EXO_FIRECRACKER_JAILER", "/usr/local/bin/jailer"),
            kernel: env_path("EXO_FIRECRACKER_KERNEL", "/var/lib/exo/firecracker/vmlinux"),
            guest_runtime: env_path(
                "EXO_FIRECRACKER_GUEST_RUNTIME",
                "/var/lib/exo/firecracker/exo-firecracker-guest",
            ),
            state_root: env_path(
                "EXO_FIRECRACKER_STATE_ROOT",
                "/var/lib/exo/firecracker/state",
            ),
            vcpu_count: env_parse("EXO_FIRECRACKER_VCPU_COUNT", DEFAULT_VCPU_COUNT)?,
            memory_mib: env_parse("EXO_FIRECRACKER_MEMORY_MIB", DEFAULT_MEMORY_MIB)?,
            image_size_gib: env_parse("EXO_FIRECRACKER_IMAGE_SIZE_GIB", DEFAULT_IMAGE_SIZE_GIB)?,
            workspace_size_gib: env_parse(
                "EXO_FIRECRACKER_WORKSPACE_SIZE_GIB",
                DEFAULT_WORKSPACE_SIZE_GIB,
            )?,
            jailer_uid_base: env_parse("EXO_FIRECRACKER_JAILER_UID_BASE", DEFAULT_JAILER_UID_BASE)?,
            dns_server: env_parse("EXO_FIRECRACKER_DNS_SERVER", Ipv4Addr::new(1, 1, 1, 1))?,
            allowed_egress_cidrs: env_cidrs("EXO_FIRECRACKER_ALLOWED_EGRESS_CIDRS")?,
            network_bytes_per_second: env_parse(
                "EXO_FIRECRACKER_NETWORK_BYTES_PER_SECOND",
                DEFAULT_NETWORK_BYTES_PER_SECOND,
            )?,
        })
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
struct MachineRecord {
    machine_id: String,
    spec_hash: String,
    slot: u32,
    network_enabled: bool,
    workspace_id: Option<String>,
    // The lease mtime is refreshed on use; keeping the TTL in the immutable
    // manifest lets a later CLI process reap a VM without process-local state.
    idle_ttl_seconds: Option<u64>,
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
    warm_machines: Mutex<HashMap<SandboxKey, WarmMachineEntry>>,
    lifecycle_lock: Mutex<()>,
}

pub struct FirecrackerSandboxBackend {
    shared: Arc<Shared>,
}

impl FirecrackerSandboxBackend {
    pub fn new(mut config: FirecrackerConfig) -> Result<Self> {
        validate_host(&config)?;
        fs::create_dir_all(&config.state_root).with_context(|| {
            format!(
                "creating Firecracker state root {}",
                config.state_root.display()
            )
        })?;
        fs::set_permissions(&config.state_root, Permissions::from_mode(0o700))?;
        for directory in ["jailer", "leases", "manifests", "slots", "workspaces"] {
            let path = config.state_root.join(directory);
            fs::create_dir_all(&path)?;
            fs::set_permissions(path, Permissions::from_mode(0o700))?;
        }
        config.firecracker_bin = fs::canonicalize(&config.firecracker_bin)?;
        config.jailer_bin = fs::canonicalize(&config.jailer_bin)?;
        config.kernel = fs::canonicalize(&config.kernel)?;
        config.guest_runtime = fs::canonicalize(&config.guest_runtime)?;
        config.state_root = fs::canonicalize(&config.state_root)?;
        validate_private_root(&config.state_root)?;
        validate_api_socket_path(&config)?;

        Ok(Self {
            shared: Arc::new(Shared {
                config,
                warm_machines: Mutex::new(HashMap::new()),
                lifecycle_lock: Mutex::new(()),
            }),
        })
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
}

#[async_trait]
impl ManagedSandboxBackend for FirecrackerSandboxBackend {
    fn is_local(&self) -> bool {
        true
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let mut request = prepare_request(request)?;
        let image = resolve_image(
            &self.shared.config.state_root,
            &request.spec.image,
            self.shared.config.image_size_gib,
            &self.shared.config.guest_runtime,
        )
        .await?;
        request.spec.image = image.to_string_lossy().into_owned();
        let _lifecycle_guard = self.shared.lifecycle_lock.lock().await;
        self.reap_expired_machines().await?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        let stable_machine_id = machine_id(&request.key, &spec_hash);
        let machine_key_prefix = format!("fc-{}-", stable_fnv1a_hex(&request.key.to_string()));

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

        if request.lifecycle.idle_ttl.is_none() {
            let sequence = ONE_SHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let machine_id = one_shot_machine_id(&request.key, &spec_hash, sequence);
            let machine = self
                .shared
                .ensure_machine(&request, &machine_id, &spec_hash)
                .await?;
            return Ok(Arc::new(FirecrackerSandboxHandle {
                id: format!("firecracker-oneshot:{machine_id}"),
                machine,
                request,
                spec_hash,
                shared: Arc::clone(&self.shared),
                one_shot: true,
            }));
        }

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
        let machine = self
            .shared
            .ensure_machine(&request, &stable_machine_id, &spec_hash)
            .await?;
        self.shared.touch_machine_lease(&stable_machine_id).await?;
        self.shared.warm_machines.lock().await.insert(
            request.key.clone(),
            WarmMachineEntry {
                machine_id: stable_machine_id.clone(),
                spec_hash: spec_hash.clone(),
                idle_ttl: request.lifecycle.idle_ttl,
                last_used_at: Instant::now(),
            },
        );
        Ok(Arc::new(FirecrackerSandboxHandle {
            id: format!("firecracker:{stable_machine_id}"),
            machine,
            request,
            spec_hash,
            shared: Arc::clone(&self.shared),
            one_shot: false,
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Firecracker sandboxes do not support external attachments")
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Firecracker sandboxes do not support restoring Exo snapshots")
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
        let machine = self
            .shared
            .ensure_machine(
                &self.request,
                &self.machine.record.machine_id,
                &self.spec_hash,
            )
            .await?;
        touch_machine(
            &self.shared,
            &self.shared.warm_machines,
            &self.request.key,
            &machine.record.machine_id,
        )
        .await?;
        let output = GuestClient::new(Arc::clone(&self.shared), machine.vsock_path)
            .exec(&self.request.spec, command)
            .await;
        if self.one_shot {
            let cleanup = self
                .shared
                .cleanup_machine(&machine.record.machine_id, true)
                .await;
            return match (output, cleanup) {
                (Ok(output), Ok(())) => Ok(output),
                (Ok(_), Err(error)) | (Err(error), _) => Err(error),
            };
        }
        touch_machine(
            &self.shared,
            &self.shared.warm_machines,
            &self.request.key,
            &machine.record.machine_id,
        )
        .await?;
        output
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        let machine = self
            .shared
            .ensure_machine(
                &self.request,
                &self.machine.record.machine_id,
                &self.spec_hash,
            )
            .await?;
        touch_machine(
            &self.shared,
            &self.shared.warm_machines,
            &self.request.key,
            &machine.record.machine_id,
        )
        .await?;
        let cleanup_machine_id = self.one_shot.then(|| machine.record.machine_id.clone());
        GuestClient::new(Arc::clone(&self.shared), machine.vsock_path)
            .start_process(&self.request.spec, command, cleanup_machine_id)
            .await
    }

    async fn stop(&self) -> Result<()> {
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
        bail!("Firecracker sandbox snapshots are not implemented")
    }
}

impl Shared {
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
                self.stop_machine_process(machine_id).await?;
                if record.network_enabled {
                    self.cleanup_network(&record.network()).await;
                }
            }
        }

        let launch_started = Instant::now();
        let machine_record_started = Instant::now();
        let record = match existing {
            Some(record) if record.spec_hash == spec_hash => record,
            _ => {
                self.new_machine_record(request, machine_id, spec_hash)
                    .await?
            }
        };
        record_launch_timing(
            machine_id,
            "machine_record",
            machine_record_started.elapsed(),
        );
        let host_started = Instant::now();
        if let Err(error) = self.prepare_and_launch(request, &record).await {
            if let Err(cleanup_error) = self.cleanup_machine(machine_id, true).await {
                tracing::warn!(%cleanup_error, machine_id, "failed cleaning up unsuccessful Firecracker launch");
            }
            return Err(error);
        }
        record_launch_timing(machine_id, "host_prepare_and_start", host_started.elapsed());
        let machine = machine_from_record(&self.config, record);
        let client = GuestClient::new(Arc::clone(self), machine.vsock_path.clone());
        let guest_ready_started = Instant::now();
        wait_for_guest(self, &client, machine_id).await?;
        record_launch_timing(machine_id, "guest_ready", guest_ready_started.elapsed());
        record_launch_timing(machine_id, "total", launch_started.elapsed());
        Ok(machine)
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
    ) -> Result<MachineRecord> {
        let state_root = self.config.state_root.clone();
        let machine_id = machine_id.to_string();
        let spec_hash = spec_hash.to_string();
        let network_enabled = request.spec.network == SandboxNetworkPolicy::Enabled;
        let workspace_id = request
            .spec
            .durable_file_systems
            .first()
            .map(|file_system| stable_fnv1a_hex(&format!("{}\n{}", request.key, file_system.name)));
        let idle_ttl_seconds = request.lifecycle.idle_ttl.map(|ttl| ttl.as_secs());
        tokio::task::spawn_blocking(move || {
            let slot = allocate_resource_slot(&state_root, &machine_id)?;
            let record = MachineRecord {
                machine_id,
                spec_hash,
                slot,
                network_enabled,
                workspace_id,
                idle_ttl_seconds,
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
    ) -> Result<()> {
        let config = self.config.clone();
        let request = request.clone();
        let record = record.clone();
        tokio::task::spawn_blocking(move || {
            let network = record.network();
            let result = (|| {
                if record.network_enabled {
                    let started = Instant::now();
                    prepare_network(&config, &network, jailer_uid(&config, &record)?)?;
                    record_launch_timing(&record.machine_id, "network_setup", started.elapsed());
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
        if let Some(record) = record.as_ref().filter(|record| record.network_enabled) {
            self.cleanup_network(&record.network()).await;
        }
        if delete_rootfs {
            let jail_dir = self.jail_dir(machine_id);
            let manifest = self.manifest_path(machine_id);
            let lease = lease_path(&self.config.state_root, machine_id);
            let cgroup_dir = firecracker_cgroup_dir(&self.config, machine_id);
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
                if cgroup_dir.try_exists()? {
                    fs::remove_dir(&cgroup_dir).with_context(|| {
                        format!("removing Firecracker cgroup {}", cgroup_dir.display())
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

fn prepare_request(request: SandboxRequest) -> Result<SandboxRequest> {
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

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn env_parse<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value
            .parse()
            .map_err(|error| anyhow!("invalid {name} value {value:?}: {error}")),
        _ => Ok(default),
    }
}

fn env_cidrs(name: &str) -> Result<Vec<String>> {
    let Some(value) = std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(Vec::new());
    };
    value
        .split(',')
        .map(|cidr| {
            let cidr = cidr.trim();
            validate_ipv4_cidr(cidr)?;
            Ok(cidr.to_string())
        })
        .collect()
}

fn validate_ipv4_cidr(cidr: &str) -> Result<()> {
    let (address, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow!("IPv4 CIDR must contain a prefix: {cidr}"))?;
    address
        .parse::<Ipv4Addr>()
        .with_context(|| format!("invalid IPv4 address in CIDR {cidr}"))?;
    let prefix = prefix
        .parse::<u8>()
        .with_context(|| format!("invalid IPv4 prefix in CIDR {cidr}"))?;
    if prefix > 32 {
        bail!("invalid IPv4 prefix in CIDR {cidr}");
    }
    Ok(())
}

fn validate_host(config: &FirecrackerConfig) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("Firecracker sandbox execution is only supported on Linux");
    }
    if fs::metadata("/proc/self")?.uid() != 0 {
        bail!("Firecracker sandbox execution must run as root so jailer can set up isolation");
    }
    validate_trusted_file("Firecracker binary", &config.firecracker_bin)?;
    validate_trusted_file("Firecracker jailer", &config.jailer_bin)?;
    validate_trusted_file("Firecracker guest kernel", &config.kernel)?;
    validate_trusted_file("Firecracker guest runtime", &config.guest_runtime)?;
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
    for program in [
        "chown",
        "cp",
        "ip",
        "iptables",
        "kill",
        "mkfs.ext4",
        "nft",
        "sysctl",
    ] {
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
    if config.workspace_size_gib == 0 {
        bail!("Firecracker workspace size must be positive");
    }
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
    for cidr in &config.allowed_egress_cidrs {
        validate_ipv4_cidr(cidr)?;
    }
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

fn validate_api_socket_path(config: &FirecrackerConfig) -> Result<()> {
    let path = jail_root(config, MAX_MACHINE_ID).join("run/firecracker.socket");
    if path.as_os_str().as_bytes().len() >= UNIX_SOCKET_PATH_CAPACITY {
        bail!(
            "Firecracker state root is too long for the API Unix socket path: {}",
            config.state_root.display()
        );
    }
    Ok(())
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
    format!(
        "fc-{}-{}",
        stable_fnv1a_hex(&key.to_string()),
        &spec_hash[..8]
    )
}

fn one_shot_machine_id(key: &SandboxKey, spec_hash: &str, sequence: u64) -> String {
    format!(
        "fc-{}-{}",
        stable_fnv1a_hex(&format!("{key}\n{}\n{sequence}", std::process::id())),
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

fn stable_fnv1a_hex(input: &str) -> String {
    format!("{:016x}", stable_fnv1a(input))
}

fn stable_fnv1a(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn machine_from_record(config: &FirecrackerConfig, record: MachineRecord) -> Machine {
    let vsock_path = jail_root(config, &record.machine_id).join("run/exo.vsock");
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
    let sequence = LEASE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
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

fn write_manifest(state_root: &Path, record: &MachineRecord) -> Result<()> {
    let path = manifest_path(state_root, &record.machine_id);
    let sequence = MANIFEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
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
    let first = (stable_fnv1a(machine_id) % u64::from(MAX_RESOURCE_SLOTS)) as u32;
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

    run_checked("nft", &["add", "table", "inet", &network.nft_table])?;
    run_checked(
        "nft",
        &[
            "add",
            "chain",
            "inet",
            &network.nft_table,
            "input",
            "{ type filter hook input priority filter; policy accept; }",
        ],
    )?;
    run_checked(
        "nft",
        &[
            "add",
            "chain",
            "inet",
            &network.nft_table,
            "forward",
            "{ type filter hook forward priority filter; policy accept; }",
        ],
    )?;
    run_checked(
        "nft",
        &[
            "add",
            "chain",
            "inet",
            &network.nft_table,
            "postrouting",
            "{ type nat hook postrouting priority srcnat; policy accept; }",
        ],
    )?;
    // Permit only replies to host-initiated connections before rejecting all
    // unsolicited guest-to-host traffic. This lets host controllers reach a
    // selected guest service without exposing host listeners to the guest.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md#filtering-guest-egress-network-traffic
    run_checked(
        "nft",
        &[
            "add",
            "rule",
            "inet",
            &network.nft_table,
            "input",
            "iifname",
            &network.host_veth,
            "ct",
            "state",
            "established,related",
            "counter",
            "accept",
        ],
    )?;
    run_checked(
        "nft",
        &[
            "add",
            "rule",
            "inet",
            &network.nft_table,
            "input",
            "iifname",
            &network.host_veth,
            "counter",
            "drop",
        ],
    )?;
    run_checked(
        "nft",
        &[
            "add",
            "rule",
            "inet",
            &network.nft_table,
            "forward",
            "iifname",
            &network.host_veth,
            "ip",
            "saddr",
            "!=",
            &network.guest_cidr,
            "counter",
            "drop",
        ],
    )?;
    // Never let an explicit private-CIDR exception admit another Exo VM. Each
    // VM receives two /30s from this range, so this rule preserves tenant
    // separation independently of the configurable destination allowlist.
    run_checked(
        "nft",
        &[
            "add",
            "rule",
            "inet",
            &network.nft_table,
            "forward",
            "iifname",
            &network.host_veth,
            "ip",
            "daddr",
            EXO_NETWORK_CIDR,
            "counter",
            "reject",
        ],
    )?;
    for cidr in &config.allowed_egress_cidrs {
        run_checked(
            "nft",
            &[
                "add",
                "rule",
                "inet",
                &network.nft_table,
                "forward",
                "iifname",
                &network.host_veth,
                "ip",
                "daddr",
                cidr,
                "counter",
                "accept",
            ],
        )?;
    }
    let blocked_cidrs = format!("{{ {} }}", BLOCKED_EGRESS_CIDRS.join(", "));
    run_checked(
        "nft",
        &[
            "add",
            "rule",
            "inet",
            &network.nft_table,
            "forward",
            "iifname",
            &network.host_veth,
            "ip",
            "daddr",
            &blocked_cidrs,
            "counter",
            "reject",
        ],
    )?;
    run_checked(
        "nft",
        &[
            "add",
            "rule",
            "inet",
            &network.nft_table,
            "forward",
            "iifname",
            &network.host_veth,
            "counter",
            "accept",
        ],
    )?;
    run_checked(
        "nft",
        &[
            "add",
            "rule",
            "inet",
            &network.nft_table,
            "forward",
            "oifname",
            &network.host_veth,
            "ct",
            "state",
            "established,related",
            "counter",
            "accept",
        ],
    )?;
    run_checked(
        "nft",
        &[
            "add",
            "rule",
            "inet",
            &network.nft_table,
            "forward",
            "oifname",
            &network.host_veth,
            "counter",
            "drop",
        ],
    )?;
    run_checked(
        "nft",
        &[
            "add",
            "rule",
            "inet",
            &network.nft_table,
            "postrouting",
            "ip",
            "saddr",
            &network.guest_cidr,
            "counter",
            "masquerade",
        ],
    )?;
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

fn prepare_and_launch_blocking(
    config: &FirecrackerConfig,
    request: &SandboxRequest,
    record: &MachineRecord,
) -> Result<()> {
    let rootfs_copy_started = Instant::now();
    let root = jail_root(config, &record.machine_id);
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, Permissions::from_mode(0o700))?;
    let rootfs = root.join("rootfs.ext4");
    let copied_rootfs = !rootfs.try_exists()?;
    if copied_rootfs {
        run_checked(
            "cp",
            &[
                "--reflink=auto",
                "--sparse=always",
                &request.spec.image,
                &rootfs.to_string_lossy(),
            ],
        )?;
    }
    let rootfs_metadata = fs::metadata(&rootfs)
        .with_context(|| format!("reading Firecracker rootfs metadata {}", rootfs.display()))?;
    tracing::info!(
        machine_id = record.machine_id,
        step = "rootfs_copy",
        duration_ms = rootfs_copy_started.elapsed().as_secs_f64() * 1000.0,
        copied_rootfs,
        rootfs_logical_bytes = rootfs_metadata.len(),
        rootfs_allocated_bytes = rootfs_metadata.blocks().saturating_mul(512),
        "Firecracker VM launch timing"
    );

    let jail_setup_started = Instant::now();
    let kernel = root.join("vmlinux");
    fs::copy(&config.kernel, &kernel).with_context(|| {
        format!(
            "copying Firecracker kernel {} to {}",
            config.kernel.display(),
            kernel.display()
        )
    })?;
    let host_uid = jailer_uid(config, record)?;
    let ownership = format!("{host_uid}:{host_uid}");
    run_checked(
        "chown",
        &[
            &ownership,
            &rootfs.to_string_lossy(),
            &kernel.to_string_lossy(),
        ],
    )?;
    fs::set_permissions(&kernel, Permissions::from_mode(0o400))?;
    fs::set_permissions(&rootfs, Permissions::from_mode(0o600))?;

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
            file.set_len(config.workspace_size_gib * 1024 * 1024 * 1024)?;
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
        run_checked("chown", &[&ownership, &jailed_workspace.to_string_lossy()])?;
    }

    let stderr_path = root.join("firecracker.stderr");
    let stderr = File::create(&stderr_path)?;
    fs::set_permissions(&stderr_path, Permissions::from_mode(0o600))?;
    let memory_max = u64::from(
        config
            .memory_mib
            .checked_add(256)
            .context("Firecracker cgroup memory limit overflow")?,
    ) * 1024
        * 1024;
    let cpu_max = format!("{} 100000", u32::from(config.vcpu_count) * 100_000);
    record_launch_timing(
        &record.machine_id,
        "jail_setup",
        jail_setup_started.elapsed(),
    );
    let vmm_api_ready_started = Instant::now();
    // Always use the matching jailer: it creates the mount/PID namespaces and
    // cgroup, then drops to a unique unprivileged UID before execing Firecracker.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md#jailer-operation
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md#jailer-configuration
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
        .arg(format!("cpu.max={cpu_max}"))
        .arg("--resource-limit")
        .arg("no-file=4096")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
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

    let api_socket = root.join("run/firecracker.socket");
    wait_for_api_socket(&api_socket, &root.join("firecracker.pid"), &stderr_path)?;
    record_launch_timing(
        &record.machine_id,
        "vmm_api_ready",
        vmm_api_ready_started.elapsed(),
    );
    let vmm_configure_started = Instant::now();
    firecracker_put(
        &api_socket,
        "/machine-config",
        &json!({
            "vcpu_count": config.vcpu_count,
            "mem_size_mib": config.memory_mib,
            "smt": false,
        }),
    )?;
    let network = record.network();
    // Disable the guest serial driver and discard VMM stdout so malicious guest
    // writes cannot grow an unbounded host log.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md#8250-serial-device
    let mut boot_args = String::from(
        "reboot=k panic=1 pci=off root=/dev/vda rw init=/runtime/exo-firecracker-guest 8250.nr_uarts=0 quiet loglevel=1",
    );
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
    firecracker_put(
        &api_socket,
        "/boot-source",
        &json!({
            "kernel_image_path": "/vmlinux",
            "boot_args": boot_args,
        }),
    )?;
    firecracker_put(
        &api_socket,
        "/drives/rootfs",
        &json!({
            "drive_id": "rootfs",
            "path_on_host": "/rootfs.ext4",
            "is_root_device": true,
            "is_read_only": false,
        }),
    )?;
    if record.workspace_id.is_some() {
        // Writeback advertises virtio-blk FLUSH to the guest and turns a guest
        // flush into fsync(2) on the backing file. Combined with the explicit
        // guest sync during stop, this makes the workspace a durability boundary.
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/api_requests/block-caching.md#writeback-mode
        firecracker_put(
            &api_socket,
            "/drives/workspace",
            &json!({
                "drive_id": "workspace",
                "path_on_host": "/workspace.ext4",
                "is_root_device": false,
                "is_read_only": false,
                "cache_type": "Writeback",
            }),
        )?;
    }
    // The control channel is vsock rather than TCP: networking-disabled sandboxes
    // still support exec, and the guest agent is never reachable through egress.
    // https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md#setting-up-the-virtio-vsock-device
    firecracker_put(
        &api_socket,
        "/vsock",
        &json!({
            "guest_cid": record.slot + 3,
            "uds_path": "/run/exo.vsock",
        }),
    )?;
    if record.network_enabled {
        let bucket = json!({
            "bandwidth": {
                "size": config.network_bytes_per_second,
                "refill_time": 1000,
            }
        });
        firecracker_put(
            &api_socket,
            "/network-interfaces/eth0",
            &json!({
                "iface_id": "eth0",
                "guest_mac": network.guest_mac,
                "host_dev_name": "tap0",
                "rx_rate_limiter": bucket,
                "tx_rate_limiter": bucket,
            }),
        )?;
    }
    record_launch_timing(
        &record.machine_id,
        "vmm_configure",
        vmm_configure_started.elapsed(),
    );
    let instance_start_started = Instant::now();
    firecracker_put(
        &api_socket,
        "/actions",
        &json!({"action_type": "InstanceStart"}),
    )?;
    record_launch_timing(
        &record.machine_id,
        "instance_start_api",
        instance_start_started.elapsed(),
    );
    Ok(())
}

fn jailer_uid(config: &FirecrackerConfig, record: &MachineRecord) -> Result<u32> {
    config
        .jailer_uid_base
        .checked_add(record.slot)
        .context("Firecracker jailer UID overflow")
}

fn wait_for_api_socket(socket: &Path, pid_path: &Path, stderr_path: &Path) -> Result<()> {
    let started = Instant::now();
    let mut delay = Duration::from_millis(25);
    while started.elapsed() < API_READY_TIMEOUT {
        if socket.exists() {
            return Ok(());
        }
        if pid_path.exists() && !process_running(pid_path) {
            bail!(
                "Firecracker exited before its API became ready: {}",
                tail_file(stderr_path)?
            );
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(250));
    }
    bail!(
        "Firecracker API socket did not become ready: {}",
        tail_file(stderr_path)?
    )
}

fn firecracker_put(socket: &Path, path: &str, body: &Value) -> Result<()> {
    let payload = serde_json::to_vec(body)?;
    let mut stream = StdUnixStream::connect(socket)
        .with_context(|| format!("connecting to Firecracker API socket {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "PUT {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    )?;
    stream.write_all(&payload)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .with_context(|| format!("invalid Firecracker API status line: {status_line:?}"))?;
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some(value) = header
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim())
        {
            content_length = value
                .parse()
                .context("invalid Firecracker content-length")?;
        }
    }
    if content_length > MAX_GUEST_RESPONSE_BYTES {
        bail!("Firecracker API response is too large: {content_length} bytes");
    }
    let mut response = vec![0; content_length];
    reader.read_exact(&mut response)?;
    if (200..300).contains(&status) {
        return Ok(());
    }
    bail!(
        "Firecracker API {path} returned {status}: {}",
        String::from_utf8_lossy(&response)
    )
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

fn stop_machine_process_blocking(machine_id: &str, pid_path: &Path) -> Result<()> {
    let Ok(pid) = fs::read_to_string(pid_path) else {
        return Ok(());
    };
    let pid = pid
        .trim()
        .parse::<u32>()
        .context("invalid Firecracker pid")?;
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
    run_checked("kill", &["-TERM", &pid.to_string()])?;
    let started = Instant::now();
    while started.elapsed() < PROCESS_STOP_TIMEOUT {
        if !proc_dir.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    run_checked("kill", &["-KILL", &pid.to_string()])?;
    Ok(())
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
        let response: OperationResponse = self
            .invoke_with_timeout(
                &PingRequest {
                    request_type: "ping",
                },
                timeout,
            )
            .await?;
        if response.ok {
            return Ok(());
        }
        bail!(
            "Firecracker guest healthcheck failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        )
    }

    async fn invoke<Request, Response>(&self, request: &Request) -> Result<Response>
    where
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        self.invoke_with_timeout(request, GUEST_REQUEST_TIMEOUT)
            .await
    }

    async fn invoke_with_timeout<Request, Response>(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<Response>
    where
        Request: Serialize + ?Sized,
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
        let response: ExecResponse = self
            .invoke_with_timeout(
                &ExecRequest {
                    request_type: "exec",
                    argv: &command.argv,
                    env: &command.env,
                    cwd: &cwd,
                    timeout_ms: command.timeout.map(duration_to_millis),
                },
                request_timeout,
            )
            .await?;
        let ok = response
            .ok
            .unwrap_or_else(|| response.exit_code.is_some_and(|code| code == 0));
        let mut stderr = response.stderr.unwrap_or_default();
        if let Some(error) = response.error {
            if !stderr.is_empty() {
                stderr.push('\n');
            }
            stderr.push_str(&error);
        }
        Ok(SandboxCommandOutput {
            ok,
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
        let response: StartProcessResponse = self
            .invoke(&StartProcessRequest {
                request_type: "start_process",
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
        let response: OperationResponse = self
            .invoke(&KillProcessRequest {
                request_type: "kill_process",
                process_id,
            })
            .await?;
        if response.ok {
            return Ok(());
        }
        bail!(
            "Firecracker kill_process failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        )
    }

    async fn sync_filesystem(&self, path: &str) -> Result<()> {
        let response: OperationResponse = self
            .invoke_with_timeout(
                &SyncFilesystemRequest {
                    request_type: "sync_filesystem",
                    path,
                },
                GUEST_REQUEST_TIMEOUT,
            )
            .await?;
        if response.ok {
            return Ok(());
        }
        bail!(
            "Firecracker guest filesystem sync failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        )
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
    stream.read_until(b'\n', &mut acknowledgement).await?;
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
struct PingRequest {
    #[serde(rename = "type")]
    request_type: &'static str,
}

#[derive(Serialize)]
struct ExecRequest<'a> {
    #[serde(rename = "type")]
    request_type: &'static str,
    argv: &'a [String],
    env: &'a HashMap<String, String>,
    cwd: &'a str,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct ExecResponse {
    ok: Option<bool>,
    exit_code: Option<i32>,
    stdout: Option<String>,
    stderr: Option<String>,
    cwd: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct StartProcessRequest<'a> {
    #[serde(rename = "type")]
    request_type: &'static str,
    argv: &'a [String],
    env: &'a HashMap<String, String>,
    cwd: &'a str,
}

#[derive(Deserialize)]
struct StartProcessResponse {
    process_id: Option<String>,
    error: Option<String>,
}

#[derive(Serialize)]
struct ProcessBridgeRequest<'a> {
    #[serde(rename = "type")]
    request_type: &'static str,
    process_id: &'a str,
    request: process_bridge::Request,
}

#[derive(Serialize)]
struct KillProcessRequest<'a> {
    #[serde(rename = "type")]
    request_type: &'static str,
    process_id: &'a str,
}

#[derive(Serialize)]
struct SyncFilesystemRequest<'a> {
    #[serde(rename = "type")]
    request_type: &'static str,
    path: &'a str,
}

#[derive(Deserialize)]
struct OperationResponse {
    ok: bool,
    error: Option<String>,
}

struct FirecrackerProcessBridgeClient {
    guest: GuestClient,
    process_id: String,
}

#[async_trait]
impl process_bridge::Client for FirecrackerProcessBridgeClient {
    async fn request(&self, request: process_bridge::Request) -> Result<process_bridge::Response> {
        self.guest
            .invoke(&ProcessBridgeRequest {
                request_type: "process_bridge",
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
                && let Err(error) = guest.shared.cleanup_machine(&machine_id, true).await
            {
                tracing::warn!(%error, machine_id, "failed to clean up one-shot Firecracker machine");
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

fn duration_to_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn record_launch_timing(machine_id: &str, step: &str, duration: Duration) {
    tracing::info!(
        machine_id,
        step,
        duration_ms = duration.as_secs_f64() * 1000.0,
        "Firecracker VM launch timing"
    );
}

async fn wait_for_guest(shared: &Shared, client: &GuestClient, machine_id: &str) -> Result<()> {
    let started = Instant::now();
    let mut last_error = None;
    let mut attempts = 0u32;
    let mut failed_ping_duration = Duration::ZERO;
    let mut sleep_duration = Duration::ZERO;
    while started.elapsed() < GUEST_READY_TIMEOUT {
        if !process_running(&shared.pid_path(machine_id)) {
            bail!("Firecracker process exited while waiting for the guest agent");
        }
        attempts += 1;
        let ping_started = Instant::now();
        match client.ping_with_timeout(GUEST_READY_PROBE_TIMEOUT).await {
            Ok(()) => {
                tracing::info!(
                    machine_id,
                    step = "guest_ready_detail",
                    duration_ms = started.elapsed().as_secs_f64() * 1000.0,
                    attempts,
                    failed_ping_duration_ms = failed_ping_duration.as_secs_f64() * 1000.0,
                    successful_ping_duration_ms = ping_started.elapsed().as_secs_f64() * 1000.0,
                    sleep_duration_ms = sleep_duration.as_secs_f64() * 1000.0,
                    "Firecracker VM launch timing"
                );
                return Ok(());
            }
            Err(error) => {
                failed_ping_duration += ping_started.elapsed();
                last_error = Some(error);
            }
        }
        let sleep_started = Instant::now();
        tokio::time::sleep(GUEST_READY_POLL_INTERVAL).await;
        sleep_duration += sleep_started.elapsed();
    }
    let detail = last_error
        .map(|error| format!("{error:#}"))
        .unwrap_or_else(|| "no response".to_string());
    bail!(
        "Firecracker guest agent did not become ready: {detail}; {}",
        tail_file(&jail_root(&shared.config, machine_id).join("firecracker.stderr"))?
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
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[test]
    fn resource_names_and_addresses_are_distinct() {
        let first = network_config(1);
        let second = network_config(2);
        assert_ne!(first.namespace, second.namespace);
        assert_ne!(first.host_veth, second.host_veth);
        assert_ne!(first.nft_table, second.nft_table);
        assert_ne!(first.guest_ip, second.guest_ip);
        assert_ne!(first.guest_cidr, second.guest_cidr);
    }

    #[test]
    fn validates_machine_ids_and_cidrs() {
        assert!(valid_machine_id("fc-0123456789abcdef-01234567"));
        assert!(!valid_machine_id("../firecracker"));
        let one_shot = one_shot_machine_id(
            &SandboxKey::AgentSandbox {
                agent_id: "agent".to_string(),
                sandbox_id: "sandbox".to_string(),
            },
            "0123456789abcdef",
            u64::MAX,
        );
        assert!(valid_machine_id(&one_shot));
        assert_eq!(one_shot.len(), MAX_MACHINE_ID.len());
        assert!(validate_ipv4_cidr("203.0.113.0/24").is_ok());
        assert!(validate_ipv4_cidr("203.0.113.0/33").is_err());
        assert!(validate_ipv4_cidr("example.com/24").is_err());
    }

    #[test]
    fn validates_ext4_magic() {
        let directory = tempfile::tempdir().unwrap();
        let image_path = directory.path().join("rootfs.ext4");
        let mut image = File::create(&image_path).unwrap();
        image.set_len(2048).unwrap();
        image.seek(SeekFrom::Start(1024 + 0x38)).unwrap();
        image.write_all(&[0x53, 0xef]).unwrap();
        image.flush().unwrap();
        assert!(validate_ext4_image(&image_path).is_ok());

        image.seek(SeekFrom::Start(1024 + 0x38)).unwrap();
        image.write_all(&[0, 0]).unwrap();
        image.flush().unwrap();
        assert!(validate_ext4_image(&image_path).is_err());
    }

    #[test]
    fn resource_slot_claims_are_atomic_and_owner_checked() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("slots")).unwrap();
        let first = allocate_resource_slot(directory.path(), "fc-first").unwrap();
        let second = allocate_resource_slot(directory.path(), "fc-second").unwrap();
        assert_ne!(first, second);
        assert!(release_resource_slot(directory.path(), first, "fc-second").is_err());
        release_resource_slot(directory.path(), first, "fc-first").unwrap();
        assert!(!resource_slot_path(directory.path(), first).exists());
    }

    #[test]
    fn manifest_publish_does_not_replace_an_existing_machine() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("manifests")).unwrap();
        let first = MachineRecord {
            machine_id: "fc-machine".to_string(),
            spec_hash: "first".to_string(),
            slot: 1,
            network_enabled: false,
            workspace_id: None,
            idle_ttl_seconds: Some(60),
        };
        let second = MachineRecord {
            spec_hash: "second".to_string(),
            ..first.clone()
        };
        write_manifest(directory.path(), &first).unwrap();
        assert!(write_manifest(directory.path(), &second).is_err());
        let stored = serde_json::from_slice::<MachineRecord>(
            &fs::read(manifest_path(directory.path(), &first.machine_id)).unwrap(),
        )
        .unwrap();
        assert_eq!(stored.spec_hash, "first");
    }

    #[test]
    fn persisted_lease_expires_machine_after_idle_ttl() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("leases")).unwrap();
        fs::create_dir(directory.path().join("manifests")).unwrap();
        let record = MachineRecord {
            machine_id: "fc-machine".to_string(),
            spec_hash: "spec".to_string(),
            slot: 1,
            network_enabled: false,
            workspace_id: None,
            idle_ttl_seconds: Some(60),
        };
        write_manifest(directory.path(), &record).unwrap();
        touch_machine_lease(directory.path(), &record.machine_id).unwrap();
        let last_used = fs::metadata(lease_path(directory.path(), &record.machine_id))
            .unwrap()
            .modified()
            .unwrap();

        assert!(
            expired_machine_ids(directory.path(), last_used + Duration::from_secs(59))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            expired_machine_ids(directory.path(), last_used + Duration::from_secs(60)).unwrap(),
            vec![record.machine_id]
        );
    }

    #[tokio::test]
    async fn vsock_request_uses_firecracker_handshake_and_framing() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("exo.vsock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = AsyncBufReader::new(stream);
            let mut handshake = String::new();
            stream.read_line(&mut handshake).await.unwrap();
            assert_eq!(handshake, "CONNECT 10052\n");
            stream
                .get_mut()
                .write_all(b"OK 1073741824\n")
                .await
                .unwrap();
            stream.get_mut().flush().await.unwrap();
            let mut stream = stream.into_inner();
            let length = stream.read_u32().await.unwrap() as usize;
            let mut request = vec![0; length];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(request, br#"{"type":"ping"}"#);
            let response = br#"{"ok":true}"#;
            stream
                .write_all(&(response.len() as u32).to_be_bytes())
                .await
                .unwrap();
            stream.write_all(response).await.unwrap();
        });

        let response = vsock_request(&socket, br#"{"type":"ping"}"#).await.unwrap();
        assert_eq!(response, br#"{"ok":true}"#);
        server.await.unwrap();
    }
}

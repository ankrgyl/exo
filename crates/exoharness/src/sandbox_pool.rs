//! A single-process pool of interchangeable, disposable sandbox runtimes.
//!
//! Start one reconciler alongside callers:
//! ```ignore
//! let pool = Arc::new(SandboxPool::new(key, backend, capacity, SandboxPoolSeed::Empty)?);
//! let (shutdown, receiver) = tokio::sync::watch::channel(false);
//! let task = tokio::spawn({
//!     let pool = Arc::clone(&pool);
//!     async move { pool.run_reconciler(receiver).await }
//! });
//! let (lease, sandbox) = tokio::time::timeout(
//!     Duration::from_secs(150), pool.acquire_any("conversation:123"),
//! ).await??;
//! let output = sandbox.exec(&command).await?;
//! pool.heartbeat(&lease).await?;
//! // Save any files you need before release: release destroys the runtime.
//! pool.release(&lease).await?;
//! shutdown.send(true)?;
//! task.await?;
//! ```
//!
//! Shutdown stops replenishment and rejects new acquisitions; it does not destroy
//! outstanding leases or warm runtimes. Release owned leases before shutdown.
//! Dropped leases expire and are retired by reconciliation. This API does not
//! persist pool ownership across process restarts.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use tokio::sync::{Mutex, Notify, RwLock, Semaphore, watch};
use tokio::time::{self, MissedTickBehavior};

use crate::{
    ManagedSandboxBackend, ManagedSandboxHandle, Result, SandboxCommand, SandboxId,
    SandboxProvider, SandboxRequest, SandboxSpec, SnapshotPayload, Uuid7,
};

/// The immutable sandbox configuration shared by entries in one pool.
///
/// A pool can only hand out entries that are interchangeable for the request
/// represented by this key. Pool policy such as capacity and lease duration is
/// kept separately in [`PoolCapacity`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxPoolKey {
    pub pool_id: String,
    pub provider: SandboxProvider,
    pub spec: SandboxSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolCapacity {
    pub min_ready: usize,
    pub target_ready: usize,
    pub max_total: usize,
    pub lease_ttl: Duration,
    pub idle_ttl: Duration,
}

/// Filesystem state used when the pool creates a fresh runtime.
///
/// Mounts and durable file systems remain part of [`SandboxSpec`]. A snapshot
/// copies an immutable codebase or toolchain into each new pool entry.
#[derive(Debug, Clone)]
pub enum SandboxPoolSeed {
    Empty,
    Snapshot(SnapshotPayload),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolEntryState {
    /// The provider state has not been confirmed, usually after a manager
    /// restart or an ambiguous provider operation.
    Unknown,
    Creating,
    Checking,
    Ready,
    Leased,
    Evicting,
    Resetting,
    Unhealthy,
    Draining,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLease {
    pub id: String,
    pub entry_id: String,
    pub worker_id: String,
    pub fencing_token: String,
    pub issued_at: Instant,
    pub expires_at: Instant,
}

struct PoolEntry {
    pub id: String,
    pub request: SandboxRequest,
    pub sandbox_id: SandboxId,
    /// The live provider capability. This is absent after a manager restart or
    /// while an entry is being reattached/recreated from its durable record.
    pub handle: Option<Arc<dyn ManagedSandboxHandle>>,
    pub state: PoolEntryState,
    pub lease: Option<SandboxLease>,
    pub last_used_at: Instant,
    pub last_health_check_at: Option<Instant>,
    pub next_retry_at: Option<Instant>,
    pub failure_count: u32,
    /// The workspace is durable outside this runtime, so the runtime may be
    /// terminated and recreated when it becomes idle.
    pub evictable: bool,
    lifecycle: Arc<RwLock<()>>,
}

/// Owns the runtime lifecycle of managing multiple Sandboxes organized into a pool
///
///
pub struct SandboxPool {
    key: SandboxPoolKey,
    backend: Arc<dyn ManagedSandboxBackend>,
    seed: SandboxPoolSeed,
    entries: Arc<Mutex<HashMap<String, PoolEntry>>>,
    capacity: PoolCapacity,
    notify: Arc<Notify>,
    provider_operations: Arc<Semaphore>,
    changed: Arc<Notify>,
    reconcile: Mutex<()>,
    closed: AtomicBool,
}

const DEFAULT_PROVIDER_OPERATION_LIMIT: usize = 8;
/// Reconciler runs a tick based check every 10 seconds
const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(10);
const DEFAULT_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(30);

#[cfg(test)]
impl PoolEntry {
    fn new(
        id: String,
        request: SandboxRequest,
        sandbox_id: SandboxId,
        handle: Option<Arc<dyn ManagedSandboxHandle>>,
    ) -> Self {
        Self {
            id,
            request,
            sandbox_id,
            handle,
            state: PoolEntryState::Ready,
            lease: None,
            last_used_at: Instant::now(),
            last_health_check_at: None,
            next_retry_at: None,
            failure_count: 0,
            evictable: false,
            lifecycle: Arc::new(RwLock::new(())),
        }
    }
}

impl SandboxPool {
    pub fn new(
        key: SandboxPoolKey,
        backend: Arc<dyn ManagedSandboxBackend>,
        capacity: PoolCapacity,
        seed: SandboxPoolSeed,
    ) -> Result<Self> {
        if capacity.max_total == 0
            || capacity.target_ready == 0
            || capacity.min_ready > capacity.target_ready
            || capacity.target_ready > capacity.max_total
            || capacity.lease_ttl.is_zero()
        {
            bail!(
                "invalid pool capacity: require 0 <= min <= target <= max, positive target and lease TTL"
            );
        }
        Ok(Self {
            key,
            backend,
            seed,
            entries: Arc::new(Mutex::new(HashMap::new())),
            capacity,
            notify: Arc::new(Notify::new()),
            provider_operations: Arc::new(Semaphore::new(DEFAULT_PROVIDER_OPERATION_LIMIT)),
            changed: Arc::new(Notify::new()),
            reconcile: Mutex::new(()),
            closed: AtomicBool::new(false),
        })
    }

    pub async fn entry_count(&self) -> usize {
        self.entries.lock().await.len()
    }

    #[cfg(test)]
    async fn insert_entry(&self, entry: PoolEntry) -> Result<()> {
        let mut entries = self.entries.lock().await;
        if entries.contains_key(&entry.id) {
            bail!("sandbox pool entry already exists: {}", entry.id);
        }
        entries.insert(entry.id.clone(), entry);
        self.notify.notify_one();
        self.changed.notify_waiters();
        Ok(())
    }

    /// Lease a ready sandbox. If the entry has no live handle, acquire one
    /// from the provider using the request persisted on the entry.
    pub async fn try_acquire(
        &self,
        worker_id: impl Into<String>,
    ) -> Result<(SandboxLease, LeasedSandbox)> {
        if self.closed.load(Ordering::Acquire) {
            bail!("sandbox pool is closed");
        }
        let worker_id = worker_id.into();
        let (entry_id, lease, request, live_handle, lifecycle) = {
            let mut entries = self.entries.lock().await;
            let entry = entries
                .values_mut()
                .filter(|entry| entry.state == PoolEntryState::Ready)
                .min_by_key(|entry| entry.last_used_at)
                .ok_or_else(|| anyhow::Error::new(NoReadyCapacity))?;
            // Create lease and mark in entry table
            let lease = SandboxLease {
                id: Uuid7::now().to_string(),
                entry_id: entry.id.clone(),
                worker_id,
                fencing_token: Uuid7::now().to_string(),
                issued_at: Instant::now(),
                expires_at: Instant::now() + self.capacity.lease_ttl,
            };
            entry.state = PoolEntryState::Leased;
            entry.lease = Some(lease.clone());
            entry.last_used_at = Instant::now();
            (
                entry.id.clone(),
                lease,
                entry.request.clone(),
                entry.handle.clone(),
                Arc::clone(&entry.lifecycle),
            )
        };

        let _operation = lifecycle.read().await;
        let handle = match live_handle {
            Some(handle) => handle,
            None => match self.acquire_seeded(request).await {
                Ok(handle) => handle,
                Err(error) => {
                    self.mark_unknown(&entry_id, &lease).await;
                    return Err(error);
                }
            },
        };

        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(&entry_id)
            .ok_or_else(|| anyhow!("sandbox pool entry disappeared: {entry_id}"))?;
        if validate_lease(entry, &lease).is_err() {
            // A concurrent release/reset invalidated this acquisition. Do not
            // return a handle whose ownership is no longer represented by the
            // pool.
            drop(entries);
            handle
                .stop()
                .await
                .map_err(|error| anyhow!("failed stopping invalidated sandbox lease: {error}"))?;
            bail!("sandbox lease was invalidated while acquiring capacity");
        }
        // Record the live provider handle for this pool entry.
        entry.handle = Some(Arc::clone(&handle));
        entry.sandbox_id = handle.id().to_string();
        self.notify.notify_one();
        self.changed.notify_waiters();
        let leased = LeasedSandbox {
            lease: lease.clone(),
            handle,
            entries: Arc::clone(&self.entries),
            lifecycle: Arc::clone(&lifecycle),
            notify: Arc::clone(&self.notify),
            changed: Arc::clone(&self.changed),
        };
        Ok((lease, leased))
    }

    /// Wait for clean capacity. Run `run_reconciler` concurrently.
    /// Dropping this future cancels the wait; callers can use tokio::time::timeout.
    pub async fn acquire_any(
        &self,
        worker_id: impl Into<String>,
    ) -> Result<(SandboxLease, LeasedSandbox)> {
        let worker_id = worker_id.into();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.try_acquire(worker_id.clone()).await {
                Ok(lease) => return Ok(lease),
                Err(error) if error.is::<NoReadyCapacity>() => {}
                Err(error) => return Err(error),
            }
            self.notify.notify_one();
            changed.await;
        }
    }

    pub async fn heartbeat(&self, lease: &SandboxLease) -> Result<()> {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(&lease.entry_id)
            .ok_or_else(|| anyhow!("entry missing"))?;
        validate_lease(entry, lease)?;
        entry.lease.as_mut().expect("validated lease").expires_at =
            Instant::now() + self.capacity.lease_ttl;
        Ok(())
    }

    // This operation should remain behind the pool manager's authorization
    // boundary when the pool is exposed to remote workers.
    pub async fn release(&self, lease: &SandboxLease) -> Result<()> {
        self.reset(lease).await
    }

    // This operation should remain behind the pool manager's authorization
    // boundary when the pool is exposed to remote workers.
    pub async fn reset(&self, lease: &SandboxLease) -> Result<()> {
        let lifecycle = self.entry_lifecycle(&lease.entry_id).await?;
        let _operation = lifecycle.write().await;
        let (entry_id, request) = {
            let mut entries = self.entries.lock().await;
            let entry = entries
                .get_mut(&lease.entry_id)
                .ok_or_else(|| anyhow!("sandbox pool entry not found: {}", lease.entry_id))?;
            validate_lease(entry, lease)?;
            entry.state = PoolEntryState::Resetting;
            (entry.id.clone(), entry.request.clone())
        };

        if let Err(error) = self.terminate_with_provider(request).await {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(&entry_id) {
                entry.state = PoolEntryState::Unknown;
                entry.lease = None;
                entry.handle = None;
                entry.failure_count = entry.failure_count.saturating_add(1);
                entry.next_retry_at = Some(Instant::now() + retry_delay(entry.failure_count));
            }
            self.notify.notify_one();
            self.changed.notify_waiters();
            return Err(error);
        }

        self.entries.lock().await.remove(&entry_id);
        self.notify.notify_one();
        self.changed.notify_waiters();
        Ok(())
    }

    /// Stop every runtime in the pool and reject subsequent acquisitions.
    /// Call this after durable workspace state is saved and active users have
    /// released their leases.
    pub async fn drain(&self) -> Result<()> {
        let _reconcile = self.reconcile.lock().await;
        self.closed.store(true, Ordering::Release);
        let entry_ids = {
            let mut entries = self.entries.lock().await;
            entries
                .values_mut()
                .map(|entry| {
                    entry.state = PoolEntryState::Draining;
                    entry.lease = None;
                    entry.id.clone()
                })
                .collect::<Vec<_>>()
        };
        self.changed.notify_waiters();

        for entry_id in entry_ids {
            self.retire_entry(&entry_id).await?;
        }
        Ok(())
    }

    pub async fn health_check(&self, entry_id: &str) -> Result<()> {
        let lifecycle = self.entry_lifecycle(entry_id).await?;
        let _operation = lifecycle.write().await;
        let handle = {
            let mut entries = self.entries.lock().await;
            let entry = entries
                .get_mut(entry_id)
                .ok_or_else(|| anyhow!("sandbox pool entry not found: {entry_id}"))?;
            if entry.state != PoolEntryState::Ready {
                bail!("sandbox pool entry is draining: {entry_id}");
            }
            let handle = entry
                .handle
                .clone()
                .ok_or_else(|| anyhow!("sandbox pool entry has no live handle: {entry_id}"))?;
            entry.state = PoolEntryState::Checking;
            handle
        };

        let command = SandboxCommand {
            argv: vec!["true".to_string()],
            env: HashMap::new(),
            display_argv: Some(vec!["true".to_string()]),
            cwd: None,
            timeout: Some(Duration::from_secs(5)),
        };
        let _permit = self.provider_operations.acquire().await.map_err(|error| {
            anyhow!("sandbox pool provider-operation semaphore closed: {error}")
        })?;
        let result = time::timeout(Duration::from_secs(5), handle.exec(&command))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
            .and_then(|output| {
                if output.ok {
                    Ok(())
                } else {
                    bail!("health command failed")
                }
            });
        if let Err(error) = result {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(entry_id) {
                entry.state = PoolEntryState::Unhealthy;
                entry.last_health_check_at = Some(Instant::now());
                entry.failure_count = entry.failure_count.saturating_add(1);
                entry.next_retry_at = Some(Instant::now() + retry_delay(entry.failure_count));
            }
            return Err(error);
        }

        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get_mut(entry_id) {
            entry.state = PoolEntryState::Ready;
            entry.last_health_check_at = Some(Instant::now());
            entry.next_retry_at = None;
            entry.failure_count = 0;
        }
        self.changed.notify_waiters();
        Ok(())
    }

    /// Run one reconciliation pass. Provider calls are bounded and never run
    /// while the entry table mutex is held.
    pub async fn reconcile_once(&self) -> Result<()> {
        let _reconcile = self.reconcile.lock().await;
        self.evict_idle().await?;
        let (ready_entries, unhealthy_entries) = {
            let mut entries = self.entries.lock().await;
            let now = Instant::now();
            let mut ready_entries = Vec::new();
            let mut unhealthy_entries = Vec::new();
            for entry in entries.values_mut() {
                if entry.state == PoolEntryState::Leased
                    && entry
                        .lease
                        .as_ref()
                        .is_some_and(|lease| lease.expires_at <= now)
                {
                    // The old worker is fenced from lifecycle operations, but
                    // the provider state is still unknown until reconciliation.
                    entry.state = PoolEntryState::Unknown;
                }
                match entry.state {
                    PoolEntryState::Ready
                        if entry.last_health_check_at.is_none_or(|last| {
                            now.duration_since(last) >= DEFAULT_HEALTH_CHECK_INTERVAL
                        }) =>
                    {
                        ready_entries.push(entry.id.clone())
                    }
                    PoolEntryState::Unhealthy
                    | PoolEntryState::Unknown
                    | PoolEntryState::Resetting
                    | PoolEntryState::Creating
                    | PoolEntryState::Checking
                    | PoolEntryState::Evicting
                        if entry.next_retry_at.is_none_or(|retry| retry <= now) =>
                    {
                        unhealthy_entries.push(entry.id.clone())
                    }
                    _ => {}
                }
            }
            (ready_entries, unhealthy_entries)
        };

        for entry_id in ready_entries {
            if let Err(error) = self.health_check(&entry_id).await {
                tracing::warn!(%error, entry_id, "sandbox pool health check failed");
            }
        }

        for entry_id in unhealthy_entries {
            if let Err(error) = self.retire_entry(&entry_id).await {
                tracing::warn!(%error, entry_id, "failed retiring unhealthy sandbox pool entry");
            }
        }

        self.ensure_capacity().await
    }

    /// Run the event-driven pool reconciler until the shutdown watch is set.
    /// The timer is only a safety sweep; normal changes wake the loop through
    /// `Notify`.
    pub async fn run_reconciler(&self, mut shutdown: watch::Receiver<bool>) {
        let mut interval = time::interval(DEFAULT_RECONCILE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            if *shutdown.borrow() {
                self.closed.store(true, Ordering::Release);
                self.changed.notify_waiters();
                return;
            }
            tokio::select! {
                _ = self.notify.notified() => {
                    if let Err(error) = self.reconcile_once().await {
                        tracing::warn!(%error, "sandbox pool reconciliation failed");
                    }
                }
                _ = interval.tick() => {
                    if let Err(error) = self.reconcile_once().await {
                        tracing::warn!(%error, "sandbox pool reconciliation failed");
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        self.closed.store(true, Ordering::Release);
                        self.changed.notify_waiters();
                        return;
                    }
                }
            }
        }
    }

    async fn evict_idle(&self) -> Result<()> {
        if self.capacity.idle_ttl.is_zero() {
            return Ok(());
        }

        let candidates = {
            let mut entries = self.entries.lock().await;
            let now = Instant::now();
            let ready_count = entries
                .values()
                .filter(|entry| entry.state == PoolEntryState::Ready)
                .count();
            let evictable_count = ready_count.saturating_sub(self.capacity.min_ready);
            let mut candidates = entries
                .values_mut()
                .filter(|entry| {
                    entry.state == PoolEntryState::Ready
                        && entry.lease.is_none()
                        && entry.evictable
                        && now.duration_since(entry.last_used_at) >= self.capacity.idle_ttl
                })
                .collect::<Vec<_>>();
            candidates.sort_unstable_by_key(|entry| entry.last_used_at);
            candidates
                .into_iter()
                .take(evictable_count)
                .map(|entry| {
                    entry.state = PoolEntryState::Evicting;
                    entry.id.clone()
                })
                .collect::<Vec<_>>()
        };

        for entry_id in candidates {
            self.retire_entry(&entry_id).await?;
        }
        Ok(())
    }

    async fn ensure_capacity(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        loop {
            let mut entries = self.entries.lock().await;
            let ready_or_creating = entries
                .values()
                .filter(|entry| {
                    matches!(
                        entry.state,
                        PoolEntryState::Ready | PoolEntryState::Creating
                    )
                })
                .count();
            let desired_ready = self.capacity.target_ready.max(self.capacity.min_ready);
            if ready_or_creating >= desired_ready || entries.len() >= self.capacity.max_total {
                return Ok(());
            }

            let entry_id = format!("pool-entry-{}", Uuid7::now());
            let request = self.pool_request(&entry_id);
            {
                entries.insert(
                    entry_id.clone(),
                    PoolEntry {
                        id: entry_id.clone(),
                        request: request.clone(),
                        sandbox_id: String::new(),
                        handle: None,
                        state: PoolEntryState::Creating,
                        lease: None,
                        last_used_at: Instant::now(),
                        last_health_check_at: None,
                        next_retry_at: None,
                        failure_count: 0,
                        evictable: true,
                        lifecycle: Arc::new(RwLock::new(())),
                    },
                );
            }

            drop(entries);
            match self.acquire_seeded(request).await {
                Ok(handle) => {
                    let mut entries = self.entries.lock().await;
                    if let Some(entry) = entries.get_mut(&entry_id) {
                        entry.sandbox_id = handle.id().to_string();
                        entry.request.provider_state = handle.provider_state();
                        entry.handle = Some(handle);
                        entry.state = PoolEntryState::Ready;
                        entry.last_used_at = Instant::now();
                        entry.failure_count = 0;
                        entry.next_retry_at = None;
                    }
                }
                Err(error) => {
                    let mut entries = self.entries.lock().await;
                    if let Some(entry) = entries.get_mut(&entry_id) {
                        entry.state = PoolEntryState::Unknown;
                        entry.failure_count = entry.failure_count.saturating_add(1);
                        entry.next_retry_at =
                            Some(Instant::now() + retry_delay(entry.failure_count));
                    }
                    return Err(error);
                }
            }
            self.notify.notify_one();
            self.changed.notify_waiters();
        }
    }

    async fn entry_lifecycle(&self, entry_id: &str) -> Result<Arc<RwLock<()>>> {
        self.entries
            .lock()
            .await
            .get(entry_id)
            .map(|entry| Arc::clone(&entry.lifecycle))
            .ok_or_else(|| anyhow!("sandbox pool entry not found: {entry_id}"))
    }

    async fn retire_entry(&self, entry_id: &str) -> Result<()> {
        let lifecycle = self.entry_lifecycle(entry_id).await?;
        let _operation = lifecycle.write().await;
        let request = {
            let mut entries = self.entries.lock().await;
            let entry = entries
                .get_mut(entry_id)
                .ok_or_else(|| anyhow!("sandbox pool entry not found: {entry_id}"))?;
            if !matches!(
                entry.state,
                PoolEntryState::Unknown
                    | PoolEntryState::Creating
                    | PoolEntryState::Checking
                    | PoolEntryState::Draining
                    | PoolEntryState::Unhealthy
                    | PoolEntryState::Evicting
                    | PoolEntryState::Resetting
            ) {
                return Ok(());
            }
            entry.state = PoolEntryState::Resetting;
            entry.request.clone()
        };

        if let Err(error) = self.terminate_with_provider(request).await {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(entry_id) {
                entry.state = PoolEntryState::Unknown;
                entry.failure_count = entry.failure_count.saturating_add(1);
                entry.next_retry_at = Some(Instant::now() + retry_delay(entry.failure_count));
            }
            return Err(error);
        }
        self.entries.lock().await.remove(entry_id);
        self.notify.notify_one();
        self.changed.notify_waiters();
        Ok(())
    }

    fn pool_request(&self, entry_id: &str) -> SandboxRequest {
        SandboxRequest {
            key: crate::SandboxKey::PoolSandbox {
                pool_id: self.key.pool_id.clone(),
                entry_id: entry_id.to_string(),
            },
            spec: self.key.spec.clone(),
            lifecycle: crate::SandboxLifecycleConfig {
                idle_ttl: Some(self.capacity.idle_ttl),
            },
            provider_state: None,
        }
    }

    async fn acquire_seeded(
        &self,
        request: SandboxRequest,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let _permit = self.provider_operations.acquire().await.map_err(|error| {
            anyhow!("sandbox pool provider-operation semaphore closed: {error}")
        })?;
        match &self.seed {
            SandboxPoolSeed::Empty => {
                time::timeout(Duration::from_secs(120), self.backend.acquire(request)).await?
            }
            SandboxPoolSeed::Snapshot(snapshot) => {
                time::timeout(
                    Duration::from_secs(120),
                    self.backend
                        .acquire_from_snapshot(request, snapshot.clone()),
                )
                .await?
            }
        }
    }

    async fn terminate_with_provider(&self, request: SandboxRequest) -> Result<()> {
        let _permit = self.provider_operations.acquire().await.map_err(|error| {
            anyhow!("sandbox pool provider-operation semaphore closed: {error}")
        })?;
        time::timeout(Duration::from_secs(120), self.backend.terminate(request)).await?
    }

    async fn mark_unknown(&self, entry_id: &str, lease: &SandboxLease) {
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get_mut(entry_id)
            && entry.lease.as_ref() == Some(lease)
        {
            entry.state = PoolEntryState::Unknown;
            entry.lease = None;
            entry.handle = None;
            self.notify.notify_one();
            self.changed.notify_waiters();
        }
    }
}

/// A capability valid only while its pool lease is active.
/// Runtime lifecycle stays with the pool. Release destroys the runtime, including
/// background work and local files; checkpoint durable data before releasing.
pub struct LeasedSandbox {
    lease: SandboxLease,
    handle: Arc<dyn ManagedSandboxHandle>,
    entries: Arc<Mutex<HashMap<String, PoolEntry>>>,
    lifecycle: Arc<RwLock<()>>,
    notify: Arc<Notify>,
    changed: Arc<Notify>,
}

impl LeasedSandbox {
    pub fn id(&self) -> &str {
        self.handle.id()
    }

    pub async fn exec(&self, command: &SandboxCommand) -> Result<crate::SandboxCommandOutput> {
        let operation = self.lifecycle.read().await;
        {
            let entries = self.entries.lock().await;
            let entry = entries
                .get(&self.lease.entry_id)
                .ok_or_else(|| anyhow!("lease entry removed"))?;
            validate_lease(entry, &self.lease)?;
        }
        let timeout = command.timeout.unwrap_or(Duration::from_secs(300));
        let result = time::timeout(timeout, self.handle.exec(command))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result);
        drop(operation);

        if result.is_err() {
            self.quarantine().await;
        }
        result
    }

    async fn quarantine(&self) {
        let quarantined = {
            let mut entries = self.entries.lock().await;
            let Some(entry) = entries.get_mut(&self.lease.entry_id) else {
                return;
            };
            if !lease_matches(entry.lease.as_ref(), &self.lease)
                || entry.state != PoolEntryState::Leased
            {
                return;
            }
            entry.state = PoolEntryState::Unknown;
            entry.lease = None;
            entry.handle = None;
            entry.failure_count = entry.failure_count.saturating_add(1);
            entry.next_retry_at = Some(Instant::now() + retry_delay(entry.failure_count));
            true
        };
        if quarantined {
            self.notify.notify_one();
            self.changed.notify_waiters();
        }
    }
}

#[derive(Debug)]
pub struct NoReadyCapacity;
impl std::fmt::Display for NoReadyCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("sandbox pool has no ready capacity")
    }
}
impl std::error::Error for NoReadyCapacity {}

fn retry_delay(failure_count: u32) -> Duration {
    let exponent = failure_count.saturating_sub(1).min(6);
    Duration::from_secs(1_u64 << exponent)
}

fn validate_lease(entry: &PoolEntry, lease: &SandboxLease) -> Result<()> {
    if !lease_matches(entry.lease.as_ref(), lease) {
        bail!("sandbox lease is not valid for entry {}", entry.id);
    }
    if entry.state != PoolEntryState::Leased {
        bail!("sandbox pool entry is not leased: {}", entry.id);
    }
    if entry.lease.as_ref().expect("validated lease").expires_at <= Instant::now() {
        bail!("sandbox lease has expired: {}", lease.id);
    }
    Ok(())
}

fn lease_matches(current: Option<&SandboxLease>, lease: &SandboxLease) -> bool {
    current.is_some_and(|current| {
        current.id == lease.id
            && current.fencing_token == lease.fencing_token
            && current.worker_id == lease.worker_id
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use anyhow::bail;
    use async_trait::async_trait;

    use super::*;
    use crate::{
        SandboxAttachment, SandboxCommandOutput, SandboxKey, SandboxLifecycleConfig,
        SandboxProcessParts, SnapshotFormat, SnapshotPayload,
    };

    struct FakeBackend {
        fail_acquire: AtomicBool,
        fail_terminate: AtomicBool,
        acquire_count: AtomicUsize,
        terminate_count: AtomicUsize,
        snapshot_acquire_count: AtomicUsize,
        healthy: Arc<AtomicBool>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                fail_acquire: AtomicBool::new(false),
                fail_terminate: AtomicBool::new(false),
                acquire_count: AtomicUsize::new(0),
                terminate_count: AtomicUsize::new(0),
                snapshot_acquire_count: AtomicUsize::new(0),
                healthy: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    #[async_trait]
    impl ManagedSandboxBackend for FakeBackend {
        fn is_local(&self) -> bool {
            true
        }

        fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
            &[]
        }

        async fn acquire(&self, _request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
            if self.fail_acquire.load(Ordering::SeqCst) {
                bail!("fake acquire failed");
            }
            let sequence = self.acquire_count.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Arc::new(FakeHandle {
                id: format!("fake-sandbox-{sequence}"),
                healthy: Arc::clone(&self.healthy),
            }))
        }

        async fn attach(
            &self,
            _request: SandboxRequest,
            _attachment: SandboxAttachment,
        ) -> Result<Arc<dyn ManagedSandboxHandle>> {
            bail!("fake backend does not support attach")
        }

        async fn acquire_from_snapshot(
            &self,
            request: SandboxRequest,
            _payload: SnapshotPayload,
        ) -> Result<Arc<dyn ManagedSandboxHandle>> {
            self.snapshot_acquire_count.fetch_add(1, Ordering::SeqCst);
            self.acquire(request).await
        }

        async fn terminate(&self, _request: SandboxRequest) -> Result<()> {
            if self.fail_terminate.load(Ordering::SeqCst) {
                bail!("fake terminate failed");
            }
            self.terminate_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeHandle {
        id: String,
        healthy: Arc<AtomicBool>,
    }

    #[async_trait]
    impl ManagedSandboxHandle for FakeHandle {
        fn id(&self) -> &str {
            &self.id
        }

        async fn exec(&self, _command: &SandboxCommand) -> Result<SandboxCommandOutput> {
            if !self.healthy.load(Ordering::SeqCst) {
                bail!("fake health check failed");
            }
            Ok(SandboxCommandOutput {
                ok: true,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                command: vec!["true".to_string()],
                cwd: "/".to_string(),
            })
        }

        async fn start_process(&self, _command: &SandboxCommand) -> Result<SandboxProcessParts> {
            bail!("fake handle does not support processes")
        }

        async fn stop(&self) -> Result<()> {
            Ok(())
        }

        async fn detach(&self) -> Result<SandboxAttachment> {
            bail!("fake handle does not support detach")
        }

        async fn snapshot(&self) -> Result<SnapshotPayload> {
            bail!("fake handle does not support snapshots")
        }
    }

    struct SlowHandle;

    #[async_trait]
    impl ManagedSandboxHandle for SlowHandle {
        fn id(&self) -> &str {
            "slow-sandbox"
        }

        async fn exec(&self, _command: &SandboxCommand) -> Result<SandboxCommandOutput> {
            time::sleep(Duration::from_secs(1)).await;
            unreachable!("the lease wrapper should time out this call")
        }

        async fn start_process(&self, _command: &SandboxCommand) -> Result<SandboxProcessParts> {
            bail!("slow handle does not support processes")
        }

        async fn stop(&self) -> Result<()> {
            Ok(())
        }

        async fn detach(&self) -> Result<SandboxAttachment> {
            bail!("slow handle does not support detach")
        }

        async fn snapshot(&self) -> Result<SnapshotPayload> {
            bail!("slow handle does not support snapshots")
        }
    }

    fn request(entry_id: &str) -> SandboxRequest {
        SandboxRequest {
            key: SandboxKey::PoolSandbox {
                pool_id: "pool".to_string(),
                entry_id: entry_id.to_string(),
            },
            spec: SandboxSpec {
                image: "fake-image".to_string(),
                resources: Default::default(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                network: crate::SandboxNetworkPolicy::Disabled,
                default_workdir: "/".to_string(),
            },
            lifecycle: SandboxLifecycleConfig::default(),
            provider_state: None,
        }
    }

    fn pool(backend: Arc<FakeBackend>) -> SandboxPool {
        let spec = request("entry").spec;
        SandboxPool::new(
            SandboxPoolKey {
                pool_id: "pool".to_string(),
                provider: SandboxProvider::from_static("fake"),
                spec,
            },
            backend,
            PoolCapacity {
                min_ready: 0,
                target_ready: 1,
                max_total: 1,
                lease_ttl: Duration::from_secs(60),
                idle_ttl: Duration::from_secs(300),
            },
            SandboxPoolSeed::Empty,
        )
        .unwrap()
    }

    async fn state(pool: &SandboxPool, entry_id: &str) -> PoolEntryState {
        pool.entries
            .lock()
            .await
            .get(entry_id)
            .expect("pool entry should exist")
            .state
    }

    #[tokio::test]
    async fn acquire_and_release_transitions_entry() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        pool.insert_entry(PoolEntry::new(
            "entry".to_string(),
            request("entry"),
            "placeholder".to_string(),
            None,
        ))
        .await
        .unwrap();

        let (lease, handle) = pool.try_acquire("worker-a").await.unwrap();
        assert_eq!(handle.id(), "fake-sandbox-1");
        assert_eq!(state(&pool, "entry").await, PoolEntryState::Leased);
        assert_eq!(backend.acquire_count.load(Ordering::SeqCst), 1);

        pool.release(&lease).await.unwrap();
        assert_eq!(pool.entry_count().await, 0);
        assert_eq!(backend.acquire_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn acquire_skips_leased_entries() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        pool.insert_entry(PoolEntry::new(
            "entry".to_string(),
            request("entry"),
            "placeholder".to_string(),
            None,
        ))
        .await
        .unwrap();

        let _lease = pool.try_acquire("worker-a").await.unwrap();
        assert!(pool.try_acquire("worker-b").await.is_err());
    }

    #[tokio::test]
    async fn stale_lease_cannot_release_entry() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        pool.insert_entry(PoolEntry::new(
            "entry".to_string(),
            request("entry"),
            "placeholder".to_string(),
            None,
        ))
        .await
        .unwrap();

        let (lease, _) = pool.try_acquire("worker-a").await.unwrap();
        let mut stale = lease.clone();
        stale.fencing_token = "stale-token".to_string();
        assert!(pool.release(&stale).await.is_err());
        assert_eq!(state(&pool, "entry").await, PoolEntryState::Leased);
        pool.release(&lease).await.unwrap();
    }

    #[tokio::test]
    async fn acquire_failure_marks_entry_unknown() {
        let backend = Arc::new(FakeBackend::new());
        backend.fail_acquire.store(true, Ordering::SeqCst);
        let pool = pool(Arc::clone(&backend));
        pool.insert_entry(PoolEntry::new(
            "entry".to_string(),
            request("entry"),
            "placeholder".to_string(),
            None,
        ))
        .await
        .unwrap();

        assert!(pool.try_acquire("worker-a").await.is_err());
        assert_eq!(state(&pool, "entry").await, PoolEntryState::Unknown);
    }

    #[tokio::test]
    async fn reset_terminates_and_removes_entry() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        pool.insert_entry(PoolEntry::new(
            "entry".to_string(),
            request("entry"),
            "placeholder".to_string(),
            None,
        ))
        .await
        .unwrap();

        let (lease, _) = pool.try_acquire("worker-a").await.unwrap();
        pool.reset(&lease).await.unwrap();
        assert_eq!(pool.entry_count().await, 0);
        assert_eq!(backend.terminate_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reset_failure_marks_entry_unknown() {
        let backend = Arc::new(FakeBackend::new());
        backend.fail_terminate.store(true, Ordering::SeqCst);
        let pool = pool(Arc::clone(&backend));
        pool.insert_entry(PoolEntry::new(
            "entry".to_string(),
            request("entry"),
            "placeholder".to_string(),
            None,
        ))
        .await
        .unwrap();

        let (lease, _) = pool.try_acquire("worker-a").await.unwrap();
        assert!(pool.reset(&lease).await.is_err());
        assert_eq!(state(&pool, "entry").await, PoolEntryState::Unknown);
    }

    #[tokio::test]
    async fn health_check_marks_failed_entry_unhealthy() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        let handle: Arc<dyn ManagedSandboxHandle> = Arc::new(FakeHandle {
            id: "fake-sandbox".to_string(),
            healthy: Arc::clone(&backend.healthy),
        });
        pool.insert_entry(PoolEntry::new(
            "entry".to_string(),
            request("entry"),
            "fake-sandbox".to_string(),
            Some(handle),
        ))
        .await
        .unwrap();

        pool.health_check("entry").await.unwrap();
        backend.healthy.store(false, Ordering::SeqCst);
        assert!(pool.health_check("entry").await.is_err());
        assert_eq!(state(&pool, "entry").await, PoolEntryState::Unhealthy);
    }

    #[tokio::test]
    async fn reconcile_once_replenishes_to_target_capacity() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));

        pool.reconcile_once().await.unwrap();

        assert_eq!(pool.entry_count().await, 1);
        assert_eq!(backend.acquire_count.load(Ordering::SeqCst), 1);
        let entries = pool.entries.lock().await;
        assert!(
            entries
                .values()
                .all(|entry| entry.state == PoolEntryState::Ready)
        );
    }

    #[tokio::test]
    async fn snapshot_seed_initializes_each_fresh_pool_entry() {
        let backend = Arc::new(FakeBackend::new());
        let pool = SandboxPool::new(
            SandboxPoolKey {
                pool_id: "seeded-pool".to_string(),
                provider: SandboxProvider::from_static("fake"),
                spec: request("entry").spec,
            },
            Arc::clone(&backend) as Arc<dyn ManagedSandboxBackend>,
            PoolCapacity {
                min_ready: 0,
                target_ready: 1,
                max_total: 1,
                lease_ttl: Duration::from_secs(60),
                idle_ttl: Duration::from_secs(300),
            },
            SandboxPoolSeed::Snapshot(SnapshotPayload {
                format: SnapshotFormat::WorkspaceChunksV1,
                bytes: bytes::Bytes::from_static(b"seeded codebase"),
            }),
        )
        .unwrap();

        pool.reconcile_once().await.unwrap();

        assert_eq!(backend.snapshot_acquire_count.load(Ordering::SeqCst), 1);
        assert_eq!(backend.acquire_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn acquire_any_uses_least_recently_used_ready_entry() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        let old_handle: Arc<dyn ManagedSandboxHandle> = Arc::new(FakeHandle {
            id: "old-sandbox".to_string(),
            healthy: Arc::clone(&backend.healthy),
        });
        let new_handle: Arc<dyn ManagedSandboxHandle> = Arc::new(FakeHandle {
            id: "new-sandbox".to_string(),
            healthy: Arc::clone(&backend.healthy),
        });
        pool.insert_entry(PoolEntry::new(
            "old".to_string(),
            request("old"),
            "old-sandbox".to_string(),
            Some(old_handle),
        ))
        .await
        .unwrap();
        pool.insert_entry(PoolEntry::new(
            "new".to_string(),
            request("new"),
            "new-sandbox".to_string(),
            Some(new_handle),
        ))
        .await
        .unwrap();
        {
            let mut entries = pool.entries.lock().await;
            entries.get_mut("old").unwrap().last_used_at =
                Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        }

        let (_, handle) = pool.try_acquire("worker-a").await.unwrap();
        assert_eq!(handle.id(), "old-sandbox");
    }

    #[tokio::test]
    async fn reconcile_preserves_warm_target() {
        let backend = Arc::new(FakeBackend::new());
        let mut pool = pool(Arc::clone(&backend));
        pool.capacity = PoolCapacity {
            min_ready: 0,
            target_ready: 1,
            max_total: 1,
            lease_ttl: Duration::from_secs(60),
            idle_ttl: Duration::from_secs(1),
        };
        let mut entry = PoolEntry::new(
            "entry".to_string(),
            request("entry"),
            "sandbox".to_string(),
            Some(Arc::new(FakeHandle {
                id: "sandbox".to_string(),
                healthy: Arc::clone(&backend.healthy),
            })),
        );
        entry.evictable = true;
        entry.last_used_at = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        pool.insert_entry(entry).await.unwrap();

        pool.reconcile_once().await.unwrap();

        assert_eq!(pool.entry_count().await, 1);
        assert_eq!(backend.terminate_count.load(Ordering::SeqCst), 1);
    }

    fn command() -> SandboxCommand {
        SandboxCommand {
            argv: vec!["true".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(1)),
        }
    }

    #[tokio::test]
    async fn end_to_end_wait_release_replace_and_shutdown() {
        let backend = Arc::new(FakeBackend::new());
        let pool = Arc::new(pool(Arc::clone(&backend)));
        let (shutdown, receiver) = watch::channel(false);
        let reconciler = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move { pool.run_reconciler(receiver).await }
        });
        let (first, old_handle) = time::timeout(Duration::from_secs(2), pool.acquire_any("first"))
            .await
            .unwrap()
            .unwrap();
        assert!(old_handle.exec(&command()).await.unwrap().ok);
        pool.heartbeat(&first).await.unwrap();
        assert!(
            time::timeout(Duration::from_millis(20), pool.acquire_any("cancelled"))
                .await
                .is_err()
        );
        let waiter = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move { pool.acquire_any("second").await }
        });
        pool.release(&first).await.unwrap();
        assert!(old_handle.exec(&command()).await.is_err());
        let (second, new_handle) = time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_ne!(old_handle.id(), new_handle.id());
        assert!(pool.release(&first).await.is_err());
        assert!(new_handle.exec(&command()).await.unwrap().ok);
        pool.release(&second).await.unwrap();
        shutdown.send(true).unwrap();
        time::timeout(Duration::from_secs(2), reconciler)
            .await
            .unwrap()
            .unwrap();
        assert!(pool.acquire_any("after shutdown").await.is_err());
    }

    #[tokio::test]
    async fn concurrent_reconciliation_respects_capacity() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        let (a, b, c) = tokio::join!(
            pool.reconcile_once(),
            pool.reconcile_once(),
            pool.reconcile_once()
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        assert_eq!(pool.entry_count().await, 1);
        assert_eq!(backend.acquire_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_creation_recovers_after_backoff() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        backend.fail_acquire.store(true, Ordering::SeqCst);
        assert!(pool.reconcile_once().await.is_err());
        pool.reconcile_once().await.unwrap();
        assert_eq!(backend.terminate_count.load(Ordering::SeqCst), 0);
        backend.fail_acquire.store(false, Ordering::SeqCst);
        for entry in pool.entries.lock().await.values_mut() {
            entry.next_retry_at = Some(Instant::now());
        }
        pool.reconcile_once().await.unwrap();
        assert_eq!(backend.terminate_count.load(Ordering::SeqCst), 1);
        assert!(pool.try_acquire("recovered").await.is_ok());
    }

    #[tokio::test]
    async fn health_checks_do_not_extend_idle_lifetime() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(backend);
        pool.reconcile_once().await.unwrap();
        let (id, last_used) = {
            let entries = pool.entries.lock().await;
            let entry = entries.values().next().unwrap();
            (entry.id.clone(), entry.last_used_at)
        };
        pool.health_check(&id).await.unwrap();
        assert_eq!(pool.entries.lock().await[&id].last_used_at, last_used);
    }

    #[tokio::test]
    async fn idle_eviction_is_lru_and_preserves_minimum() {
        let backend = Arc::new(FakeBackend::new());
        let mut pool = pool(Arc::clone(&backend));
        pool.capacity.min_ready = 1;
        pool.capacity.target_ready = 2;
        pool.capacity.max_total = 2;
        pool.reconcile_once().await.unwrap();
        let old_id = {
            let mut entries = pool.entries.lock().await;
            let mut ids = entries.keys().cloned().collect::<Vec<_>>();
            ids.sort();
            entries.get_mut(&ids[0]).unwrap().last_used_at =
                Instant::now() - Duration::from_secs(600);
            entries.get_mut(&ids[1]).unwrap().last_used_at =
                Instant::now() - Duration::from_secs(400);
            ids[0].clone()
        };
        pool.evict_idle().await.unwrap();
        assert_eq!(pool.entry_count().await, 1);
        assert!(!pool.entries.lock().await.contains_key(&old_id));
        pool.reconcile_once().await.unwrap();
        assert_eq!(pool.entry_count().await, 2);
        assert_eq!(backend.terminate_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn health_check_cannot_change_active_lease() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(backend);
        pool.reconcile_once().await.unwrap();
        let (lease, handle) = pool.try_acquire("active").await.unwrap();
        assert!(pool.health_check(&lease.entry_id).await.is_err());
        assert!(handle.exec(&command()).await.unwrap().ok);
        assert_eq!(state(&pool, &lease.entry_id).await, PoolEntryState::Leased);
    }

    #[tokio::test]
    async fn expired_handles_are_fenced_and_replaced() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        pool.reconcile_once().await.unwrap();
        let (lease, handle) = pool.try_acquire("expired").await.unwrap();
        pool.entries
            .lock()
            .await
            .get_mut(&lease.entry_id)
            .unwrap()
            .lease
            .as_mut()
            .unwrap()
            .expires_at = Instant::now();
        assert!(handle.exec(&command()).await.is_err());
        assert!(pool.heartbeat(&lease).await.is_err());
        pool.reconcile_once().await.unwrap();
        assert_eq!(backend.terminate_count.load(Ordering::SeqCst), 1);
        let (_, replacement) = pool.try_acquire("replacement").await.unwrap();
        assert_ne!(handle.id(), replacement.id());
    }

    #[tokio::test]
    async fn provider_failure_quarantines_and_replaces_a_leased_runtime() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(Arc::clone(&backend));
        pool.reconcile_once().await.unwrap();
        let (lease, handle) = pool.try_acquire("worker").await.unwrap();

        backend.healthy.store(false, Ordering::SeqCst);
        assert!(handle.exec(&command()).await.is_err());
        assert_eq!(state(&pool, &lease.entry_id).await, PoolEntryState::Unknown);
        assert!(pool.heartbeat(&lease).await.is_err());

        backend.healthy.store(true, Ordering::SeqCst);
        pool.entries
            .lock()
            .await
            .get_mut(&lease.entry_id)
            .unwrap()
            .next_retry_at = Some(Instant::now());
        pool.reconcile_once().await.unwrap();

        assert_eq!(backend.terminate_count.load(Ordering::SeqCst), 1);
        let (_, replacement) = pool.try_acquire("replacement").await.unwrap();
        assert_ne!(handle.id(), replacement.id());
        assert!(replacement.exec(&command()).await.unwrap().ok);
    }

    #[tokio::test]
    async fn command_timeout_quarantines_a_leased_runtime() {
        let backend = Arc::new(FakeBackend::new());
        let pool = pool(backend);
        pool.insert_entry(PoolEntry::new(
            "slow".to_string(),
            request("slow"),
            "slow-sandbox".to_string(),
            Some(Arc::new(SlowHandle)),
        ))
        .await
        .unwrap();
        let (lease, sandbox) = pool.try_acquire("worker").await.unwrap();
        let mut timed = command();
        timed.timeout = Some(Duration::from_millis(1));

        assert!(sandbox.exec(&timed).await.is_err());
        assert_eq!(state(&pool, &lease.entry_id).await, PoolEntryState::Unknown);
        assert!(pool.release(&lease).await.is_err());
    }
}

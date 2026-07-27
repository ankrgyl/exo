# Local Sandbox Harness Design

Status: proposed

## Summary

This document proposes a local sandbox layer for `exo`'s future harness. The
goal is to keep sandboxing local, fast, and pluggable without making VM
management the main project.

The key design decision is:

- The harness owns lifecycle, checkpoints, policy, and state.
- The execution backend is replaceable.
- The current recommended backend is Apple `container`.
- The backend should model a sandbox as a named environment, not necessarily a
  single VM or single process.
- The initial environment shape is one primary dev container plus zero or more
  sibling service containers on a private network.
- The primary checkpoint unit is the host workspace and local SQL state, not the
  full VM.

That gives us:

- warm sandboxes with repeated `exec`
- cheap rewind/fork via filesystem and SQLite checkpoints
- the ability to run Linux processes and sibling service containers
- a clean path to other backends later

## Incremental Plan

The end-state we care about is an `exo-dev` style agent that can take a
feature workspace, work on it inside a sandbox, and eventually keep that
environment warm. The right path to that is incremental.

### Current state

Today we already have:

- a sandbox backend interface
- Apple `container` as the first backend
- tool-backed shell commands routed through that backend
- conversation-owned mount state
- a configurable base image on the agent definition
- agent-owned sandbox lifecycle config, including an optional idle TTL
- per-mount read-only vs read-write behavior in the runtime model
- a generic shell tool that relies on mount permissions instead of a shell allowlist

What we do not have yet:

- fallback writable workspace semantics for dirty trees and non-git paths
- warm named environments
- explicit lifecycle config
- a good dev-environment profile

### Phase 1: make filesystem semantics real

This phase is implemented.

Work in this phase:

- rely on per-mount read-only vs read-write enforcement from the container
  runtime
- keep one primary workspace mount by convention at `/home/exo/workspace`
- let the shell tool stay generic and stop treating "read-only" as a command
  allowlist problem

Outcome:

- a conversation can have mounts with real access modes
- the shell tool becomes closer to the final shape we actually want

### Phase 2: provision writable workspaces

This phase is partially implemented.

Today we have:

- a workspace provisioner abstraction
- a `git worktree` provisioning path
- a requirement that the worktree path create a named branch up front
- read-write mounting of that provisioned workspace at the primary workspace path
- a conversation-level sandbox network policy (`enabled` / `disabled`)

What remains:

- copied/cloned fallback provisioning for dirty trees and non-git paths
- richer network policy beyond simple on/off if we need per-environment service
  topologies later

Once filesystem semantics are real, the next problem is protecting the source repo.

Work in this phase:

- add a workspace provisioner abstraction
- first provisioning strategy: `git worktree` for feature work, but require a
  branch so the worktree does not start detached
- let writable local-dev flows disable outbound network access by capability
- fallback strategy: copied/cloned workspace for dirty trees or non-git paths
- mount the provisioned workspace read-write into the sandbox

Outcome:

- the agent can modify files in a sandbox without touching the source working
  tree directly
- we get the core of the eventual `exo-dev` workflow without needing warm
  containers yet

### Phase 3: add lifecycle

This phase is mostly implemented.

Today we have:

- agent-level sandbox lifecycle config, including base image and optional idle TTL
- named warm sandboxes keyed by conversation
- reuse of warm sandboxes during the idle window
- Apple `container exec` for warm command execution instead of one-shot `run` per command

What remains:

- reintroduce explicit sandbox snapshots once warm named sandboxes and checkpoint semantics are real
- stream in-flight stdout/stderr from long-running sandbox tools

Only after writable workspaces work well should we optimize lifecycle.

Work in this phase:

- put lifecycle config such as base image and idle TTL on the agent definition
- keep environments alive until idle timeout
- switch from one-shot `container run` to named warm environments plus `exec`

Outcome:

- repeated commands become cheap
- warm sandboxes become a performance optimization, not a prerequisite for basic
  development workflows

### Phase 4: ship a real dev agent

With writable workspaces and warm lifecycle in place, we can create a useful
default dev environment.

This phase is not implemented yet.

Work in this phase:

- build an `exo-dev` base image with the dependencies this repo needs
- define the expected conversation/workspace flow for feature work
- decide how sibling services such as databases should fit into that
  environment

Outcome:

- a user can point `exo-dev` at a feature workspace and let it work there
  end-to-end inside the sandbox

## Backend Decision

We considered four realistic backends for macOS:

- Apple `container`
- Lima
- `krunvm`
- direct `libkrun` / Virtualization.framework

### Recommendation

Use Apple `container` first.

Why:

- it already gives us the lifecycle primitives we actually need:
  - `run`
  - `exec`
  - `stop`
  - `network create`
  - `volume create`
- warm command execution is already fast enough for a devbox workflow
- it avoids nested Docker and instead supports the standard sibling-service model
- it is a better fit for per-environment networks than `krunvm`
- it is a cleaner path to a bundled product surface than Lima

Primary sources:

- `container` repo: https://github.com/apple/container
- `container` how-to: https://github.com/apple/container/blob/main/docs/how-to.md
- `containerization` repo: https://github.com/apple/containerization

### Apple `container`

Pros:

- native command surface for running long-lived containers
- `exec` into warm containers
- named networks and volumes
- each Linux container runs inside a lightweight VM
- best fit for "one app container plus sibling Postgres/Redis/etc."
- lightest product-facing dependency among the practical options

Cons:

- newer and less mature than Lima
- no native Compose runner today
- Swift/Apple-first implementation stack under the hood
- container semantics rather than "named Linux VM" semantics

Relevant docs:

- https://github.com/apple/container
- https://github.com/apple/container/blob/main/docs/how-to.md
- https://github.com/apple/containerization

### Lima

Pros:

- mature control plane
- named instances, start/stop/restart, snapshots
- well-documented guest container support

Cons:

- heavier dependency
- CLI and config model are awkward
- users need Lima installed unless we bundle/manage it

Relevant docs:

- https://lima-vm.io/docs/reference/limactl/
- https://lima-vm.io/docs/reference/limactl_shell/
- https://lima-vm.io/docs/reference/limactl_snapshot/
- https://lima-vm.io/docs/examples/containers/containerd/

### `krunvm`

Pros:

- minimal footprint
- fast boot time
- zero disk image maintenance
- host volume mapping
- guest port exposure
- works on macOS/Hypervisor.framework on ARM64

Cons:

- smaller ecosystem than Lima
- less batteries-included lifecycle state
- still a helper runtime dependency
- weak fit for repeated `exec` into a long-lived environment

Relevant docs:

- https://github.com/containers/krunvm

### Direct `libkrun` / Virtualization.framework

Pros:

- lowest dependency surface
- stable public C API from `1.0.0`
- best control over lifecycle and performance tuning

Cons:

- highest implementation cost
- we would need to build our own helper/runtime surface
- networking, exec protocol, and image/rootfs handling become our problem

Relevant docs:

- https://github.com/containers/libkrun

### Decision Rule

Pick the highest-level backend that does not distort the product.

Today that means:

- Apple `container` first
- Lima only if we later decide we want stronger VM-oriented lifecycle features
- direct `libkrun` / Virtualization.framework only if runtime ownership becomes a
  core project

## Goals

- Start a sandbox on demand.
- Keep it warm across many agent turns.
- Run commands inside it with low steady-state latency.
- Allow long-lived processes while warm.
- Allow sibling services such as Postgres and Redis inside the environment.
- Support rewind, restore, and fork at the harness layer.
- Keep the sandbox backend pluggable.

## Non-Goals

- Build a general-purpose VM manager.
- Expose raw backend-specific CLI flags or config formats to application code.
- Depend on full-VM snapshots for every turn.
- Preserve running process memory across cold restarts.

## Core Principles

### 1. The harness owns control-plane state

The harness decides:

- which sandbox is attached to which workstream or branch
- when a sandbox is warm or cold
- when a checkpoint exists
- how a checkpoint is restored or forked
- what tools and capabilities are available in a turn

The helper runtime should never become the source of truth for this logic.

### 2. Workspace checkpoints are the fast path

Per-turn checkpointing should snapshot:

- the workspace directory
- the local relational database
- turn metadata
- artifacts

Per-turn checkpointing should not default to:

- full runtime VM snapshots
- live process checkpoints

This keeps checkpoints cheap and deterministic.

### 3. Warm sandboxes are the normal path

The steady-state model is:

1. ensure the sandbox is started
2. run many commands inside it
3. checkpoint host state occasionally
4. stop it when idle or explicitly cooled down

If the sandbox goes cold, restoring from the latest checkpoint is acceptable.

### 4. The runtime backend is replaceable

The harness should be written against a backend interface. Apple `container` is
just the first backend.

## Proposed Architecture

There are six major layers.

### 1. Transport Adapter

This handles CLI, API, or any future ingress surface.

Responsibilities:

- map inbound events to `WorkstreamId`
- load recent conversation state
- send streaming and final replies
- keep track of subscribed workstreams

This layer should not know how sandboxing works internally.

### 2. Harness Core

This is the source of truth for turn execution.

Responsibilities:

- classify the turn: `private`, `internal`, `external_shared`
- decide inline vs background execution
- assemble model context
- choose tools and skills
- acquire sandbox and database capabilities
- commit checkpoints
- emit turn events for streaming/UIs

### 3. Checkpoint Store

This stores logical turn checkpoints.

Responsibilities:

- persist message history and compaction state
- persist workspace snapshot metadata
- persist SQL checkpoint metadata
- support rewind and fork
- map `BranchId -> latest CheckpointId`

### 4. Sandbox Manager

This provides warm and cold execution environments.

Responsibilities:

- create and start warm environments
- attach host workspace mounts
- run commands in the primary dev container
- stop or restart environments
- create sibling service containers
- create environment-scoped networks and volumes

### 5. Structured Store

This is the harness-level relational database.

Responsibilities:

- turn metadata
- task and workflow state
- checkpoint graph
- skills and artifact indexes
- optional agent-visible SQL data

This can begin as SQLite.

### 6. Model Runtime

This is the provider abstraction layer.

Responsibilities:

- accept normalized messages and tools
- stream model events
- support MCP and native tools
- remain provider-neutral via Lingua-style message/tool schemas

The harness owns policy. The model runtime just executes.

## Data Model

These are the core identifiers.

```rust
pub struct WorkstreamId(pub String);
pub struct BranchId(pub String);
pub struct CheckpointId(pub String);
pub struct SandboxId(pub String);
pub struct WorkspaceSnapshotId(pub String);
pub struct DatabaseSnapshotId(pub String);
pub struct ArtifactId(pub String);
```

Core records:

```rust
pub enum AccessScope {
    Private,
    Internal,
    ExternalShared,
}

pub struct CheckpointRecord {
    pub id: CheckpointId,
    pub branch_id: BranchId,
    pub parent_id: Option<CheckpointId>,
    pub workstream_id: WorkstreamId,
    pub access_scope: AccessScope,
    pub workspace_snapshot_id: WorkspaceSnapshotId,
    pub database_snapshot_id: DatabaseSnapshotId,
    pub artifact_ids: Vec<ArtifactId>,
    pub compaction_ref: Option<String>,
    pub created_at: String,
}

pub struct SandboxLease {
    pub sandbox_id: SandboxId,
    pub branch_id: BranchId,
    pub checkpoint_id: CheckpointId,
    pub warm: bool,
}
```

In the Apple `container` backend, `SandboxId` refers to an environment, not a
single container. An environment may contain:

- one primary dev container
- one private network
- zero or more sibling service containers
- zero or more named volumes

## Proposed Interfaces

The interfaces below are Rust-shaped, but they are intentionally backend- and
transport-agnostic.

### Harness Core

```rust
#[async_trait::async_trait]
pub trait Harness {
    async fn handle_turn(
        &self,
        input: TurnInput,
    ) -> anyhow::Result<impl futures::Stream<Item = TurnEvent>>;
}

pub struct TurnInput {
    pub workstream_id: WorkstreamId,
    pub access_scope: AccessScope,
    pub user_message: String,
    pub history: Vec<MessageRecord>,
    pub background: bool,
}

pub enum TurnEvent {
    TextDelta(String),
    ToolStarted { name: String },
    ToolFinished { name: String, summary: String },
    CheckpointCommitted { checkpoint_id: CheckpointId },
    FinalMessage(String),
}
```

### Sandbox Manager

```rust
#[async_trait::async_trait]
pub trait SandboxManager {
    async fn ensure_started(
        &self,
        request: SandboxStartRequest,
    ) -> anyhow::Result<SandboxHandle>;

    async fn stop(&self, sandbox_id: &SandboxId) -> anyhow::Result<()>;

    async fn snapshot_workspace(
        &self,
        sandbox_id: &SandboxId,
        request: WorkspaceSnapshotRequest,
    ) -> anyhow::Result<WorkspaceSnapshotId>;

    async fn snapshot_database(
        &self,
        sandbox_id: &SandboxId,
        request: DatabaseSnapshotRequest,
    ) -> anyhow::Result<DatabaseSnapshotId>;

    async fn ensure_service(
        &self,
        sandbox_id: &SandboxId,
        request: ServiceStartRequest,
    ) -> anyhow::Result<ServiceHandle>;
}

pub struct SandboxStartRequest {
    pub branch_id: BranchId,
    pub checkpoint_id: CheckpointId,
    pub image: String,
    pub workspace_snapshot_id: WorkspaceSnapshotId,
    pub database_snapshot_id: DatabaseSnapshotId,
    pub network_policy: NetworkPolicy,
    pub resources: SandboxResources,
}

pub struct SandboxResources {
    pub cpus: u8,
    pub memory_gib: u16,
    pub disk_gib: u16,
}

pub enum NetworkPolicy {
    Disabled,
    Default,
    Custom(String),
}

pub struct ServiceStartRequest {
    pub name: String,
    pub image: String,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<ServiceMount>,
    pub publish_ports: Vec<PortBinding>,
}

pub struct ServiceMount {
    pub source: String,
    pub target: String,
}
```

### Sandbox Handle

```rust
#[async_trait::async_trait]
pub trait SandboxHandle {
    fn sandbox_id(&self) -> &SandboxId;

    async fn exec(&self, request: ExecRequest) -> anyhow::Result<ExecResult>;

    async fn open_port(&self, guest_port: u16) -> anyhow::Result<PortBinding>;

    async fn write_file(&self, path: &str, contents: &[u8]) -> anyhow::Result<()>;

    async fn read_file(&self, path: &str) -> anyhow::Result<Vec<u8>>;
}

pub struct ServiceHandle {
    pub name: String,
}

pub struct ExecRequest {
    pub command: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub timeout_secs: u64,
}

pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub struct PortBinding {
    pub host_port: u16,
    pub guest_port: u16,
}
```

### Checkpoint Store

```rust
#[async_trait::async_trait]
pub trait CheckpointStore {
    async fn head(&self, branch_id: &BranchId) -> anyhow::Result<CheckpointRecord>;

    async fn commit(&self, request: CommitCheckpointRequest)
        -> anyhow::Result<CheckpointRecord>;

    async fn fork(
        &self,
        source: &CheckpointId,
        new_branch: &BranchId,
    ) -> anyhow::Result<CheckpointRecord>;

    async fn set_head(
        &self,
        branch_id: &BranchId,
        checkpoint_id: &CheckpointId,
    ) -> anyhow::Result<()>;
}

pub struct CommitCheckpointRequest {
    pub branch_id: BranchId,
    pub parent_id: CheckpointId,
    pub workstream_id: WorkstreamId,
    pub access_scope: AccessScope,
    pub workspace_snapshot_id: WorkspaceSnapshotId,
    pub database_snapshot_id: DatabaseSnapshotId,
    pub artifact_ids: Vec<ArtifactId>,
    pub compaction_ref: Option<String>,
}
```

### Structured Store

```rust
#[async_trait::async_trait]
pub trait StructuredStore {
    async fn open_agent_db(
        &self,
        database_snapshot_id: &DatabaseSnapshotId,
    ) -> anyhow::Result<AgentSqlConnection>;

    async fn snapshot_agent_db(
        &self,
        conn: &mut AgentSqlConnection,
    ) -> anyhow::Result<DatabaseSnapshotId>;
}
```

The initial implementation should use SQLite and the backup API. Agent-facing SQL
should feel normal; snapshotting happens at turn boundaries.

### Model Runtime

```rust
#[async_trait::async_trait]
pub trait ModelRuntime {
    async fn run(
        &self,
        request: ModelRequest,
    ) -> anyhow::Result<impl futures::Stream<Item = ModelEvent>>;
}

pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub system_prompt: String,
    pub tools: Vec<ModelTool>,
    pub skills: Vec<SkillReference>,
}

pub enum ModelEvent {
    TextDelta(String),
    ToolCallRequested { tool_name: String, payload: serde_json::Value },
    Completed,
}
```

### Skill Registry

```rust
#[async_trait::async_trait]
pub trait SkillRegistry {
    async fn list(&self) -> anyhow::Result<Vec<SkillSummary>>;
    async fn read(&self, id: &str) -> anyhow::Result<SkillBody>;
    async fn search(&self, query: &str) -> anyhow::Result<Vec<SkillSummary>>;
}
```

Skills should be virtual resources, not filesystem-installed prompt bundles.

## Apple `container` Backend Design

The Apple `container` backend should be treated as a thin adapter around the
`container` CLI and its background system service. It should not leak raw CLI
flags or command wiring to the rest of the harness.

### Environment Model

Each warm sandbox gets a named environment:

```text
exo-<branch-id>
```

Each environment owns:

- one primary dev container
- one private network
- zero or more service containers
- zero or more named volumes

The names are owned by the harness. Application code never deals with raw
container ids directly.

### Runtime Model

The backend should treat the environment as a warm Linux devbox:

- one primary Linux container image as the dev environment
- writable host workspace mount at `/workspace`
- writable host state mount at `/state`
- repeated `exec` while warm
- optional sibling services such as Postgres and Redis on the same private network

This should feel like a long-running developer environment, not a one-shot
command runner.

### Backend Lifecycle

The backend should roughly translate the interface above into:

- `ensure_started`
  - `container network create` if missing
  - `container run --detach` for the primary dev container if missing
  - `container start` if the primary dev container is stopped
- `exec`
  - `container exec` inside the primary dev container with `/workspace` as cwd
- `stop`
  - `container stop` on all containers in the environment
- `open_port`
  - use `--publish` when a host-visible port is actually required
- `ensure_service`
  - `container run --detach --network <env-network>` for the named service
  - attach named volumes and env vars as needed

### Service Topology

If the environment needs Postgres, Redis, or other service dependencies during
feature development, the default model is:

- sibling containers on the same named network
- one service container per dependency
- one named volume per stateful dependency when persistence is needed

This is intentionally the same model used by Compose-like systems, just without
native Compose support.

Nested Docker inside the primary dev container is explicitly not the intended
model.

### Compose-Like Manifests

Apple `container` does not natively run the Compose Specification today.

For our purposes, that is acceptable. The harness can adopt a small internal
service manifest shaped like a subset of Compose:

- services
- networks
- volumes
- environment
- published ports

The backend can then materialize that manifest through `container network`,
`container volume`, and `container run`.

## Checkpoint Strategy

### Workspace Checkpoints

Primary mechanism:

- host workspace directory
- APFS clone or recursive copy to create immutable snapshots

We should snapshot the mounted host directory, not ask the runtime to snapshot
the whole VM on every turn.

### Database Checkpoints

Primary mechanism:

- SQLite backup API
- immutable checkpoint files

The agent sees a normal SQL database. The harness snapshots it per turn.

### VM Snapshots

Runtime-level snapshots should be optional and rare:

- useful for debugging
- useful for expensive provisioning checkpoints
- not the default per-turn restore path

This avoids coupling turn semantics to runtime internals.

## Turn Lifecycle

The expected turn flow is:

1. Resolve `WorkstreamId` and `AccessScope`.
2. Load the branch head checkpoint.
3. Ensure the sandbox for that branch is started.
4. Restore the workspace and SQL state from the branch head if needed.
5. Run one or more commands in the sandbox.
6. Let the model call tools, including sandbox-backed tools.
7. Snapshot the workspace and SQL state.
8. Commit a new checkpoint record.
9. Stream results back to the transport.

## Warm and Cold Behavior

### Warm

Warm means:

- primary dev container is running
- workspace mount is attached
- SQL state mount is attached
- repeated `exec` calls should avoid instance startup latency

This is the default state for active branches.

### Cold

Cold means:

- primary dev container is stopped
- the latest checkpoint is still durable

Restoring from cold means:

1. start the primary dev container
2. remount workspace and state
3. continue from latest checkpoint

Running processes do not survive cold transitions. Files, volumes, and
structured state do.

## Performance Strategy

The performance budget should be optimized for steady-state `exec`, not just boot
time.

Rules:

- do not create or destroy the primary dev container per command
- keep active sandboxes warm
- checkpoint host workspace, not container root filesystems, on every turn
- only use runtime-level snapshots for infrequent coarse checkpoints
- avoid copying the entire repo into the container if a host mount is sufficient

Expected performance shape:

- first start: noticeable but acceptable
- repeated commands in a warm primary container: close to normal shell latency
- checkpoints: bounded by APFS clone plus SQLite backup cost

## Risks

### Warm instance leaks

Mitigation:

- explicit idle timeout
- branch-to-sandbox lease table
- hard kill on reclaim

### Multi-container orchestration complexity

Mitigation:

- start with one private network per environment
- one primary dev container
- only the minimum set of sibling services
- no nested Docker

### Helper runtime behavior

Mitigation:

- never let application code shell out to the helper directly
- keep integration tests around instance lifecycle

## Implementation Plan

### Phase 1

- add a `SandboxBackend` abstraction
- implement a `ContainerBackend`
- support `ensure_started`, `exec`, and `stop`
- use host-mounted workspace and SQLite state

### Phase 2

- add workspace checkpoints via APFS clone fallback copy
- add SQLite snapshotting via backup API
- add branch head and fork support
- add a small internal service manifest for sibling services

### Phase 3

- add sibling service helpers
- add optional host port publishing
- add named-volume lifecycle management

### Phase 4

- optional direct `containerization` or `libkrun` backend
- same harness interfaces, lower-level implementation

## Recommendation

Build the harness around:

- warm local environments
- host workspace checkpoints
- SQLite turn checkpoints
- backend interfaces that keep the runtime isolated behind a single module
- sibling service containers on private per-environment networks

Current recommendation:

- start with Apple `container`
- keep the interfaces generic
- revisit lower-level runtime ownership only if the CLI/service becomes the real bottleneck

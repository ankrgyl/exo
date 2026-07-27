# Sandbox Snapshots

Status: implemented for the Docker, E2B, Sprites, Tensorlake, and Daytona
sandbox backends; stubs in place for the others. Daytona can additionally
restore a Docker snapshot (the cross-provider "teleport" bridge).

## Summary

A snapshot captures the current filesystem state of a sandboxed container so
that the sandbox can later be rewound to that state. Snapshots are taken,
listed, and replayed within an `exo` conversation. They give the user — or in
a later iteration, an executor policy — the ability to time-travel a
sandbox's state without forking the conversation itself.

The earlier model only recorded snapshot _metadata_ (a UUID written to the
event log). This work adds the captured artifact, the persistence layer for
it, and the restore path that actually consumes it.

## What you get

- `ConversationHandle::snapshot_sandbox(id)` actually captures the live
  container's filesystem and persists it.
- `ConversationHandle::start_sandbox(StartSandboxRequest { id, snapshot_id, .. })`
  starts a fresh container whose filesystem is sourced from the snapshot,
  preserving the original sandbox's mounts, network policy, and lifecycle.
- A chat-REPL slash-command surface — `/snapshot`, `/snapshots`, `/rewind <id>`,
  `/teleport <provider>` — that drives the round-trip without leaving the
  conversation.
- **Teleportation**: `start_sandbox` accepts an optional `provider` override,
  so a snapshot taken on one backend can be restored under another. The
  flagship path is local Docker → Daytona: the same sandbox id, resumed
  remotely with its filesystem intact.

## What this is not

- **Not a process or memory checkpoint.** Only filesystem state is captured.
  Running processes inside the container are not preserved; they are
  re-launched fresh from the restored image.
- **Not a conversation rewind.** The event log, message history, and prior
  tool calls are untouched. Use `conversation fork` to rewind the
  conversation itself.
- **Not yet cross-process.** A snapshot can only be taken of a sandbox that
  is live in the _current_ `exo` process (`running_sandboxes` is per-process).
  See "Known limits" below.

## Model

Snapshots are an interaction between three layers:

```
ConversationHandle             ManagedSandboxHandle           ManagedSandboxBackend
       │                              │                                │
  snapshot_sandbox(id) ──► running_sandboxes.get(id).snapshot() ──┐    │
       │                                                          │    │
       ◄────────── SnapshotPayload { kind, bytes } ────────────────┘    │
       │                                                                │
  put_bytes / put_json                                                  │
  (manifest.json + payload.bin)                                         │
                                                                        │
  start_sandbox(req) ─── load manifest + payload ──► acquire_from_snapshot(req, payload)
```

`ConversationHandle` orchestrates: it locates the live handle, asks for a
payload, persists the bytes, updates sandbox metadata, and emits the
`SandboxSnapshotted` event. `ManagedSandboxHandle::snapshot` and
`ManagedSandboxBackend::acquire_from_snapshot` are the backend-specific
methods that produce and consume the bytes.

### SnapshotPayload and SnapshotKind

```rust
pub struct SnapshotPayload {
    pub kind: SnapshotKind,
    pub bytes: Bytes,
}

pub enum SnapshotKind {
    DockerImageTar,
    // future: AppleContainerImageTar, etc.
}
```

`SnapshotPayload` is opaque to the harness. The `kind` tag is the contract
between producer and consumer: a payload produced by one backend can only be
restored by a backend that knows how to interpret that kind. The harness
never inspects `bytes` — it just persists them and hands them back on
restore.

## Docker pipeline

`ManagedSandboxHandle::snapshot` (Docker):

1. `ensure_warm_sandbox_ready` — make sure the container exists and is the
   one in the warm cache for this `SandboxKey`.
2. `docker commit -p <container> exo-snap-<uuid>` — pause the container
   during commit for a consistent filesystem capture, then create a new
   image from its layers.
3. `docker save exo-snap-<uuid>` — export the image as a tarball on stdout;
   capture into `Bytes`.
4. `docker image rm exo-snap-<uuid>` — drop the local image. The canonical
   store of the snapshot lives in exoharness storage, not the docker daemon.

`ManagedSandboxBackend::acquire_from_snapshot` (Docker):

1. Validate that `payload.kind == DockerImageTar`.
2. `docker load < payload.bytes` — load the image back into the local
   daemon; parse stdout to find the assigned image reference (the line
   `Loaded image: <ref>`).
3. Build a fresh `SandboxRequest` with `spec.image` swapped for the loaded
   reference. Mounts, network policy, default workdir, lifecycle, and
   `SandboxKey` are preserved from the original request.
4. Evict any pre-existing warm container for this key (we want a fresh
   container booted from the restored image, not a reuse of whatever was
   running before).
5. `docker run --detach …` with the loaded image — exactly the same path as
   a normal cold-start container, just with a different image.

## On-disk layout

Snapshots live under the conversation directory, alongside other
conversation-scoped artifacts:

```
agents/<agent_id>/conversations/<conversation_id>/snapshots/<snapshot_id>/
├── manifest.json   JSON sidecar (StoredSnapshotManifest)
└── payload.bin     raw blob (docker save tarball for SnapshotKind::DockerImageTar)
```

The manifest schema:

```json
{
  "snapshot_id": "019e5782-7c6b-72a2-b4fa-a81bf56eb37e",
  "sandbox_id": "sandbox-019e5782-2a46-7970-a5bf-62900a2233e8",
  "kind": "docker_image_tar",
  "created_at": "2026-05-24T01:03:49.867230008Z",
  "payload_size_bytes": 48498688
}
```

This mirrors the existing artifact layout (sidecar `.json` + `.bin` blob in
a per-id directory). A future migration to chunked or streamed storage
would touch a small surface.

The snapshot's existence is also recorded in the conversation event log as
`SandboxSnapshotted { sandbox_id, snapshot_id }`, which is what
`/snapshots` walks to list past snapshots.

## CLI surface

Inside the chat REPL (`exo chat repl <agent> <conv>`):

```
/snapshot           capture the conversation's currently-running sandbox;
                    prints the new snapshot id
/snapshots          list snapshots taken in this conversation
/rewind <id>        stop the current sandbox, start a fresh one from the
                    named snapshot
/teleport <provider> snapshot the live sandbox and restore it under another
                    provider (e.g. `/teleport daytona` moves a local Docker
                    sandbox up to Daytona)
/help               show command list
```

There is intentionally no top-level `exo conversation snapshot` subcommand
today — see "Known limits" for the cross-invocation gap that makes such
a subcommand useless until it's resolved.

## Executable demo

[`crates/cli/tests/snapshot_round_trip.rs`](../crates/cli/tests/snapshot_round_trip.rs)
is the canonical, runnable reference for using the snapshot APIs. It drives
the harness library directly (no LLM, no binary spawn) and exercises the same
lifecycle this doc describes. Run it manually with:

```
EXO_TEST_SANDBOX_BACKEND=docker cargo test --package exo \
    --test snapshot_round_trip -- --ignored --nocapture
```

The CI integration workflow runs it on push to `main` against each Linux
matrix cell that supports docker. The test self-skips on cells that don't
(`local-process`) so they don't false-fail.

Two live Daytona analogs (both `#[ignore]`d; they need `DAYTONA_API_KEY`
— snapshots are available in Daytona's shared `us` region — and, for the
teleport, a local docker daemon):

```
# native Daytona snapshot + rewind
cargo test -p exo --test snapshot_round_trip_daytona -- --ignored --nocapture

# teleport: local Docker sandbox snapshotted and resumed on Daytona
cargo test -p exo --test teleport_docker_to_daytona -- --ignored --nocapture
```

## Extending to another sandbox backend

To add snapshot support for a new backend (say, Apple's `container` CLI
when it grows a commit/save flow):

1. Add a new variant to `SnapshotKind` — e.g. `AppleContainerImageTar`.
   The tag is the contract; pick a name that names the on-disk format.
2. Implement `ManagedSandboxHandle::snapshot` for that backend's handle
   type, producing the appropriate `SnapshotPayload`. The Docker version in
   `docker_snapshot_container` is the template — three CLI calls and a
   `Bytes` capture.
3. Implement `ManagedSandboxBackend::acquire_from_snapshot` to consume the
   same `kind`, including the safety check that the payload's `kind`
   matches what the backend understands. The Docker version is the
   template here too — load the bytes, get the loaded image reference,
   swap `request.spec.image`, evict + recreate the warm container.
4. Backends that genuinely can't snapshot (the local-process backend
   today, since there's no isolated filesystem) should return an explicit
   error from both methods rather than silently degrading.

No other layer changes. The conversation orchestration, on-disk layout,
and CLI surface are all backend-agnostic.

## Known limits

### Cross-invocation container adoption

Today each `exo` process maintains its own `running_sandboxes` map. A
container created by one invocation is not adopted by a later one even
though it is still alive on the docker daemon, so snapshots can only be
taken of sandboxes acquired in the current process. That is why the
snapshot/rewind UX lives in the chat REPL (one long-running process holds
the container for the conversation's duration) rather than as standalone
`exo` subcommands.

The fix is well-scoped — on `acquire`, query
`docker ps --filter label=exo.sandbox.key=<key> --filter status=running` and
adopt the existing container if its `exo.sandbox.spec-hash` label matches
the requested spec. Once that lands, `exo conversation snapshot` and
`exo conversation rewind` become trivial CLI subcommands that just call the
same `ConversationHandle` methods the REPL slash commands use.

### Payload size

`SnapshotPayload::bytes` is a single `Bytes` blob and the harness's
`put_bytes` / `get_bytes` take/return `Vec<u8>`. For the typical
debian-base + small workspace, that is a 30-70 MB blob held in memory
during capture and restore — acceptable but not great. A streamed
producer/consumer interface (`AsyncRead`/`AsyncWrite`) is a clean
follow-up if larger images become routine.

### Snapshot lifecycle

There is no GC. Snapshots remain on disk until the conversation directory
is deleted. A future addition could prune snapshots older than the most
recent N, or evict by total size.

### Restore semantics

Restore is a fresh container booted from the restored image, not a
checkpoint of running processes or in-memory state. Any long-running
processes the agent had started inside the container are not preserved;
they would need to be re-launched from the restored filesystem state.
This is consistent with how a fresh container is brought up for any chat
turn.

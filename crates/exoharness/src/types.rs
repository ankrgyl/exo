use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::ops::Bound;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite};
use lingua::{Message, universal::UniversalStreamChunk};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Result, Uuid7};

#[async_trait]
pub trait ExoHarness: Send + Sync {
    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>>;
    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>>;
    async fn new_agent(&self, request: NewAgentRequest) -> Result<Arc<dyn AgentHandle>>;
    async fn delete_agent(&self, id: &AgentId) -> Result<bool>;

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>>;
    async fn put_binding(&self, binding: Binding) -> Result<BindingId>;
    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>>;

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>>;
    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId>;
    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>>;
}

#[async_trait]
pub trait SnapshotHandle: Send + Sync {
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId>;
    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()>;
}

#[async_trait]
pub trait SandboxHandle: SnapshotHandle {
    async fn list_sandboxes(&self) -> Result<Vec<SandboxRecord>>;
    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId>;
    async fn fork_sandbox(&self, request: ForkSandboxRequest) -> Result<SandboxId>;
    /// Create a new sandbox directly from an immutable snapshot. Unlike
    /// `start_sandbox`, the target need not already exist.
    async fn restore_sandbox(&self, request: RestoreSandboxRequest) -> Result<SandboxId>;
    async fn terminate_sandbox(&self, id: SandboxId) -> Result<()>;
    async fn attach_sandbox(&self, request: AttachSandboxRequest) -> Result<SandboxId>;
    async fn detach_sandbox(&self, id: SandboxId) -> Result<SandboxAttachment>;
    async fn stop_sandbox(&self, id: SandboxId) -> Result<()>;
    #[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
    async fn sandbox_supports_tcp(&self, _id: SandboxId) -> Result<bool> {
        Ok(false)
    }
    #[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
    async fn connect_sandbox_tcp(
        &self,
        _id: SandboxId,
        _port: u16,
    ) -> Result<Option<crate::BoxSandboxTcpStream>> {
        Ok(None)
    }
    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord>;
    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()>;
    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()>;
    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult>;
    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus>;
    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus>;
    async fn run_in_sandbox(&self, request: RunInSandboxRequest)
    -> Result<Box<dyn SandboxProcess>>;
}

#[async_trait]
pub trait AgentHandle: SandboxHandle {
    fn record(&self) -> &AgentRecord;

    async fn list_threads(
        &self,
        request: ListThreadsRequest,
    ) -> Result<ListThreadsResult<Arc<dyn ThreadHandle>>> {
        Ok(self.list_conversations(request).await?.into())
    }
    async fn get_thread(&self, id: &ThreadId) -> Result<Option<Arc<dyn ThreadHandle>>> {
        self.get_conversation(id).await
    }
    async fn new_thread(&self, request: NewThreadRequest) -> Result<Arc<dyn ThreadHandle>> {
        self.new_conversation(request).await
    }
    async fn delete_thread(&self, id: &ThreadId) -> Result<bool> {
        self.delete_conversation(id).await
    }

    async fn list_conversations(
        &self,
        request: ListConversationsRequest,
    ) -> Result<ListConversationsResult<Arc<dyn ConversationHandle>>>;
    async fn get_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<Arc<dyn ConversationHandle>>>;
    async fn new_conversation(
        &self,
        request: NewConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>>;
    async fn delete_conversation(&self, id: &ConversationId) -> Result<bool>;

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>>;
    async fn put_binding(&self, binding: Binding) -> Result<BindingId>;
    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>>;

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>>;
    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId>;
    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>>;

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion>;
    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>>;
    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>>;
}

#[async_trait]
pub trait ThreadHandle: SandboxHandle {
    fn record(&self) -> &ThreadRecord;

    async fn start_session(&self) -> Result<SessionId>;
    async fn end_session(&self, id: SessionId) -> Result<()>;
    async fn begin_turn(&self, request: BeginTurnRequest) -> Result<Arc<dyn TurnHandle>>;
    /// Rebuilds the local TurnHandle facade for an already-created turn.
    /// The durable identity is the agent, thread, session, and turn ids;
    /// this method only bundles those ids back into the trait object API.
    async fn turn_handle(&self, record: TurnRecord) -> Result<Arc<dyn TurnHandle>>;

    async fn get_events(&self, query: Option<EventQuery>) -> Result<GetEventsResult>;
    async fn watch_events(&self, after_exclusive: Bound<EventId>) -> Result<EventStream>;
    async fn get_event(&self, id: EventId) -> Result<Option<Event>>;
    async fn add_events(&self, request: AddEventsRequest) -> Result<AddEventsResult>;
    async fn fork(&self, request: ForkThreadRequest) -> Result<Arc<dyn ThreadHandle>>;

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion>;
    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>>;
    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>>;

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>>;
    async fn put_binding(&self, binding: Binding) -> Result<BindingId>;
    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>>;

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>>;
    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId>;
    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>>;
}

/// Compatibility name for [`ThreadHandle`].
pub use ThreadHandle as ConversationHandle;

#[async_trait]
pub trait TurnHandle: SnapshotHandle {
    fn record(&self) -> &TurnRecord;

    async fn add_events(&self, data: Vec<EventData>) -> Result<AddEventsResult>;
    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion>;
    async fn finish(&self) -> Result<EventId>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRecord {
    pub id: AgentId,
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewAgentRequest {
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadRecord {
    pub id: ThreadId,
    pub slug: String,
    pub name: String,
    pub latest_event_id: Option<EventId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewThreadRequest {
    pub slug: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListThreadsRequest {
    pub cursor: Option<EventId>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListConversationsResult<T> {
    pub conversations: Vec<T>,
    pub next_cursor: Option<EventId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListThreadsResult<T> {
    pub threads: Vec<T>,
    pub next_cursor: Option<EventId>,
}

impl<T> From<ListConversationsResult<T>> for ListThreadsResult<T> {
    fn from(result: ListConversationsResult<T>) -> Self {
        Self {
            threads: result.conversations,
            next_cursor: result.next_cursor,
        }
    }
}

impl<T> From<ListThreadsResult<T>> for ListConversationsResult<T> {
    fn from(result: ListThreadsResult<T>) -> Self {
        Self {
            conversations: result.threads,
            next_cursor: result.next_cursor,
        }
    }
}

/// Compatibility name for [`ThreadRecord`].
pub type ConversationRecord = ThreadRecord;
/// Compatibility name for [`NewThreadRequest`].
pub type NewConversationRequest = NewThreadRequest;
/// Compatibility name for [`ListThreadsRequest`].
pub type ListConversationsRequest = ListThreadsRequest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnRecord {
    pub id: TurnId,
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BeginTurnRequest {
    pub session_id: Option<SessionId>,
    pub input: Vec<Message>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventQuery {
    pub cursor: Option<EventId>,
    pub direction: Option<EventQueryDirection>,
    pub limit: Option<u32>,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub types: Option<Vec<EventKind>>,
}

/// Tag identifying an `EventData` variant — used by `EventQuery::types` to
/// filter events by kind without stringly comparing snake_case names. The
/// constants below cover every built-in variant; `EventKind::custom(name)`
/// is the escape hatch for `EventData::Custom { event_type, .. }`.
///
/// Wire format is the same single-string-per-kind shape the underlying
/// `EventData` serde tag uses (`#[serde(transparent)]`), so changing
/// `EventQuery::types` from `Vec<String>` to `Vec<EventKind>` is a
/// source-level change only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventKind(Cow<'static, str>);

impl EventKind {
    pub const THREAD_CREATED: EventKind = EventKind(Cow::Borrowed("thread_created"));
    pub const THREAD_UPDATED: EventKind = EventKind(Cow::Borrowed("thread_updated"));
    pub const THREAD_DELETED: EventKind = EventKind(Cow::Borrowed("thread_deleted"));
    pub const THREAD_FORKED: EventKind = EventKind(Cow::Borrowed("thread_forked"));
    pub const CONVERSATION_CREATED: EventKind = EventKind(Cow::Borrowed("conversation_created"));
    pub const CONVERSATION_UPDATED: EventKind = EventKind(Cow::Borrowed("conversation_updated"));
    pub const CONVERSATION_DELETED: EventKind = EventKind(Cow::Borrowed("conversation_deleted"));
    pub const CONVERSATION_FORKED: EventKind = EventKind(Cow::Borrowed("conversation_forked"));
    pub const SESSION_STARTED: EventKind = EventKind(Cow::Borrowed("session_started"));
    pub const SESSION_ENDED: EventKind = EventKind(Cow::Borrowed("session_ended"));
    pub const TURN_STARTED: EventKind = EventKind(Cow::Borrowed("turn_started"));
    pub const TURN_ENDED: EventKind = EventKind(Cow::Borrowed("turn_ended"));
    pub const MESSAGES: EventKind = EventKind(Cow::Borrowed("messages"));
    pub const TOOL_REQUESTED: EventKind = EventKind(Cow::Borrowed("tool_requested"));
    pub const TOOL_RESULT: EventKind = EventKind(Cow::Borrowed("tool_result"));
    pub const LINGUA_STREAM_CHUNK: EventKind = EventKind(Cow::Borrowed("lingua_stream_chunk"));
    pub const ERROR: EventKind = EventKind(Cow::Borrowed("error"));
    pub const ARTIFACT_WRITTEN: EventKind = EventKind(Cow::Borrowed("artifact_written"));
    pub const SANDBOX_CREATED: EventKind = EventKind(Cow::Borrowed("sandbox_created"));
    pub const SANDBOX_STARTED: EventKind = EventKind(Cow::Borrowed("sandbox_started"));
    pub const SANDBOX_STOPPED: EventKind = EventKind(Cow::Borrowed("sandbox_stopped"));
    pub const SANDBOX_ATTACHED: EventKind = EventKind(Cow::Borrowed("sandbox_attached"));
    pub const SANDBOX_DETACHED: EventKind = EventKind(Cow::Borrowed("sandbox_detached"));
    pub const SANDBOX_SELECTED: EventKind = EventKind(Cow::Borrowed("sandbox_selected"));
    pub const SANDBOX_SNAPSHOTTED: EventKind = EventKind(Cow::Borrowed("sandbox_snapshotted"));
    pub const SANDBOX_PROCESS_STARTED: EventKind =
        EventKind(Cow::Borrowed("sandbox_process_started"));
    pub const SANDBOX_PROCESS_STATE_UPDATED: EventKind =
        EventKind(Cow::Borrowed("sandbox_process_state_updated"));
    pub const SANDBOX_PROCESS_EVENT: EventKind = EventKind(Cow::Borrowed("sandbox_process_event"));

    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, other: &Self) -> bool {
        canonical_event_kind(self.as_str()) == canonical_event_kind(other.as_str())
    }
}

fn canonical_event_kind(kind: &str) -> &str {
    match kind {
        "conversation_created" => "thread_created",
        "conversation_updated" => "thread_updated",
        "conversation_deleted" => "thread_deleted",
        "conversation_forked" => "thread_forked",
        kind => kind,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventQueryDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetEventsResult {
    pub events: Vec<Event>,
    pub cursor: Option<EventId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddEventsRequest {
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub data: Vec<EventData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AddEventsResult {
    pub event_ids: Vec<EventId>,
    pub latest_event_id: EventId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkThreadRequest {
    pub up_to_inclusive: Option<EventId>,
    pub slug: Option<String>,
    pub name: Option<String>,
}

/// Compatibility name for [`ForkThreadRequest`].
pub type ForkConversationRequest = ForkThreadRequest;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageRecord {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cached_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_creation_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_reasoning_tokens: Option<i64>,
    /// Cost in USD, computed at call time from the price table baked into
    /// this binary. `None` if the model is not in the table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Time to first token (streaming only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Wall-clock duration from request start to end of response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    #[serde(alias = "conversation_id")]
    pub thread_id: ThreadId,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub created_at: DateTimeUtc,
    pub data: EventData,
}

impl Event {
    pub fn conversation_id(&self) -> ConversationId {
        self.thread_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    #[serde(alias = "conversation_created")]
    ThreadCreated {
        slug: String,
        name: String,
    },
    #[serde(alias = "conversation_updated")]
    ThreadUpdated {
        slug: Option<String>,
        name: Option<String>,
    },
    #[serde(alias = "conversation_deleted")]
    ThreadDeleted,
    #[serde(alias = "conversation_forked")]
    ThreadForked {
        #[serde(alias = "source_conversation_id")]
        source_thread_id: ThreadId,
        up_to_inclusive: Option<EventId>,
    },
    SessionStarted,
    SessionEnded,
    TurnStarted,
    TurnEnded,
    Messages {
        messages: Vec<Message>,
        response_id: Option<ResponseId>,
        // Boxed to keep `EventData` small: `UsageRecord` is ~170 bytes and
        // most events don't carry it, so inlining it would bloat every
        // variant (and every enum that embeds `EventData`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Box<UsageRecord>>,
    },
    ToolRequested {
        tool_call_id: ToolCallId,
        response_id: Option<ResponseId>,
        request: ToolRequest,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        #[serde(default)]
        result: ToolResult,
    },
    LinguaStreamChunk {
        chunk: UniversalStreamChunk,
    },
    Error {
        message: String,
    },
    ArtifactWritten {
        artifact_id: ArtifactId,
        path: String,
        version: u64,
    },
    SandboxCreated {
        sandbox_id: SandboxId,
        #[serde(default)]
        name: Option<String>,
        provider: SandboxProvider,
        image: String,
        default_workdir: String,
        file_system_mounts: Vec<FileSystemMount>,
        #[serde(default)]
        durable_file_systems: Vec<DurableFileSystem>,
        enable_networking: bool,
        idle_seconds: u64,
    },
    SandboxStarted {
        sandbox_id: SandboxId,
        snapshot_id: Option<SnapshotId>,
    },
    SandboxStopped {
        sandbox_id: SandboxId,
    },
    SandboxAttached {
        sandbox_id: SandboxId,
        attachment: SandboxAttachment,
        default_workdir: String,
    },
    /// This owner now uses `sandbox_id`, which may belong to a parent scope.
    ///
    /// Recorded rather than kept as configuration so the log answers which
    /// sandbox any given turn ran in. Selection reads the most recent one, so
    /// a rebind is an append rather than an overwrite.
    SandboxSelected {
        /// None unpins, so selection falls back to configuration again.
        sandbox_id: Option<SandboxId>,
    },
    SandboxDetached {
        sandbox_id: SandboxId,
        attachment: SandboxAttachment,
    },
    SandboxSnapshotted {
        sandbox_id: SandboxId,
        snapshot_id: SnapshotId,
    },
    SandboxProcessStarted {
        sandbox_id: SandboxId,
        process_id: SandboxProcessId,
        #[serde(default)]
        name: Option<String>,
        command: Vec<String>,
        cwd: Option<String>,
        mode: SandboxProcessMode,
        stdin: SandboxProcessStdin,
        output: SandboxProcessOutput,
        lifecycle: SandboxProcessLifecycle,
        status: SandboxProcessStatus,
        provider_state: Option<Value>,
    },
    SandboxProcessStateUpdated {
        sandbox_id: SandboxId,
        process_id: SandboxProcessId,
        status: SandboxProcessStatus,
        provider_state: Option<Value>,
    },
    SandboxProcessEvent {
        sandbox_id: SandboxId,
        process_id: SandboxProcessId,
        event: SandboxProcessEvent,
    },
    Custom {
        event_type: String,
        payload: Value,
    },
}

impl EventData {
    /// Tag identifying this event's variant. Source of truth for the
    /// `EventQuery::types` filter on `get_events`.
    pub fn kind(&self) -> EventKind {
        match self {
            Self::ThreadCreated { .. } => EventKind::THREAD_CREATED,
            Self::ThreadUpdated { .. } => EventKind::THREAD_UPDATED,
            Self::ThreadDeleted => EventKind::THREAD_DELETED,
            Self::ThreadForked { .. } => EventKind::THREAD_FORKED,
            Self::SessionStarted => EventKind::SESSION_STARTED,
            Self::SessionEnded => EventKind::SESSION_ENDED,
            Self::TurnStarted => EventKind::TURN_STARTED,
            Self::TurnEnded => EventKind::TURN_ENDED,
            Self::Messages { .. } => EventKind::MESSAGES,
            Self::ToolRequested { .. } => EventKind::TOOL_REQUESTED,
            Self::ToolResult { .. } => EventKind::TOOL_RESULT,
            Self::LinguaStreamChunk { .. } => EventKind::LINGUA_STREAM_CHUNK,
            Self::Error { .. } => EventKind::ERROR,
            Self::ArtifactWritten { .. } => EventKind::ARTIFACT_WRITTEN,
            Self::SandboxCreated { .. } => EventKind::SANDBOX_CREATED,
            Self::SandboxStarted { .. } => EventKind::SANDBOX_STARTED,
            Self::SandboxStopped { .. } => EventKind::SANDBOX_STOPPED,
            Self::SandboxAttached { .. } => EventKind::SANDBOX_ATTACHED,
            Self::SandboxDetached { .. } => EventKind::SANDBOX_DETACHED,
            Self::SandboxSelected { .. } => EventKind::SANDBOX_SELECTED,
            Self::SandboxSnapshotted { .. } => EventKind::SANDBOX_SNAPSHOTTED,
            Self::SandboxProcessStarted { .. } => EventKind::SANDBOX_PROCESS_STARTED,
            Self::SandboxProcessStateUpdated { .. } => EventKind::SANDBOX_PROCESS_STATE_UPDATED,
            Self::SandboxProcessEvent { .. } => EventKind::SANDBOX_PROCESS_EVENT,
            Self::Custom { event_type, .. } => EventKind::custom(event_type.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolRequest {
    pub function_name: String,
    pub arguments: ToolArguments,
    /// Namespace the tool lives in, for providers with namespaced tools (e.g.
    /// the OpenAI Responses API requires function_call items to be replayed
    /// with their namespace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactVersion {
    pub artifact_id: ArtifactId,
    pub path: String,
    pub version: u64,
    pub created_at: DateTimeUtc,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    #[serde(flatten)]
    pub version: ArtifactVersion,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteArtifactRequest {
    pub path: String,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadArtifactRequest {
    pub artifact_id: ArtifactId,
    pub version: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum FileSystemMountMode {
    #[serde(rename = "ro")]
    ReadOnly,
    #[serde(rename = "rw")]
    ReadWrite,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSystemMount {
    pub host_path: String,
    pub mount_path: String,
    pub mode: FileSystemMountMode,
    pub internal: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct DurableFileSystem {
    pub name: String,
    pub mount_path: String,
    pub mode: FileSystemMountMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxRecord {
    pub id: SandboxId,
    pub name: Option<String>,
    pub provider: SandboxProvider,
    pub image: String,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateSandboxRequest {
    #[serde(default)]
    pub name: Option<String>,
    pub provider: SandboxProvider,
    pub image: String,
    pub default_workdir: Option<String>,
    pub file_system_mounts: Option<Vec<FileSystemMount>>,
    pub durable_file_systems: Option<Vec<DurableFileSystem>>,
    pub enable_networking: Option<bool>,
    pub idle_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkSandboxRequest {
    pub source_id: SandboxId,
    pub sandbox: CreateSandboxRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestoreSandboxRequest {
    pub snapshot_id: SnapshotId,
    pub sandbox: CreateSandboxRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachSandboxRequest {
    pub attachment: SandboxAttachment,
    pub default_workdir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SandboxAttachment {
    DockerContainer { container_id: String },
}

impl SandboxAttachment {
    pub fn provider(&self) -> SandboxProvider {
        match self {
            Self::DockerContainer { .. } => SandboxProvider::Docker,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct SandboxProvider(Cow<'static, str>);

#[allow(non_upper_case_globals)]
impl SandboxProvider {
    pub const Daytona: Self = Self::from_static("daytona");
    pub const E2b: Self = Self::from_static("e2b");
    pub const Sprites: Self = Self::from_static("sprites");
    pub const Vercel: Self = Self::from_static("vercel");
    pub const AwsAgentCore: Self = Self::from_static("aws_agentcore");
    pub const AppleContainer: Self = Self::from_static("apple_container");
    pub const Docker: Self = Self::from_static("docker");
    pub const Firecracker: Self = Self::from_static("firecracker");
    pub const LocalProcess: Self = Self::from_static("local_process");

    pub const fn from_static(provider: &'static str) -> Self {
        Self(Cow::Borrowed(provider))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl Display for SandboxProvider {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SandboxProvider {
    type Err = crate::Error;

    fn from_str(value: &str) -> Result<Self> {
        Ok(Self(Cow::Owned(value.to_string())))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartSandboxRequest {
    pub id: SandboxId,
    pub snapshot_id: SnapshotId,
    pub idle_seconds: Option<u64>,
    // If unspecified, starts sandbox where it was last run. If specified, will attempt to
    // start the sandbox on the specified provider, if supported. If successful, the
    // sandbox will start there going forward.
    #[serde(default)]
    pub provider: Option<SandboxProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunInSandboxRequest {
    pub id: SandboxId,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartSandboxProcessRequest {
    pub sandbox_id: SandboxId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub mode: SandboxProcessMode,
    #[serde(default)]
    pub stdin: SandboxProcessStdin,
    #[serde(default)]
    pub output: SandboxProcessOutput,
    #[serde(default)]
    pub lifecycle: SandboxProcessLifecycle,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProcessMode {
    #[default]
    Exec,
    Pty,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProcessStdin {
    None,
    #[default]
    Open,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProcessOutput {
    Buffered,
    #[default]
    Stream,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProcessLifecycle {
    #[default]
    Attached,
    Detached,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxProcessRecord {
    pub id: SandboxProcessId,
    pub sandbox_id: SandboxId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub status: SandboxProcessStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SandboxProcessStatus {
    Running,
    Exited { exit_code: i32 },
    Failed { message: String },
    Cancelled,
}

impl SandboxProcessStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxProcessEventQuery {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
    pub after: Option<u64>,
    pub limit: Option<u32>,
    pub follow: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetSandboxProcessEventsResult {
    pub events: Vec<SandboxProcessEvent>,
    pub cursor: Option<u64>,
    pub status: SandboxProcessStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SandboxProcessEvent {
    Stdout { cursor: u64, data: Vec<u8> },
    Stderr { cursor: u64, data: Vec<u8> },
    Exit { cursor: u64, exit_code: i32 },
    Error { cursor: u64, message: String },
    Cancelled { cursor: u64 },
}

impl SandboxProcessEvent {
    pub fn cursor(&self) -> u64 {
        match self {
            Self::Stdout { cursor, .. }
            | Self::Stderr { cursor, .. }
            | Self::Exit { cursor, .. }
            | Self::Error { cursor, .. }
            | Self::Cancelled { cursor } => *cursor,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteSandboxProcessInputRequest {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloseSandboxProcessInputRequest {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaitSandboxProcessRequest {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelSandboxProcessRequest {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
    pub signal: Option<String>,
}

#[async_trait]
pub trait SandboxProcess: Send {
    fn into_parts(self: Box<Self>) -> SandboxProcessParts;
}

pub struct SandboxProcessParts {
    pub stdout: BoxAsyncRead,
    pub stderr: BoxAsyncRead,
    pub stdin: BoxAsyncWrite,
    pub wait: BoxFuture<'static, Result<i32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BindingRecord {
    pub id: BindingId,
    pub r#type: BindingType,
    pub name: String,
    pub created_at: DateTimeUtc,
    pub binding: Binding,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BindingType {
    Env,
    Mcp,
    Llm,
    Sandbox,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretMetadata {
    pub id: SecretId,
    pub r#type: SecretType,
    pub name: String,
    pub created_at: DateTimeUtc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PutSecretRequest {
    pub name: String,
    pub secret: Secret,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretType {
    Key,
    Oauth,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Binding {
    Env {
        name: String,
        env_var: String,
        secret_id: SecretId,
    },
    Mcp {
        name: String,
        server_url: String,
        secret_id: Option<SecretId>,
    },
    Llm {
        name: String,
        model: String,
        base_url: Option<String>,
        secret_id: Option<SecretId>,
    },
    /// How to reach a remote sandbox provider.
    Sandbox {
        name: String,
        config: SandboxProviderConfig,
    },
}

/// Per-provider sandbox config for a `Binding::Sandbox`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SandboxProviderConfig {
    Docker {
        #[serde(default = "crate::sandbox_provider::default_docker_image")]
        default_image: String,
    },
    Firecracker {
        #[serde(default = "crate::sandbox_provider::default_firecracker_image")]
        default_image: String,
    },
    Daytona {
        /// Secret-store id of the API key.
        api_key_secret_id: SecretId,
        /// Daytona `target` region (e.g. `us` / `eu`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        organization_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_url: Option<String>,
        #[serde(default = "crate::sandbox_provider::default_daytona_image")]
        default_image: String,
    },
    Vercel {
        /// Secret-store id of the Vercel API/access token.
        api_token_secret_id: SecretId,
        team_id: String,
        project_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_url: Option<String>,
        #[serde(default = "crate::sandbox_provider::default_vercel_image")]
        default_image: String,
    },
    E2b {
        api_key_secret_id: SecretId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_url: Option<String>,
        #[serde(default = "default_e2b_template")]
        default_image: String,
    },
    Sprites {
        token_secret_id: SecretId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_url: Option<String>,
        /// Sprite HTTP URL auth mode: `sprite` (default) or `public`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url_auth: Option<String>,
        /// Organization slug when the token can access multiple orgs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        organization: Option<String>,
        /// Extra Sprites labels on create (exo resume labels are added automatically).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        labels: Vec<String>,
    },
    #[serde(rename = "aws_agentcore", alias = "aws-agentcore")]
    AwsAgentCore {
        runtime_arn: String,
        region: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        qualifier: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_storage_mount_path: Option<String>,
        #[serde(default = "crate::sandbox_provider::default_aws_agentcore_image")]
        default_image: String,
    },
}

pub fn default_e2b_template() -> String {
    "base".to_string()
}

impl SandboxProviderConfig {
    pub fn provider(&self) -> SandboxProvider {
        match self {
            Self::Daytona { .. } => SandboxProvider::Daytona,
            Self::E2b { .. } => SandboxProvider::E2b,
            Self::Sprites { .. } => SandboxProvider::Sprites,
            Self::Vercel { .. } => SandboxProvider::Vercel,
            Self::Docker { .. } => SandboxProvider::Docker,
            Self::Firecracker { .. } => SandboxProvider::Firecracker,
            Self::AwsAgentCore { .. } => SandboxProvider::AwsAgentCore,
        }
    }

    /// The binding's configured default base image / E2B template id.
    pub fn default_image(&self) -> Option<&str> {
        match self {
            Self::Daytona { default_image, .. }
            | Self::Vercel { default_image, .. }
            | Self::Docker { default_image, .. }
            | Self::Firecracker { default_image, .. }
            | Self::E2b { default_image, .. }
            | Self::AwsAgentCore { default_image, .. } => Some(default_image),
            Self::Sprites { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Secret {
    Key {
        value: String,
    },
    Oauth {
        access_token: String,
        refresh_token: Option<String>,
    },
}

pub type AgentId = Uuid7;
pub type ThreadId = Uuid7;
/// Compatibility name for [`ThreadId`].
pub type ConversationId = ThreadId;
pub type SessionId = Uuid7;
pub type TurnId = Uuid7;
pub type EventId = Uuid7;
pub type ResponseId = Uuid7;
pub type ToolCallId = String;
pub type ArtifactId = Uuid7;
pub type SandboxId = String;
pub type SandboxProcessId = String;
pub type SnapshotId = Uuid7;
pub type BindingId = Uuid7;
pub type SecretId = Uuid7;
pub type DateTimeUtc = DateTime<Utc>;
pub type ToolResult = Value;
pub type ToolArguments = Map<String, Value>;
pub type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send + Unpin>>;
pub type BoxAsyncWrite = Pin<Box<dyn AsyncWrite + Send + Unpin>>;
pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;

crate::impl_has_uuid7_id!(AgentRecord, id);
crate::impl_has_uuid7_id!(ThreadRecord, id);
crate::impl_has_uuid7_id!(TurnRecord, id);
crate::impl_has_uuid7_id!(Event, id);
crate::impl_has_uuid7_id!(BindingRecord, id);
crate::impl_has_uuid7_id!(SecretMetadata, id);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_event_types_as_snake_case() {
        let event = EventData::SessionStarted;
        let value = serde_json::to_value(event).expect("event should serialize");
        assert_eq!(
            value.get("type").and_then(Value::as_str),
            Some("session_started")
        );
    }

    #[test]
    fn thread_event_kinds_match_legacy_conversation_filters() {
        assert_eq!(EventKind::THREAD_CREATED.as_str(), "thread_created");
        assert!(EventKind::THREAD_CREATED.matches(&EventKind::CONVERSATION_CREATED));
        assert!(EventKind::THREAD_UPDATED.matches(&EventKind::CONVERSATION_UPDATED));
        assert!(EventKind::THREAD_DELETED.matches(&EventKind::CONVERSATION_DELETED));
        assert!(EventKind::THREAD_FORKED.matches(&EventKind::CONVERSATION_FORKED));
    }

    #[test]
    fn thread_events_read_legacy_schema_and_write_canonical_schema() {
        let event_id = Uuid7::now();
        let thread_id = Uuid7::now();
        let event = Event {
            id: event_id,
            thread_id,
            session_id: None,
            turn_id: None,
            created_at: event_id.timestamp().expect("uuid7 timestamp"),
            data: EventData::ThreadCreated {
                slug: "thread".to_string(),
                name: "Thread".to_string(),
            },
        };
        let mut value = serde_json::to_value(event).expect("thread event should serialize");
        assert_eq!(value["thread_id"], serde_json::json!(thread_id));
        assert!(value.get("conversation_id").is_none());
        assert_eq!(value["data"]["type"], "thread_created");

        let object = value.as_object_mut().expect("event should be an object");
        let serialized_thread_id = object.remove("thread_id").expect("thread id should exist");
        object.insert("conversation_id".to_string(), serialized_thread_id);
        value["data"]["type"] = Value::String("conversation_created".to_string());
        let event: Event = serde_json::from_value(value).expect("legacy event should deserialize");
        assert_eq!(event.thread_id, thread_id);
        assert!(matches!(event.data, EventData::ThreadCreated { .. }));
    }

    #[test]
    fn thread_fork_event_reads_legacy_source_id() {
        let source_thread_id = Uuid7::now();
        let event: EventData = serde_json::from_value(serde_json::json!({
            "type": "conversation_forked",
            "source_conversation_id": source_thread_id,
            "up_to_inclusive": null,
        }))
        .expect("legacy fork event should deserialize");
        assert!(matches!(
            event,
            EventData::ThreadForked {
                source_thread_id: actual,
                up_to_inclusive: None,
            } if actual == source_thread_id
        ));

        let value = serde_json::to_value(EventData::ThreadForked {
            source_thread_id,
            up_to_inclusive: None,
        })
        .expect("thread fork event should serialize");
        assert_eq!(value["type"], "thread_forked");
        assert_eq!(
            value["source_thread_id"],
            serde_json::json!(source_thread_id)
        );
        assert!(value.get("source_conversation_id").is_none());
    }

    #[test]
    fn messages_event_deserializes_without_usage_field() {
        // Older logs predate per-message cost tracking and have no `usage`
        // field on Messages events; serde(default) must keep them readable.
        let json = serde_json::json!({
            "type": "messages",
            "messages": [],
            "response_id": null,
        });
        let event: EventData = serde_json::from_value(json).expect("legacy event should parse");
        match event {
            EventData::Messages { usage, .. } => assert!(usage.is_none()),
            _ => panic!("expected Messages variant"),
        }
    }

    #[test]
    fn tool_result_event_deserializes_without_result_field() {
        let json = serde_json::json!({
            "type": "tool_result",
            "tool_call_id": "call-1",
        });
        let event: EventData =
            serde_json::from_value(json).expect("tool result without result field should parse");
        match event {
            EventData::ToolResult { result, .. } => assert!(result.is_null()),
            _ => panic!("expected ToolResult variant"),
        }
    }

    #[test]
    fn messages_event_serializes_usage_when_present() {
        let event = EventData::Messages {
            messages: vec![],
            response_id: None,
            usage: Some(Box::new(UsageRecord {
                model: "claude-sonnet-4-6".to_string(),
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                cost_usd: Some(0.001),
                ..Default::default()
            })),
        };
        let value = serde_json::to_value(&event).expect("event should serialize");
        let usage = value.get("usage").expect("usage field present");
        assert_eq!(
            usage.get("model").and_then(Value::as_str),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            usage.get("prompt_tokens").and_then(Value::as_i64),
            Some(100)
        );
        assert!((usage.get("cost_usd").and_then(Value::as_f64).unwrap() - 0.001).abs() < 1e-9);
        // Round-trip back through the legacy-event parser
        let parsed: EventData = serde_json::from_value(value).expect("round-trip parse");
        assert!(matches!(parsed, EventData::Messages { usage: Some(_), .. }));
    }

    #[test]
    fn serializes_mount_modes_as_ro_rw() {
        let ro =
            serde_json::to_value(FileSystemMountMode::ReadOnly).expect("mode should serialize");
        let rw =
            serde_json::to_value(FileSystemMountMode::ReadWrite).expect("mode should serialize");
        assert_eq!(ro, Value::String("ro".to_string()));
        assert_eq!(rw, Value::String("rw".to_string()));
    }

    #[test]
    fn preserves_sandbox_provider_names() {
        assert_eq!(
            "apple_container".parse::<SandboxProvider>().unwrap(),
            SandboxProvider::AppleContainer
        );
        assert_eq!(
            "apple-container"
                .parse::<SandboxProvider>()
                .unwrap()
                .as_str(),
            "apple-container"
        );
        assert_eq!(
            "local".parse::<SandboxProvider>().unwrap().as_str(),
            "local"
        );
        assert_eq!(
            "Bad Provider".parse::<SandboxProvider>().unwrap().as_str(),
            "Bad Provider"
        );
        assert_eq!(
            SandboxProvider::AppleContainer.to_string(),
            "apple_container"
        );
        assert_eq!(SandboxProvider::Vercel.to_string(), "vercel");
        assert_eq!(SandboxProvider::AwsAgentCore.to_string(), "aws_agentcore");
        assert_eq!(SandboxProvider::Firecracker.to_string(), "firecracker");
        assert_eq!(SandboxProvider::LocalProcess.to_string(), "local_process");
        assert_eq!(
            serde_json::to_value(SandboxProvider::AppleContainer).unwrap(),
            Value::String("apple_container".to_string())
        );
        assert_eq!(
            serde_json::from_value::<SandboxProvider>(Value::String("smolvm".to_string()))
                .unwrap()
                .as_str(),
            "smolvm"
        );
    }
}

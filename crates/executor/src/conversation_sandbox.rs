use anyhow::bail;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use crate::{AgentConfig, ConversationConfig};
use exoharness::{
    ConversationHandle, CreateSandboxRequest, DEFAULT_SANDBOX_IMAGE, EventData, EventKind,
    EventQuery, EventQueryDirection, FileSystemMount, FileSystemMountMode, Result, SandboxProvider,
};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversationSandboxInfo {
    pub(crate) id: String,
    pub(crate) provider: SandboxProvider,
    pub(crate) image: String,
    pub(crate) default_workdir: String,
    pub(crate) file_system_mounts: Vec<FileSystemMount>,
    pub(crate) durable_file_systems: Vec<exoharness::DurableFileSystem>,
    pub(crate) enable_networking: bool,
    pub(crate) idle_seconds: u64,
}

impl ConversationSandboxInfo {
    pub(crate) fn matches_spec(&self, spec: &ConversationSandboxSpec) -> bool {
        self.provider == spec.provider
            && self.image == spec.image
            && self.default_workdir == spec.default_workdir
            && self.file_system_mounts == spec.file_system_mounts
            && self.durable_file_systems == spec.durable_file_systems
            && self.enable_networking == spec.enable_networking
            && self.idle_seconds == spec.idle_seconds
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversationSandboxSpec {
    pub(crate) provider: SandboxProvider,
    pub(crate) image: String,
    pub(crate) default_workdir: String,
    pub(crate) file_system_mounts: Vec<FileSystemMount>,
    pub(crate) durable_file_systems: Vec<exoharness::DurableFileSystem>,
    pub(crate) enable_networking: bool,
    pub(crate) idle_seconds: u64,
}

pub(crate) async fn ensure_conversation_sandbox(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> Result<String> {
    let sandbox_lock = conversation_sandbox_lock(&conversation.record().id.to_string());
    let _guard = sandbox_lock.lock().await;
    if let Some(sandbox_id) = resolve_conversation_sandbox_selection(conversation).await? {
        return Ok(sandbox_id);
    }

    let spec = conversation_sandbox_spec(agent_config, config);

    // Of the sandboxes this conversation still has, prefer the most recent one
    // matching the spec derived from configuration.
    for sandbox in conversation_sandboxes(conversation)
        .await?
        .into_iter()
        .rev()
    {
        if sandbox.matches_spec(&spec) {
            return Ok(sandbox.id);
        }
    }

    create_conversation_sandbox(conversation, agent_config, config).await
}

/// The sandbox this conversation is bound to, if any (else none).
pub(crate) async fn resolve_conversation_sandbox_selection(
    conversation: &dyn ConversationHandle,
) -> Result<Option<String>> {
    let Some(selected) = selected_conversation_sandbox(conversation).await? else {
        return Ok(None);
    };
    // Check to make sure the selected sandbox is still active for this conversation.
    let active = conversation_sandbox_candidates(conversation)
        .await?
        .iter()
        .any(|candidate| candidate.id() == selected);
    if !active {
        bail!(
            "conversation {} selects sandbox {selected}, which it no longer has; \
             it was detached or terminated. Run `exo sandbox deselect --agent <agent> \
             --conversation <conversation>` to go back to its configured sandbox",
            conversation.record().slug
        );
    }
    Ok(Some(selected))
}

/// Return the most recent sandbox selection event, if any, for the conversation.
/// Note: deselection is recorded as selection of 'None', which is caught here.
pub(crate) async fn selected_conversation_sandbox(
    conversation: &dyn ConversationHandle,
) -> Result<Option<String>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(1),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_SELECTED]),
        }))
        .await?
        .events;
    Ok(events
        .into_iter()
        .next()
        .and_then(|event| match event.data {
            EventData::SandboxSelected { sandbox_id } => sandbox_id,
            _ => None,
        }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConversationSandboxCandidate {
    Created(ConversationSandboxInfo),
    Attached { id: String },
}

impl ConversationSandboxCandidate {
    fn id(&self) -> &str {
        match self {
            Self::Created(sandbox) => &sandbox.id,
            Self::Attached { id } => id,
        }
    }
}

// Replay sandbox lifecycle events and return active candidates in chronological order.
// Stopped or detached sandboxes are excluded; a later start reactivates a stopped sandbox.
async fn conversation_sandbox_candidates(
    conversation: &dyn ConversationHandle,
) -> Result<Vec<ConversationSandboxCandidate>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![
                EventKind::SANDBOX_CREATED,
                EventKind::SANDBOX_STARTED,
                EventKind::SANDBOX_STOPPED,
                EventKind::SANDBOX_ATTACHED,
                EventKind::SANDBOX_DETACHED,
            ]),
        }))
        .await?
        .events;
    let mut candidates = Vec::new();
    let mut inactive = HashSet::new();
    for event in events {
        match event.data {
            EventData::SandboxCreated {
                sandbox_id,
                provider,
                image,
                default_workdir,
                file_system_mounts,
                durable_file_systems,
                enable_networking,
                idle_seconds,
                ..
            } => {
                candidates.push(ConversationSandboxCandidate::Created(
                    ConversationSandboxInfo {
                        id: sandbox_id,
                        provider,
                        image,
                        default_workdir,
                        file_system_mounts,
                        durable_file_systems,
                        enable_networking,
                        idle_seconds,
                    },
                ));
            }
            EventData::SandboxAttached { sandbox_id, .. } => {
                candidates.push(ConversationSandboxCandidate::Attached { id: sandbox_id });
            }
            EventData::SandboxStarted { sandbox_id, .. } => {
                inactive.remove(&sandbox_id);
            }
            EventData::SandboxStopped { sandbox_id }
            | EventData::SandboxDetached { sandbox_id, .. } => {
                inactive.insert(sandbox_id);
            }
            _ => {}
        }
    }
    candidates.retain(|candidate| !inactive.contains(candidate.id()));
    Ok(candidates)
}

pub(crate) async fn create_conversation_sandbox(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> Result<String> {
    let spec = conversation_sandbox_spec(agent_config, config);
    conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: spec.provider,
            image: spec.image,
            default_workdir: Some(spec.default_workdir),
            file_system_mounts: Some(spec.file_system_mounts),
            durable_file_systems: Some(spec.durable_file_systems),
            enable_networking: Some(spec.enable_networking),
            idle_seconds: Some(spec.idle_seconds),
        })
        .await
}

pub(crate) async fn conversation_sandboxes(
    conversation: &dyn ConversationHandle,
) -> Result<Vec<ConversationSandboxInfo>> {
    Ok(conversation_sandbox_candidates(conversation)
        .await?
        .into_iter()
        .filter_map(|candidate| match candidate {
            ConversationSandboxCandidate::Created(sandbox) => Some(sandbox),
            ConversationSandboxCandidate::Attached { .. } => None,
        })
        .collect())
}

// The agent-scoped sandbox is shared by every conversation, so its spec must
// not depend on which conversation asks for it: it is derived from the agent
// config alone.
pub(crate) fn agent_sandbox_spec(agent_config: &AgentConfig) -> ConversationSandboxSpec {
    ConversationSandboxSpec {
        provider: agent_config.sandbox.provider.clone(),
        image: agent_config
            .sandbox
            .image
            .clone()
            .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string()),
        default_workdir: agent_config
            .sandbox
            .mounts
            .first()
            .map(|mount| mount.mount_path.clone())
            .unwrap_or_else(|| "/".to_string()),
        file_system_mounts: normalize_mounts(&agent_config.sandbox.mounts),
        durable_file_systems: Vec::new(),
        enable_networking: agent_config.sandbox.enable_networking,
        idle_seconds: 300,
    }
}

pub(crate) fn conversation_sandbox_spec(
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> ConversationSandboxSpec {
    ConversationSandboxSpec {
        provider: config.effective_sandbox_provider(agent_config),
        image: config
            .effective_sandbox_image(agent_config)
            .map(str::to_string)
            .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string()),
        default_workdir: config
            .mounts
            .first()
            .map(|mount| mount.mount_path.clone())
            .or_else(|| {
                config
                    .durable_file_systems
                    .first()
                    .map(|file_system| file_system.mount_path.clone())
            })
            .unwrap_or_else(|| "/".to_string()),
        file_system_mounts: normalize_mounts(&config.mounts),
        durable_file_systems: config.durable_file_systems.clone(),
        enable_networking: agent_config.sandbox.enable_networking,
        idle_seconds: 300,
    }
}

fn normalize_mounts(mounts: &[FileSystemMount]) -> Vec<FileSystemMount> {
    mounts
        .iter()
        .map(|mount| FileSystemMount {
            host_path: mount.host_path.clone(),
            mount_path: mount.mount_path.clone(),
            mode: match mount.mode {
                FileSystemMountMode::ReadOnly => FileSystemMountMode::ReadOnly,
                FileSystemMountMode::ReadWrite => FileSystemMountMode::ReadWrite,
            },
            internal: Some(mount.internal.unwrap_or(false)),
        })
        .collect()
}

fn conversation_sandbox_lock(conversation_id: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .expect("conversation sandbox lock registry poisoned");
    Arc::clone(
        locks
            .entry(conversation_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

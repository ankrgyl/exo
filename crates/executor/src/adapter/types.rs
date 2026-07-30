use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use exoharness::Uuid7;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterSource {
    #[serde(alias = "built_in")]
    Library,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterLifecycleState {
    #[default]
    Starting,
    Running,
    Disabled,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdapterConfig {
    pub adapter_type: String,
    pub worker_command: Vec<String>,
    #[serde(default)]
    pub initialization: Value,
    #[serde(default)]
    pub state_dir: Option<String>,
    #[serde(default)]
    pub secret_env: Vec<WorkerSecretEnvVar>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkerSecretEnvVar {
    pub env: String,
    pub secret_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdapterAttachment {
    pub kind: AdapterAttachmentKind,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub sandbox_path: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterAttachmentKind {
    Image,
    Video,
    Audio,
    Document,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterRecord {
    pub id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub name: String,
    pub source: AdapterSource,
    pub enabled: bool,
    #[serde(default)]
    pub lifecycle_state: AdapterLifecycleState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub config: AdapterConfig,
    #[serde(default)]
    pub last_connected_at_ms: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewAdapter {
    pub agent_id: String,
    pub conversation_id: String,
    pub name: String,
    pub source: AdapterSource,
    pub config: AdapterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterEventRecord {
    pub id: String,
    pub adapter_id: String,
    pub event_type: AdapterEventType,
    pub created_at_ms: u64,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterOutboundMessageRecord {
    pub id: String,
    pub adapter_id: String,
    pub created_at_ms: u64,
    pub text: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AdapterAttachment>,
    #[serde(default)]
    pub status: AdapterDeliveryStatus,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub completed_at_ms: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterDeliveryStatus {
    #[default]
    Queued,
    InFlight,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterEventType {
    Connected,
    Disconnected,
    Inbound,
    Outbound,
    Error,
    /// Worker lifecycle notices (send progress, reconnect attempts, ...) that
    /// are neither traffic nor errors.
    Lifecycle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterTargetConversationRecord {
    pub adapter_id: String,
    pub target: String,
    pub conversation_id: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterInboundMessageRecord {
    pub adapter_id: String,
    pub target: String,
    pub message_id: String,
    pub first_seen_at_ms: u64,
}

impl AdapterTargetConversationRecord {
    pub fn new(
        adapter_id: String,
        target: String,
        conversation_id: String,
        now_ms: u64,
    ) -> Result<Self> {
        Ok(Self {
            adapter_id: non_empty("adapterId", adapter_id)?,
            target: non_empty("target", target)?,
            conversation_id: non_empty("conversationId", conversation_id)?,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
    }
}

impl AdapterRecord {
    pub fn new(request: NewAdapter, now_ms: u64) -> Result<Self> {
        validate_adapter_name(&request.name)?;
        request.config.validate()?;
        Ok(Self {
            id: Uuid7::now().to_string(),
            agent_id: non_empty("agentId", request.agent_id)?,
            conversation_id: non_empty("conversationId", request.conversation_id)?,
            name: request.name,
            source: request.source,
            enabled: true,
            lifecycle_state: AdapterLifecycleState::Starting,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            config: request.config,
            last_connected_at_ms: None,
            last_error: None,
        })
    }
}

impl AdapterConfig {
    pub fn validate(&self) -> Result<()> {
        non_empty_ref("adapterType", &self.adapter_type)?;
        if self.worker_command.is_empty() {
            bail!("worker adapter workerCommand must not be empty");
        }
        for arg in &self.worker_command {
            non_empty_ref("workerCommand item", arg)?;
        }
        if let Some(state_dir) = &self.state_dir {
            non_empty_ref("stateDir", state_dir)?;
        }
        for secret in &self.secret_env {
            non_empty_ref("secretEnv env", &secret.env)?;
            non_empty_ref("secretEnv secretId", &secret.secret_id)?;
        }
        Ok(())
    }
}

impl AdapterEventRecord {
    pub fn new(
        adapter_id: String,
        event_type: AdapterEventType,
        summary: String,
        now_ms: u64,
    ) -> Result<Self> {
        Ok(Self {
            id: Uuid7::now().to_string(),
            adapter_id: non_empty("adapterId", adapter_id)?,
            event_type,
            created_at_ms: now_ms,
            summary,
        })
    }
}

impl AdapterOutboundMessageRecord {
    pub fn new(
        adapter_id: String,
        text: String,
        target: Option<String>,
        attachments: Vec<AdapterAttachment>,
        now_ms: u64,
    ) -> Result<Self> {
        const MAX_TEXT_BYTES: usize = 64 * 1024;
        const MAX_TARGET_BYTES: usize = 4 * 1024;
        const MAX_ATTACHMENTS: usize = 8;
        if text.len() > MAX_TEXT_BYTES {
            bail!("adapter message text exceeds {MAX_TEXT_BYTES} bytes");
        }
        if let Some(target) = &target {
            non_empty_ref("target", target)?;
            if target.len() > MAX_TARGET_BYTES {
                bail!("adapter message target exceeds {MAX_TARGET_BYTES} bytes");
            }
        }
        if attachments.len() > MAX_ATTACHMENTS {
            bail!("adapter message has more than {MAX_ATTACHMENTS} attachments");
        }
        for attachment in &attachments {
            attachment.validate()?;
        }
        Ok(Self {
            id: Uuid7::now().to_string(),
            adapter_id: non_empty("adapterId", adapter_id)?,
            created_at_ms: now_ms,
            text: non_empty("text", text)?,
            target,
            attachments,
            status: AdapterDeliveryStatus::Queued,
            attempt: 0,
            updated_at_ms: now_ms,
            completed_at_ms: None,
            last_error: None,
        })
    }
}

impl AdapterAttachment {
    pub fn validate(&self) -> Result<()> {
        let source_count = usize::from(self.path.is_some())
            + usize::from(self.url.is_some())
            + usize::from(self.data.is_some())
            + usize::from(self.sandbox_path.is_some());
        if source_count != 1 {
            bail!("attachment must specify exactly one of path, url, data, or sandboxPath");
        }
        if let Some(path) = &self.path {
            non_empty_ref("attachment path", path)?;
        }
        if let Some(url) = &self.url {
            non_empty_ref("attachment url", url)?;
            if !url.starts_with("https://") {
                bail!("attachment url must use https");
            }
        }
        if let Some(data) = &self.data {
            non_empty_ref("attachment data", data)?;
        }
        if let Some(sandbox_path) = &self.sandbox_path {
            non_empty_ref("attachment sandboxPath", sandbox_path)?;
        }
        if let Some(mime_type) = &self.mime_type {
            non_empty_ref("attachment mimeType", mime_type)?;
        }
        if let Some(file_name) = &self.file_name {
            non_empty_ref("attachment fileName", file_name)?;
        }
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

fn validate_adapter_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("adapter name must not be empty");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        bail!("adapter name may only contain letters, numbers, '-' and '_'");
    }
    Ok(())
}

fn non_empty(field: &str, value: String) -> Result<String> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(value)
}

fn non_empty_ref(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

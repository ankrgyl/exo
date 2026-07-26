use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use exoharness::{
    AgentHandle, ConversationHandle, Result, RunInSandboxRequest, SandboxProcess, ToolRequest,
    ToolResult, Uuid7,
};
use futures::{StreamExt, io::AsyncReadExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::fs;

use super::runtime::send_adapter_message_with_handles;
use super::store::AdapterStore;
use super::types::{
    AdapterAttachment, AdapterConfig, AdapterEventType, AdapterSource, NewAdapter,
    WorkerSecretEnvVar,
};
use crate::agent_sandbox::ensure_agent_sandbox;
use crate::conversation_sandbox::ensure_conversation_sandbox;
use crate::{AgentConfig, ConversationConfig, SandboxScope, effective_sandbox_scope};

const DEFAULT_EVENT_LIMIT: usize = 50;
const MAX_EVENT_LIMIT: usize = 200;
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_ATTACHMENT_BASE64_BYTES: usize = MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 + 4;
const ATTACHMENT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ATTACHMENT_STDERR_BYTES: usize = 64 * 1024;
const DEFAULT_EXOCHAT_BASE_URL: &str = "https://exoharness.ai";

#[derive(Debug, Clone)]
pub struct AdapterCreationOptions {
    worker_root: PathBuf,
    default_irc_nick_prefix: String,
    default_irc_username: String,
}

impl AdapterCreationOptions {
    pub fn new(worker_root: impl Into<PathBuf>) -> Self {
        Self {
            worker_root: worker_root.into(),
            default_irc_nick_prefix: "exo".to_string(),
            default_irc_username: "exo".to_string(),
        }
    }

    fn worker_command(&self, adapter_type: &str) -> Vec<String> {
        worker_command(self.worker_root.join(adapter_type).join("worker.ts"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateAdapterArguments {
    name: String,
    source: AdapterSource,
    config: AdapterCreationConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversationScopedArguments {
    include_disabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdapterIdArguments {
    adapter_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAdapterEventsArguments {
    adapter_id: String,
    event_type: Option<AdapterEventType>,
    since_ms: Option<u64>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendAdapterMessageArguments {
    adapter_id: String,
    text: String,
    target: Option<String>,
    #[serde(default)]
    attachments: Option<Vec<AdapterAttachment>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AdapterCreationConfig {
    Irc(IrcAdapterCreationConfig),
    Whatsapp(WhatsappAdapterCreationConfig),
    Signal(SignalAdapterCreationConfig),
    Discord(DiscordAdapterCreationConfig),
    Slack(SlackAdapterCreationConfig),
    Exochat(ExochatAdapterCreationConfig),
    AgentCli(AgentCliAdapterCreationConfig),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IrcAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: IrcAdapterType,
    server: String,
    port: u16,
    tls: bool,
    nick: String,
    username: String,
    realname: String,
    channel: String,
    password_secret_id: Option<String>,
    trigger: IrcTrigger,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WhatsappAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: WhatsappAdapterType,
    auth_dir: Option<String>,
    link_method: Option<WhatsappLinkMethod>,
    phone_number: Option<String>,
    trigger: ChatTrigger,
    allowed_chats: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignalAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: SignalAdapterType,
    account: Option<String>,
    device_name: Option<String>,
    config_dir: Option<String>,
    trigger: ChatTrigger,
    allowed_contacts: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiscordAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: DiscordAdapterType,
    bot_token_secret_id: String,
    default_channel_id: Option<String>,
    trigger: DiscordTrigger,
    allowed_channels: Option<Vec<String>>,
    allow_bots: bool,
    /// Enable voice: join voice channels and hold a spoken conversation. Needs
    /// an OpenAI secret (see `openai_secret_id`) for STT/TTS.
    #[serde(default)]
    voice: bool,
    /// Secret id holding the OpenAI API key for voice STT/TTS. Defaults to
    /// `openai` when voice is enabled and no id is given.
    #[serde(default)]
    openai_secret_id: Option<String>,
    #[serde(default)]
    conversation_scope: Option<AdapterConversationScope>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SlackAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: SlackAdapterType,
    bot_token_secret_id: String,
    signing_secret_id: String,
    port: u16,
    path: String,
    default_channel_id: Option<String>,
    trigger: SlackTrigger,
    allowed_channels: Option<Vec<String>>,
    allow_bots: bool,
    thread_replies: bool,
    progress_mode: SlackProgressMode,
    #[serde(default)]
    conversation_scope: Option<AdapterConversationScope>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExochatAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: ExochatAdapterType,
    base_url: Option<String>,
    channel_id: Option<String>,
    secret: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentCliAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: AgentCliAdapterType,
    socket_path: Option<String>,
    mount_root: String,
    mount_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum IrcAdapterType {
    Irc,
}

#[derive(Debug, Deserialize)]
enum AgentCliAdapterType {
    #[serde(rename = "agent-cli")]
    AgentCli,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WhatsappAdapterType {
    Whatsapp,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WhatsappLinkMethod {
    Qr,
    PairingCode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SignalAdapterType {
    Signal,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DiscordAdapterType {
    Discord,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SlackAdapterType {
    Slack,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExochatAdapterType {
    Exochat,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IrcTrigger {
    Mention,
    AllMessages,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChatTrigger {
    AllMessages,
    ContactsOnly,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DiscordTrigger {
    AllMessages,
    MentionsOnly,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SlackTrigger {
    AllMessages,
    MentionsOnly,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SlackProgressMode {
    Update,
    Stream,
    Off,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AdapterConversationScope {
    Adapter,
    Target,
}

impl AdapterConversationScope {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Adapter => "adapter",
            Self::Target => "target",
        }
    }
}

fn default_irc_nick(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() % 10_000)
        .unwrap_or(0);
    format!("{prefix}{suffix:04}")
}

impl AdapterCreationConfig {
    fn adapter_type(&self) -> &'static str {
        match self {
            Self::Irc(_) => "irc",
            Self::Whatsapp(_) => "whatsapp",
            Self::Signal(_) => "signal",
            Self::Discord(_) => "discord",
            Self::Slack(_) => "slack",
            Self::Exochat(_) => "exochat",
            Self::AgentCli(_) => "agent-cli",
        }
    }

    fn into_adapter_config(
        self,
        source: AdapterSource,
        options: &AdapterCreationOptions,
    ) -> Result<AdapterConfig> {
        match self {
            Self::Irc(config) => {
                require_source(source, AdapterSource::BuiltIn, "irc")?;
                Ok(AdapterConfig {
                    adapter_type: "irc".to_string(),
                    worker_command: options.worker_command("irc"),
                    initialization: serde_json::json!({
                        "server": config.server,
                        "port": config.port,
                        "tls": config.tls,
                        "nick": if config.nick.trim().is_empty() { default_irc_nick(&options.default_irc_nick_prefix) } else { config.nick },
                        "username": if config.username.trim().is_empty() { options.default_irc_username.clone() } else { config.username },
                        "realname": config.realname,
                        "channel": config.channel,
                        "trigger": config.trigger.as_str(),
                    }),
                    state_dir: None,
                    secret_env: config
                        .password_secret_id
                        .map(|secret_id| {
                            vec![WorkerSecretEnvVar {
                                env: "EXO_IRC_PASSWORD".to_string(),
                                secret_id,
                            }]
                        })
                        .unwrap_or_default(),
                })
            }
            Self::Whatsapp(config) => {
                require_source(source, AdapterSource::Library, "whatsapp")?;
                if matches!(config.link_method, Some(WhatsappLinkMethod::PairingCode))
                    && config
                        .phone_number
                        .as_deref()
                        .is_none_or(|phone_number| phone_number.trim().is_empty())
                {
                    bail!("whatsapp pairing-code linkMethod requires phoneNumber");
                }
                Ok(AdapterConfig {
                    adapter_type: "whatsapp".to_string(),
                    worker_command: options.worker_command("whatsapp"),
                    initialization: serde_json::json!({
                        "authDir": config.auth_dir,
                        "linkMethod": config.link_method.map(|method| method.as_str()),
                        "phoneNumber": config.phone_number,
                        "trigger": config.trigger.as_str(),
                        "allowedChats": config.allowed_chats,
                    }),
                    state_dir: None,
                    secret_env: Vec::new(),
                })
            }
            Self::Signal(config) => {
                require_source(source, AdapterSource::Library, "signal")?;
                Ok(AdapterConfig {
                    adapter_type: "signal".to_string(),
                    worker_command: options.worker_command("signal"),
                    initialization: serde_json::json!({
                        "account": config.account,
                        "deviceName": config.device_name,
                        "configDir": config.config_dir,
                        "trigger": config.trigger.as_str(),
                        "allowedContacts": config.allowed_contacts,
                    }),
                    state_dir: None,
                    secret_env: Vec::new(),
                })
            }
            Self::Discord(config) => {
                require_source(source, AdapterSource::Library, "discord")?;
                // Voice STT/TTS run in the worker against the OpenAI secret;
                // bind it only when voice is on so text-only adapters need no
                // OpenAI key.
                let mut secret_env = vec![WorkerSecretEnvVar {
                    env: "EXO_DISCORD_BOT_TOKEN".to_string(),
                    secret_id: config.bot_token_secret_id,
                }];
                if config.voice {
                    secret_env.push(WorkerSecretEnvVar {
                        env: "OPENAI_API_KEY".to_string(),
                        secret_id: config
                            .openai_secret_id
                            .unwrap_or_else(|| "openai".to_string()),
                    });
                }
                Ok(AdapterConfig {
                    adapter_type: "discord".to_string(),
                    worker_command: options.worker_command("discord"),
                    initialization: serde_json::json!({
                        "tokenEnv": "EXO_DISCORD_BOT_TOKEN",
                        "defaultChannelId": config.default_channel_id,
                        "trigger": config.trigger.as_str(),
                        "allowedChannels": config.allowed_channels,
                        "allowBots": config.allow_bots,
                        "voice": config.voice,
                        "conversationScope": config
                            .conversation_scope
                            .map(|scope| scope.as_str())
                            .unwrap_or("adapter"),
                    }),
                    state_dir: None,
                    secret_env,
                })
            }
            Self::Slack(config) => {
                require_source(source, AdapterSource::Library, "slack")?;
                if !config.path.starts_with('/') {
                    bail!("slack path must start with /");
                }
                Ok(AdapterConfig {
                    adapter_type: "slack".to_string(),
                    worker_command: options.worker_command("slack"),
                    initialization: serde_json::json!({
                        "botTokenEnv": "EXO_SLACK_BOT_TOKEN",
                        "signingSecretEnv": "EXO_SLACK_SIGNING_SECRET",
                        "port": config.port,
                        "path": config.path,
                        "defaultChannelId": config.default_channel_id,
                        "trigger": config.trigger.as_str(),
                        "allowedChannels": config.allowed_channels,
                        "allowBots": config.allow_bots,
                        "threadReplies": config.thread_replies,
                        "progressMode": config.progress_mode.as_str(),
                        "conversationScope": config
                            .conversation_scope
                            .map(|scope| scope.as_str())
                            .unwrap_or("target"),
                    }),
                    state_dir: None,
                    secret_env: vec![
                        WorkerSecretEnvVar {
                            env: "EXO_SLACK_BOT_TOKEN".to_string(),
                            secret_id: config.bot_token_secret_id,
                        },
                        WorkerSecretEnvVar {
                            env: "EXO_SLACK_SIGNING_SECRET".to_string(),
                            secret_id: config.signing_secret_id,
                        },
                    ],
                })
            }
            Self::Exochat(config) => {
                require_source(source, AdapterSource::Library, "exochat")?;
                let channel_id = config.channel_id.filter(|value| !value.trim().is_empty());
                let secret = config.secret.filter(|value| !value.trim().is_empty());
                if channel_id.is_some() != secret.is_some() {
                    bail!("exochat channelId and secret must either both be set or both be null");
                }
                let base_url = exochat_base_url(config.base_url)?;
                Ok(AdapterConfig {
                    adapter_type: "exochat".to_string(),
                    worker_command: options.worker_command("exochat"),
                    initialization: serde_json::json!({
                        "baseUrl": base_url,
                        "channelId": channel_id.unwrap_or_else(|| Uuid7::now().to_string()),
                        "secret": secret.unwrap_or_else(exochat_secret),
                    }),
                    state_dir: None,
                    secret_env: Vec::new(),
                })
            }
            Self::AgentCli(config) => {
                require_source(source, AdapterSource::BuiltIn, "agent-cli")?;
                if !config.mount_root.starts_with('/') {
                    bail!("agent-cli mountRoot must be an absolute host path");
                }
                let mount_path = config
                    .mount_path
                    .unwrap_or_else(|| "/agent-cli".to_string());
                if !mount_path.starts_with('/') {
                    bail!("agent-cli mountPath must be an absolute sandbox path");
                }
                Ok(AdapterConfig {
                    adapter_type: "agent-cli".to_string(),
                    worker_command: options.worker_command("agent-cli"),
                    initialization: serde_json::json!({
                        "socketPath": config.socket_path,
                        "mountRoot": config.mount_root,
                        "mountPath": mount_path,
                    }),
                    state_dir: None,
                    secret_env: Vec::new(),
                })
            }
        }
    }
}

impl IrcTrigger {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::AllMessages => "all_messages",
        }
    }
}

impl ChatTrigger {
    fn as_str(&self) -> &'static str {
        match self {
            Self::AllMessages => "all_messages",
            Self::ContactsOnly => "contacts_only",
        }
    }
}

impl WhatsappLinkMethod {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Qr => "qr",
            Self::PairingCode => "pairing-code",
        }
    }
}

impl DiscordTrigger {
    fn as_str(&self) -> &'static str {
        match self {
            Self::AllMessages => "all_messages",
            Self::MentionsOnly => "mentions_only",
        }
    }
}

impl SlackTrigger {
    fn as_str(&self) -> &'static str {
        match self {
            Self::AllMessages => "all_messages",
            Self::MentionsOnly => "mentions_only",
        }
    }
}

impl SlackProgressMode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Stream => "stream",
            Self::Off => "off",
        }
    }
}

fn require_source(
    actual: AdapterSource,
    expected: AdapterSource,
    adapter_type: &str,
) -> Result<()> {
    if actual != expected {
        bail!("{adapter_type} adapters must use source {expected:?}");
    }
    Ok(())
}

fn exochat_base_url(value: Option<String>) -> Result<String> {
    let value = value
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("EXO_CHAT_BASE_URL").ok())
        .unwrap_or_else(|| DEFAULT_EXOCHAT_BASE_URL.to_string());
    let value = value.trim().trim_end_matches('/').to_string();
    if !value.starts_with("https://") && !value.starts_with("http://") {
        bail!("exochat baseUrl must start with http:// or https://");
    }
    Ok(value)
}

fn exochat_secret() -> String {
    URL_SAFE_NO_PAD.encode(format!("{}:{}", Uuid7::now(), Uuid7::now()))
}

fn worker_command(path: PathBuf) -> Vec<String> {
    vec![
        "pnpm".to_string(),
        "tsx".to_string(),
        path.to_string_lossy().into_owned(),
    ]
}

pub async fn execute_create_adapter_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    options: &AdapterCreationOptions,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<CreateAdapterArguments>(Value::Object(request.arguments.clone()))?;
    let adapter_type = args.config.adapter_type();
    let config = args.config.into_adapter_config(args.source, options)?;
    let adapter = store
        .create_adapter(NewAdapter {
            agent_id: agent.record().id.to_string(),
            conversation_id: conversation.record().id.to_string(),
            name: args.name,
            source: args.source,
            config,
        })
        .await?;
    if adapter.config.adapter_type != adapter_type {
        bail!("created adapter type mismatch");
    }
    Ok(serde_json::json!({
        "ok": true,
        "adapter": adapter_tool_result(adapter),
    }))
}

pub async fn execute_list_adapters_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ConversationScopedArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let adapters = store
        .list_adapters_for_conversation(
            &agent.record().id.to_string(),
            &conversation.record().id.to_string(),
            args.include_disabled.unwrap_or(false),
        )
        .await?;
    let adapters = adapters
        .into_iter()
        .map(adapter_tool_result)
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "ok": true,
        "adapters": adapters,
    }))
}

fn adapter_tool_result(adapter: super::types::AdapterRecord) -> serde_json::Value {
    let mut value = serde_json::to_value(&adapter).expect("adapter record serializes");
    if adapter.config.adapter_type == "exochat"
        && let Some(chat_url) = exochat_chat_url(&adapter.config)
        && let serde_json::Value::Object(object) = &mut value
    {
        object.insert("chatUrl".to_string(), serde_json::Value::String(chat_url));
    }
    value
}

fn exochat_chat_url(config: &AdapterConfig) -> Option<String> {
    let init = config.initialization.as_object()?;
    let base_url = init.get("baseUrl")?.as_str()?.trim_end_matches('/');
    let channel_id = init.get("channelId")?.as_str()?;
    let secret = init.get("secret")?.as_str()?;
    Some(format!(
        "{base_url}/chat?role=user&c={channel_id}#k={secret}"
    ))
}

pub async fn execute_list_adapter_events_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ListAdapterEventsArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != agent.record().id.to_string()
        || adapter.conversation_id != conversation.record().id.to_string()
    {
        return Ok(not_found());
    }
    let limit = args
        .limit
        .unwrap_or(DEFAULT_EVENT_LIMIT)
        .clamp(1, MAX_EVENT_LIMIT);
    let events = store
        .list_events(&args.adapter_id, args.event_type, args.since_ms, limit)
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "adapterName": adapter.name,
        "events": events,
    }))
}

pub async fn execute_disable_adapter_tool(
    conversation: &dyn ConversationHandle,
    agent: &dyn AgentHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<AdapterIdArguments>(Value::Object(request.arguments.clone()))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != agent.record().id.to_string()
        || adapter.conversation_id != conversation.record().id.to_string()
    {
        return Ok(not_found());
    }
    store.disable_adapter(&args.adapter_id).await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "disabled": true,
    }))
}

pub async fn execute_delete_adapter_tool(
    conversation: &dyn ConversationHandle,
    agent: &dyn AgentHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<AdapterIdArguments>(Value::Object(request.arguments.clone()))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != agent.record().id.to_string()
        || adapter.conversation_id != conversation.record().id.to_string()
    {
        return Ok(not_found());
    }
    store.delete_adapter(&args.adapter_id).await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "deleted": true,
        "eventsDeleted": true,
    }))
}

pub async fn execute_send_adapter_message_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<SendAdapterMessageArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    let Some(scoped_target) = adapter_access_target(store, agent, conversation, &adapter).await?
    else {
        return Ok(not_found());
    };
    let target = resolve_send_target(
        &adapter.config.adapter_type,
        scoped_target.as_deref(),
        args.target.as_deref(),
    )?;
    let attachments = args.attachments.unwrap_or_default();
    if !attachments.is_empty()
        && adapter.config.adapter_type != "whatsapp"
        && adapter.config.adapter_type != "signal"
        && adapter.config.adapter_type != "discord"
    {
        bail!(
            "adapter {} does not support rich attachments",
            adapter.config.adapter_type
        );
    }
    let attachments = resolve_sandbox_attachments(
        agent,
        conversation,
        agent_config,
        config,
        store,
        &adapter,
        attachments,
    )
    .await?;
    send_adapter_message_with_handles(
        agent,
        conversation,
        store,
        &adapter,
        &args.text,
        target.as_deref(),
        attachments,
    )
    .await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "sent": true,
    }))
}

/// Determine whether `conversation` may send through `adapter`, gathering the
/// target-conversation mappings from the store and delegating the decision to
/// the pure [`classify_adapter_access`]. The outer `Option` is authorization
/// (None = denied); the inner `Option` is the send constraint (None = root
/// conversation, any target allowed; `Some(target)` = scoped, that target only).
async fn adapter_access_target(
    store: &AdapterStore,
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    adapter: &super::types::AdapterRecord,
) -> Result<Option<Option<String>>> {
    let conversation_id = conversation.record().id.to_string();
    let mappings = store.list_target_conversations(&adapter.id).await?;
    Ok(classify_adapter_access(
        &adapter.agent_id,
        &adapter.conversation_id,
        &agent.record().id.to_string(),
        &conversation_id,
        mappings
            .iter()
            .map(|m| (m.conversation_id.as_str(), m.target.as_str())),
    ))
}

/// Pure authorization decision. `None`: this conversation may not use the
/// adapter. `Some(None)`: the adapter's root conversation — may send to any
/// target. `Some(Some(target))`: a target-scoped conversation — may send only
/// to `target`.
fn classify_adapter_access<'a>(
    adapter_agent_id: &str,
    adapter_root_conversation_id: &str,
    agent_id: &str,
    conversation_id: &str,
    target_mappings: impl Iterator<Item = (&'a str, &'a str)>,
) -> Option<Option<String>> {
    if adapter_agent_id != agent_id {
        return None;
    }
    if adapter_root_conversation_id == conversation_id {
        return Some(None);
    }
    for (mapped_conversation, target) in target_mappings {
        if mapped_conversation == conversation_id {
            return Some(Some(target.to_string()));
        }
    }
    None
}

/// Resolve the effective send target, enforcing that a target-scoped
/// conversation may only send to its mapped target (a missing target defaults
/// to it). A root conversation sends to whatever it requested.
fn resolve_send_target(
    adapter_type: &str,
    scoped: Option<&str>,
    requested: Option<&str>,
) -> Result<Option<String>> {
    match scoped {
        Some(scoped) => match requested {
            Some(requested)
                if requested != scoped
                    && !allows_scoped_alternate_target(adapter_type, requested) =>
            {
                bail!("target-scoped conversation may only send to mapped target {scoped}")
            }
            Some(requested) => Ok(Some(requested.to_string())),
            None => Ok(Some(scoped.to_string())),
        },
        None => Ok(requested.map(str::to_string)),
    }
}

fn allows_scoped_alternate_target(adapter_type: &str, requested: &str) -> bool {
    adapter_type == "slack" && requested.starts_with("dm:") && requested.len() > "dm:".len()
}

async fn resolve_sandbox_attachments(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    store: &AdapterStore,
    adapter: &super::types::AdapterRecord,
    attachments: Vec<AdapterAttachment>,
) -> Result<Vec<AdapterAttachment>> {
    let mut resolved = Vec::with_capacity(attachments.len());
    for mut attachment in attachments {
        attachment.validate()?;
        if attachment.path.is_some() {
            bail!("attachment path is host-internal; use sandboxPath, url, or data");
        }
        if let Some(url) = attachment.url.clone() {
            let bytes = download_attachment(&url).await?;
            let path = stage_attachment(store, &adapter.id, &url, &attachment, bytes).await?;
            attachment.path = Some(path.to_string_lossy().into_owned());
            attachment.url = None;
            resolved.push(attachment);
            continue;
        }
        if let Some(data) = attachment.data.clone() {
            let bytes = decode_attachment_data(&data)?;
            if bytes.len() > MAX_ATTACHMENT_BYTES {
                bail!(
                    "attachment data is too large: {} bytes exceeds {} bytes",
                    bytes.len(),
                    MAX_ATTACHMENT_BYTES
                );
            }
            let path =
                stage_attachment(store, &adapter.id, "attachment", &attachment, bytes).await?;
            attachment.path = Some(path.to_string_lossy().into_owned());
            attachment.data = None;
            resolved.push(attachment);
            continue;
        }
        let Some(sandbox_path) = attachment.sandbox_path.clone() else {
            resolved.push(attachment);
            continue;
        };
        let bytes =
            read_sandbox_file(agent, conversation, agent_config, config, &sandbox_path).await?;
        let path = stage_attachment(store, &adapter.id, &sandbox_path, &attachment, bytes).await?;
        attachment.path = Some(path.to_string_lossy().into_owned());
        attachment.sandbox_path = None;
        resolved.push(attachment);
    }
    Ok(resolved)
}

async fn read_sandbox_file(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    sandbox_path: &str,
) -> Result<Vec<u8>> {
    match effective_sandbox_scope(agent_config, config) {
        SandboxScope::Agent => {
            let sandbox = ensure_agent_sandbox(agent, agent_config).await?;
            read_agent_sandbox_file_bytes(agent, sandbox.sandbox_id, sandbox_path).await
        }
        SandboxScope::Conversation => {
            let sandbox_id =
                ensure_conversation_sandbox(conversation, agent_config, config).await?;
            read_sandbox_file_bytes(conversation, sandbox_id, sandbox_path).await
        }
    }
}

pub(crate) async fn download_attachment(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(ATTACHMENT_DOWNLOAD_TIMEOUT)
        .build()
        .context("failed to create adapter attachment download client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download adapter attachment {url}"))?
        .error_for_status()
        .with_context(|| format!("failed to download adapter attachment {url}"))?;
    if let Some(content_length) = response.content_length()
        && content_length > MAX_ATTACHMENT_BYTES as u64
    {
        bail!(
            "attachment is too large: {} bytes exceeds {} bytes",
            content_length,
            MAX_ATTACHMENT_BYTES
        );
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("failed to read adapter attachment {url}"))?;
        if bytes.len() + chunk.len() > MAX_ATTACHMENT_BYTES {
            bail!(
                "attachment is too large: more than {} bytes",
                MAX_ATTACHMENT_BYTES
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn decode_attachment_data(data: &str) -> Result<Vec<u8>> {
    let payload = data
        .strip_prefix("data:")
        .and_then(|_| data.split_once(',').map(|(_, payload)| payload))
        .unwrap_or(data);
    if payload.len() > MAX_ATTACHMENT_BASE64_BYTES {
        bail!(
            "attachment data is too large: base64 payload exceeds {} bytes",
            MAX_ATTACHMENT_BASE64_BYTES
        );
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .context("failed to decode attachment data")
}

async fn read_sandbox_file_bytes(
    conversation: &dyn ConversationHandle,
    sandbox_id: String,
    sandbox_path: &str,
) -> Result<Vec<u8>> {
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "cat -- \"$1\"".to_string(),
                "exo-read-attachment".to_string(),
                sandbox_path.to_string(),
            ],
            env: Default::default(),
        })
        .await?;
    let output = read_process_limited(process, MAX_ATTACHMENT_BYTES).await?;
    if output.exit_code != 0 {
        bail!(
            "failed to read sandbox attachment {}: {}",
            sandbox_path,
            output.stderr.trim()
        );
    }
    if output.truncated {
        bail!(
            "sandbox attachment is too large: exceeds {} bytes",
            MAX_ATTACHMENT_BYTES
        );
    }
    Ok(output.stdout)
}

async fn read_agent_sandbox_file_bytes(
    agent: &dyn AgentHandle,
    sandbox_id: String,
    sandbox_path: &str,
) -> Result<Vec<u8>> {
    let process = agent
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "cat -- \"$1\"".to_string(),
                "exo-read-attachment".to_string(),
                sandbox_path.to_string(),
            ],
            env: Default::default(),
        })
        .await?;
    let output = read_process_limited(process, MAX_ATTACHMENT_BYTES).await?;
    if output.exit_code != 0 {
        bail!(
            "failed to read sandbox attachment {}: {}",
            sandbox_path,
            output.stderr.trim()
        );
    }
    if output.truncated {
        bail!(
            "sandbox attachment is too large: exceeds {} bytes",
            MAX_ATTACHMENT_BYTES
        );
    }
    Ok(output.stdout)
}

struct ProcessOutput {
    stdout: Vec<u8>,
    stderr: String,
    exit_code: i32,
    truncated: bool,
}

async fn read_process_limited(
    process: Box<dyn SandboxProcess>,
    max_stdout_bytes: usize,
) -> Result<ProcessOutput> {
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    drop(parts.stdin);

    let (stdout_result, stderr_result, wait_result) = tokio::join!(
        read_limited(&mut stdout, max_stdout_bytes),
        read_limited(&mut stderr, MAX_ATTACHMENT_STDERR_BYTES),
        parts.wait,
    );
    let (stdout_bytes, stdout_truncated) = stdout_result?;
    let (stderr_bytes, stderr_truncated) = stderr_result?;
    let exit_code = wait_result?;

    Ok(ProcessOutput {
        stdout: stdout_bytes,
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        exit_code,
        truncated: stdout_truncated || stderr_truncated,
    })
}

async fn read_limited<R>(reader: &mut R, max_bytes: usize) -> Result<(Vec<u8>, bool)>
where
    R: futures::io::AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len());
        if read > remaining {
            output.extend_from_slice(&buffer[..remaining]);
            truncated = true;
            continue;
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok((output, truncated))
}

async fn stage_attachment(
    store: &AdapterStore,
    adapter_id: &str,
    sandbox_path: &str,
    attachment: &AdapterAttachment,
    bytes: Vec<u8>,
) -> Result<PathBuf> {
    let media_dir = store.root().join("media").join(adapter_id);
    fs::create_dir_all(&media_dir).await?;
    let file_name = staged_file_name(sandbox_path, attachment);
    let path = media_dir.join(format!("{}-{file_name}", Uuid7::now()));
    fs::write(&path, bytes)
        .await
        .with_context(|| format!("failed to stage adapter attachment {}", path.display()))?;
    Ok(path)
}

fn staged_file_name(sandbox_path: &str, attachment: &AdapterAttachment) -> String {
    let raw = attachment
        .file_name
        .as_deref()
        .or_else(|| {
            Path::new(sandbox_path)
                .file_name()
                .and_then(|name| name.to_str())
        })
        .unwrap_or("attachment");
    let sanitized = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized
    }
}

fn not_found() -> ToolResult {
    serde_json::json!({
        "ok": false,
        "error": "adapter not found for this conversation",
    })
}

#[cfg(test)]
mod tests {
    use exoharness::{
        BasicExoHarness, ExoHarness, NewAgentRequest, NewConversationRequest, SandboxProvider,
        ToolRequest,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::test_support::local_test_config;
    use crate::{AgentConfig, AgentHarnessKind, ConversationConfig};

    fn test_creation_options() -> AdapterCreationOptions {
        AdapterCreationOptions::new("/tmp/exo-adapters")
    }

    #[test]
    fn access_denied_for_other_agent() {
        assert_eq!(
            classify_adapter_access("agent-1", "root", "agent-2", "root", std::iter::empty()),
            None
        );
    }

    #[test]
    fn root_conversation_may_send_to_any_target() {
        // Some(None) => root conversation, no target constraint.
        assert_eq!(
            classify_adapter_access("a", "root", "a", "root", std::iter::empty()),
            Some(None)
        );
    }

    #[test]
    fn scoped_conversation_is_constrained_to_its_mapped_target() {
        let mappings = [("conv-A", "chan-A"), ("conv-B", "chan-B")];
        assert_eq!(
            classify_adapter_access("a", "root", "a", "conv-B", mappings.into_iter()),
            Some(Some("chan-B".to_string()))
        );
        // A conversation with no mapping and not the root is denied.
        assert_eq!(
            classify_adapter_access("a", "root", "a", "conv-Z", mappings.into_iter()),
            None
        );
    }

    #[test]
    fn resolve_send_target_enforces_scope() {
        // Root (unscoped) passes the requested target through, or none.
        assert_eq!(
            resolve_send_target("discord", None, Some("x")).unwrap(),
            Some("x".into())
        );
        assert_eq!(resolve_send_target("discord", None, None).unwrap(), None);
        // Scoped: missing target defaults to the mapped one.
        assert_eq!(
            resolve_send_target("discord", Some("chan-A"), None).unwrap(),
            Some("chan-A".into())
        );
        // Scoped: matching target is allowed.
        assert_eq!(
            resolve_send_target("discord", Some("chan-A"), Some("chan-A")).unwrap(),
            Some("chan-A".into())
        );
        // Scoped: a different target is refused — the key cross-channel guard.
        assert!(resolve_send_target("discord", Some("chan-A"), Some("chan-B")).is_err());
        assert_eq!(
            resolve_send_target("slack", Some("C123:1700000000.000000"), Some("dm:U123")).unwrap(),
            Some("dm:U123".into())
        );
    }

    #[tokio::test]
    async fn create_and_list_adapter_tools_are_conversation_scoped() {
        let tempdir = TempDir::new().unwrap();
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        let agent = exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap();
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap();
        let store = AdapterStore::new(tempdir.path().join("adapters"));
        let create_result = execute_create_adapter_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &test_creation_options(),
            &tool_request(
                "create_adapter",
                serde_json::json!({
                    "agentId": "spoofed-agent",
                    "conversationId": "spoofed-conversation",
                    "name": "irc",
                    "source": "built_in",
                    "config": {
                        "type": "irc",
                        "server": "irc.example.test",
                        "port": 6697,
                        "tls": true,
                        "nick": "exo",
                        "username": "exo",
                        "realname": "Exo",
                        "channel": "#exo",
                        "passwordSecretId": null,
                        "trigger": "mention"
                    }
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(create_result["ok"], true);
        let adapter_id = create_result["adapter"]["id"].as_str().unwrap();
        let adapter = store.get_adapter(adapter_id).await.unwrap().unwrap();
        assert_eq!(adapter.agent_id, agent.record().id.to_string());
        assert_eq!(
            adapter.conversation_id,
            conversation.record().id.to_string()
        );
        assert_eq!(adapter.config.worker_command[0], "pnpm");
        assert_eq!(adapter.config.worker_command[1], "tsx");
        assert!(adapter.config.worker_command[2].ends_with("/tmp/exo-adapters/irc/worker.ts"));

        let list_result = execute_list_adapters_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "list_adapters",
                serde_json::json!({
                    "agentId": "spoofed-agent",
                    "conversationId": "spoofed-conversation",
                    "includeDisabled": false
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(list_result["adapters"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn agent_cli_config_applies_defaults() {
        let config: AdapterCreationConfig = serde_json::from_value(serde_json::json!({
            "type": "agent-cli",
            "socketPath": null,
            "mountRoot": "/Users/me/projects",
            "mountPath": null,
        }))
        .unwrap();
        assert_eq!(config.adapter_type(), "agent-cli");
        let adapter_config = config
            .into_adapter_config(AdapterSource::BuiltIn, &test_creation_options())
            .unwrap();
        assert_eq!(adapter_config.adapter_type, "agent-cli");
        assert!(
            adapter_config.worker_command[2].ends_with("/tmp/exo-adapters/agent-cli/worker.ts")
        );
        assert_eq!(
            adapter_config.initialization,
            serde_json::json!({
                "socketPath": null,
                "mountRoot": "/Users/me/projects",
                "mountPath": "/agent-cli",
            })
        );
        assert!(adapter_config.secret_env.is_empty());
    }

    #[test]
    fn agent_cli_config_rejects_relative_mount_root_and_wrong_source() {
        let parse = |mount_root: &str| -> AdapterCreationConfig {
            serde_json::from_value(serde_json::json!({
                "type": "agent-cli",
                "socketPath": null,
                "mountRoot": mount_root,
                "mountPath": null,
            }))
            .unwrap()
        };
        let error = parse("projects")
            .into_adapter_config(AdapterSource::BuiltIn, &test_creation_options())
            .unwrap_err();
        assert!(error.to_string().contains("absolute host path"));
        let error = parse("/Users/me/projects")
            .into_adapter_config(AdapterSource::Library, &test_creation_options())
            .unwrap_err();
        assert!(error.to_string().contains("source"));
    }

    #[test]
    fn exochat_config_applies_defaults_and_requires_library_source() {
        let parse =
            |channel_id: serde_json::Value, secret: serde_json::Value| -> AdapterCreationConfig {
                serde_json::from_value(serde_json::json!({
                    "type": "exochat",
                    "baseUrl": null,
                    "channelId": channel_id,
                    "secret": secret,
                }))
                .unwrap()
            };
        let adapter_config = parse(serde_json::Value::Null, serde_json::Value::Null)
            .into_adapter_config(AdapterSource::Library, &test_creation_options())
            .unwrap();
        assert_eq!(adapter_config.adapter_type, "exochat");
        assert!(adapter_config.worker_command[2].ends_with("/tmp/exo-adapters/exochat/worker.ts"));
        assert_eq!(
            adapter_config.initialization["baseUrl"],
            serde_json::json!(DEFAULT_EXOCHAT_BASE_URL)
        );
        assert!(
            adapter_config.initialization["channelId"]
                .as_str()
                .is_some()
        );
        assert!(adapter_config.initialization["secret"].as_str().is_some());
        let chat_url = exochat_chat_url(&adapter_config).unwrap();
        assert!(chat_url.starts_with("https://exoharness.ai/chat?role=user&c="));
        assert!(chat_url.contains("#k="));
        assert!(adapter_config.secret_env.is_empty());

        let error = parse(serde_json::Value::Null, serde_json::Value::Null)
            .into_adapter_config(AdapterSource::BuiltIn, &test_creation_options())
            .unwrap_err();
        assert!(error.to_string().contains("source"));

        let error = parse(serde_json::json!("channel"), serde_json::Value::Null)
            .into_adapter_config(AdapterSource::Library, &test_creation_options())
            .unwrap_err();
        assert!(error.to_string().contains("channelId and secret"));
    }

    #[tokio::test]
    async fn exochat_adapter_tool_result_includes_chat_url() {
        let tempdir = TempDir::new().unwrap();
        let store = AdapterStore::new(tempdir.path().join("adapters"));
        let adapter = store
            .create_adapter(NewAdapter {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "exochat".to_string(),
                source: AdapterSource::Library,
                config: AdapterConfig {
                    adapter_type: "exochat".to_string(),
                    worker_command: vec!["pnpm".to_string(), "tsx".to_string()],
                    initialization: serde_json::json!({
                        "baseUrl": "https://chat.example.test",
                        "channelId": "channel-123",
                        "secret": "secret-456",
                    }),
                    state_dir: None,
                    secret_env: Vec::new(),
                },
            })
            .await
            .unwrap();
        let result = adapter_tool_result(adapter);
        assert_eq!(
            result["chatUrl"],
            "https://chat.example.test/chat?role=user&c=channel-123#k=secret-456"
        );
    }

    #[tokio::test]
    async fn list_adapter_events_is_conversation_scoped() {
        let tempdir = TempDir::new().unwrap();
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        let agent = exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap();
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap();
        let store = AdapterStore::new(tempdir.path().join("adapters"));
        let owned = store
            .create_adapter(NewAdapter {
                agent_id: agent.record().id.to_string(),
                conversation_id: conversation.record().id.to_string(),
                name: "discord".to_string(),
                source: AdapterSource::Library,
                config: AdapterConfig {
                    adapter_type: "discord".to_string(),
                    worker_command: vec!["pnpm".to_string(), "tsx".to_string()],
                    initialization: serde_json::json!({}),
                    state_dir: None,
                    secret_env: Vec::new(),
                },
            })
            .await
            .unwrap();
        store
            .record_event(
                owned.id.clone(),
                AdapterEventType::Error,
                "shard error".to_string(),
            )
            .await
            .unwrap();
        let foreign = store
            .create_adapter(NewAdapter {
                agent_id: "other-agent".to_string(),
                conversation_id: "other-conversation".to_string(),
                name: "foreign".to_string(),
                source: AdapterSource::Library,
                config: AdapterConfig {
                    adapter_type: "discord".to_string(),
                    worker_command: vec!["pnpm".to_string(), "tsx".to_string()],
                    initialization: serde_json::json!({}),
                    state_dir: None,
                    secret_env: Vec::new(),
                },
            })
            .await
            .unwrap();

        let result = execute_list_adapter_events_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "list_adapter_events",
                serde_json::json!({
                    "adapterId": owned.id,
                    "eventType": "error",
                    "sinceMs": null,
                    "limit": null
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(result["ok"], true);
        let events = result["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["summary"], "shard error");

        let foreign_result = execute_list_adapter_events_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "list_adapter_events",
                serde_json::json!({
                    "adapterId": foreign.id,
                    "eventType": null,
                    "sinceMs": null,
                    "limit": null
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(foreign_result["ok"], false);
    }

    #[tokio::test]
    async fn create_adapter_rejects_raw_worker_config() {
        let tempdir = TempDir::new().unwrap();
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        let agent = exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap();
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap();
        let store = AdapterStore::new(tempdir.path().join("adapters"));

        let error = execute_create_adapter_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &test_creation_options(),
            &tool_request(
                "create_adapter",
                serde_json::json!({
                    "name": "whatsapp",
                    "source": "library",
                    "config": {
                        "type": "whatsapp",
                        "authDir": null,
                        "trigger": "all_messages",
                        "allowedChats": null,
                        "workerCommand": ["sh", "-c", "cat /etc/passwd"]
                    }
                }),
            ),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("data did not match"));
        assert!(store.list_adapters().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_adapter_message_rejects_host_path_attachments() {
        let tempdir = TempDir::new().unwrap();
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        let agent = exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap();
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap();
        let store = AdapterStore::new(tempdir.path().join("adapters"));
        let adapter = store
            .create_adapter(NewAdapter {
                agent_id: agent.record().id.to_string(),
                conversation_id: conversation.record().id.to_string(),
                name: "whatsapp".to_string(),
                source: AdapterSource::Library,
                config: AdapterConfig {
                    adapter_type: "whatsapp".to_string(),
                    worker_command: vec!["pnpm".to_string(), "tsx".to_string()],
                    initialization: serde_json::json!({}),
                    state_dir: None,
                    secret_env: Vec::new(),
                },
            })
            .await
            .unwrap();

        let error = execute_send_adapter_message_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &test_agent_config(),
            &ConversationConfig::default(),
            &store,
            &tool_request(
                "send_adapter_message",
                serde_json::json!({
                    "adapterId": adapter.id,
                    "text": "hello",
                    "target": "chat",
                    "attachments": [{
                        "kind": "image",
                        "path": "/etc/passwd",
                        "url": null,
                        "data": null,
                        "sandboxPath": null,
                        "mimeType": null,
                        "fileName": null
                    }]
                }),
            ),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("host-internal"));
    }

    fn tool_request(function_name: &str, arguments: serde_json::Value) -> ToolRequest {
        ToolRequest {
            function_name: function_name.to_string(),
            arguments: arguments.as_object().unwrap().clone(),
        }
    }

    fn test_agent_config() -> AgentConfig {
        AgentConfig {
            instructions: Vec::new(),
            harness: AgentHarnessKind::Exo,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox: crate::AgentSandboxConfig {
                image: None,
                provider: SandboxProvider::Docker,
                mounts: Vec::new(),
                enable_networking: false,
                scope: crate::SandboxScope::Agent,
            },
            model: "test-model".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        }
    }
}

mod adapters;
mod env;
#[cfg(test)]
mod env_tests;
#[cfg(test)]
mod mount_tests;
#[cfg(test)]
mod naming_tests;
#[cfg(test)]
mod repl_tests;
#[cfg(test)]
mod secret_tests;
mod tui;

use std::collections::HashMap;
use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use executor::{
    AgentHarnessKind, BasicExoHarness, BasicExoHarnessConfig, BasicHarness, BasicToolRuntime,
    Binding, BraintrustProject, BraintrustRuntimeConfig, BraintrustTracingConfig,
    ConversationModelConfig, CreateAgentRequest, CreateConversationRequest, DaytonaBackendSpec,
    E2bBackendSpec, EventKind, EventQuery, EventQueryDirection, ExoHarness,
    ExoHarnessHttpServeOptions, ExoToolRuntime, FileSystemMount, FileSystemMountMode,
    ForkConversationRequest, HTTP_EXOHARNESS_TRACING_TARGET, Harness, HarnessAgent,
    HarnessConversation, HttpExoHarness, LocalSandboxExoHarness, OPENAI_CHATGPT_PROVIDER_ID,
    OpenAiChatGptOAuthProvider, PutSecretRequest, RlmHarness, SANDBOX_MAIN_MOUNT_DIR,
    SandboxBackendRegistration, SandboxProvider, SandboxProviderConfig, SandboxScope, Secret,
    SecretBackendChoice, SpritesBackendSpec, ToolRequest, ToolRuntime, TypeScriptHarness,
    TypeScriptHarnessConfig, Uuid7, VercelBackendSpec, default_aws_agentcore_image,
    default_daytona_image, default_docker_image, default_e2b_template, default_vercel_image,
    effective_sandbox_scope, load_agent_config, send_conversation_wakeup,
    serve_exoharness_http_listener_with_options,
};
use lingua::Message;
use lingua::universal::{AssistantContent, AssistantContentPart, ToolContentPart, UserContent};
use serde::Deserialize;
use tabwriter::TabWriter;
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::env::CliEnvironment;
use tui::run_chat_repl;

#[derive(Debug, Parser)]
#[command(name = "exo")]
#[command(about = "CLI for exo agents")]
#[command(
    after_help = "Runtime options:\n  --braintrust-api-key <BRAINTRUST_API_KEY>\n  --braintrust-app-url <BRAINTRUST_APP_URL>\n  --braintrust-api-url <BRAINTRUST_API_URL>\n\nThese options are accepted globally, including after subcommands, but are hidden from subcommand help to reduce noise."
)]
struct Cli {
    #[arg(long, global = true, default_value = ".exo")]
    root: PathBuf,
    /// Executor runtime: basic, rlm, typescript, codex, claude-code, cursor, or a TypeScript module path.
    #[arg(long, global = true, value_name = "HARNESS")]
    harness: Option<HarnessSelection>,
    #[arg(long, global = true, value_enum, env = "EXO_SECRET_BACKEND")]
    secret_backend: Option<SecretBackendArg>,
    #[arg(long, global = true, env = "EXO_MASTER_KEY_PATH")]
    master_key_path: Option<PathBuf>,
    #[arg(long, global = true, value_enum, env = "EXO_SANDBOX_BACKEND")]
    sandbox_backend: Option<SandboxBackendArg>,
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
    #[arg(long, global = true)]
    env_file_if_exists: Option<PathBuf>,
    #[arg(long, global = true, env = "BRAINTRUST_API_KEY", hide = true)]
    braintrust_api_key: Option<String>,
    #[arg(long, global = true, env = "BRAINTRUST_APP_URL", hide = true)]
    braintrust_app_url: Option<String>,
    #[arg(long, global = true, env = "BRAINTRUST_API_URL", hide = true)]
    braintrust_api_url: Option<String>,
    #[arg(
        long = "exoharness-url",
        visible_alias = "url",
        global = true,
        env = "EXO_EXOHARNESS_URL"
    )]
    exoharness_url: Option<String>,
    #[arg(
        long = "bearer-env",
        value_name = "ENV_VAR",
        help = "Environment variable whose value is sent as the HTTP bearer token",
        global = true,
        env = "EXO_BEARER_ENV",
        requires = "exoharness_url",
        value_parser = parse_env_var_name
    )]
    bearer_env: Option<String>,
    /// Path to a LiteLLM price JSON for cost tracking (overrides fetch/cache).
    #[arg(long, global = true, env = "EXO_LITELLM_PRICES_PATH")]
    pricing_path: Option<PathBuf>,
    /// URL to fetch the LiteLLM price JSON from (overrides the default source).
    #[arg(long, global = true, env = "EXO_LITELLM_PRICES_URL")]
    pricing_url: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum HarnessKind {
    Basic,
    Rlm,
    #[value(name = "typescript")]
    TypeScript,
    Exo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HarnessSelection {
    Kind(HarnessKind),
    TypeScriptPreset(TypeScriptHarnessPreset),
    TypeScriptModule(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeScriptHarnessPreset {
    Codex,
    ClaudeCode,
    Cursor,
}

impl HarnessSelection {
    fn harness_kind(&self) -> HarnessKind {
        match self {
            Self::Kind(kind) => *kind,
            Self::TypeScriptPreset(_) | Self::TypeScriptModule(_) => HarnessKind::TypeScript,
        }
    }

    fn default_agent_slug(&self) -> Option<String> {
        match self {
            Self::TypeScriptPreset(preset) => Some(preset.agent_slug().to_string()),
            Self::TypeScriptModule(path) => path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(slugify)
                .filter(|slug| !slug.is_empty()),
            Self::Kind(_) => None,
        }
    }

    fn default_sandbox_image(&self) -> Option<&'static str> {
        match self {
            Self::TypeScriptPreset(preset) => preset.sandbox_image(),
            Self::Kind(_) | Self::TypeScriptModule(_) => None,
        }
    }

    fn default_enable_networking(&self) -> bool {
        matches!(self, Self::TypeScriptPreset(_))
    }
}

impl FromStr for HarnessSelection {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "basic" => Ok(Self::Kind(HarnessKind::Basic)),
            "rlm" => Ok(Self::Kind(HarnessKind::Rlm)),
            "typescript" => Ok(Self::Kind(HarnessKind::TypeScript)),
            "exo" => Ok(Self::Kind(HarnessKind::Exo)),
            "codex" => Ok(Self::TypeScriptPreset(TypeScriptHarnessPreset::Codex)),
            "claude-code" => Ok(Self::TypeScriptPreset(TypeScriptHarnessPreset::ClaudeCode)),
            "cursor" | "cursor-sdk" => Ok(Self::TypeScriptPreset(TypeScriptHarnessPreset::Cursor)),
            value if looks_like_typescript_module_path(value) => {
                Ok(Self::TypeScriptModule(PathBuf::from(value)))
            }
            _ => Err(format!(
                "unknown harness `{raw}`; expected basic, rlm, typescript, exo, codex, claude-code, cursor, or a TypeScript module path"
            )),
        }
    }
}

impl TypeScriptHarnessPreset {
    fn agent_slug(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::Cursor => "cursor",
        }
    }

    fn module_path(self) -> &'static Path {
        match self {
            Self::Codex => Path::new("examples/typescript/codex-harness.ts"),
            Self::ClaudeCode => Path::new("examples/typescript/claude-code-harness.ts"),
            Self::Cursor => Path::new("examples/typescript/cursor-sdk-harness.ts"),
        }
    }

    fn sandbox_image(self) -> Option<&'static str> {
        match self {
            Self::Codex => Some("exo-codex-sandbox:latest"),
            Self::ClaudeCode => Some("exo-claude-code-sandbox:latest"),
            Self::Cursor => Some("exo-cursor-sdk-sandbox:latest"),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SecretBackendArg {
    #[value(name = "apple-keychain")]
    AppleKeychain,
    File,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SandboxProviderArg {
    Daytona,
    #[value(name = "e2b")]
    E2b,
    #[value(name = "sprites")]
    Sprites,
    Vercel,
    #[value(name = "aws-agentcore")]
    AwsAgentCore,
    #[value(name = "apple-container")]
    AppleContainer,
    Docker,
    #[value(name = "local-process")]
    LocalProcess,
}

impl From<SandboxProviderArg> for SandboxProvider {
    fn from(value: SandboxProviderArg) -> Self {
        match value {
            SandboxProviderArg::Daytona => Self::Daytona,
            SandboxProviderArg::E2b => Self::E2b,
            SandboxProviderArg::Sprites => Self::Sprites,
            SandboxProviderArg::Vercel => Self::Vercel,
            SandboxProviderArg::AwsAgentCore => Self::AwsAgentCore,
            SandboxProviderArg::AppleContainer => Self::AppleContainer,
            SandboxProviderArg::Docker => Self::Docker,
            SandboxProviderArg::LocalProcess => Self::LocalProcess,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SandboxBackendArg {
    #[value(name = "apple-container")]
    AppleContainer,
    Docker,
    #[value(name = "local-process")]
    LocalProcess,
}

impl From<SandboxBackendArg> for SandboxBackendRegistration {
    fn from(value: SandboxBackendArg) -> Self {
        match value {
            SandboxBackendArg::AppleContainer => Self::apple_container(),
            SandboxBackendArg::Docker => Self::docker(),
            SandboxBackendArg::LocalProcess => Self::local_process(),
        }
    }
}

fn build_exo_config(cli: &Cli) -> Result<BasicExoHarnessConfig> {
    let secret_backend = match cli.secret_backend.unwrap_or_else(default_secret_backend) {
        SecretBackendArg::AppleKeychain => SecretBackendChoice::AppleKeychain,
        SecretBackendArg::File => SecretBackendChoice::File {
            path: cli.master_key_path.clone(),
        },
    };
    let sandbox_backend = cli
        .sandbox_backend
        .map(SandboxBackendRegistration::from)
        .unwrap_or_else(default_sandbox_backend);
    let sandbox_default = sandbox_backend.provider();
    let mut sandbox_backends = default_sandbox_backends();
    if !sandbox_backends
        .iter()
        .any(|backend| backend.provider() == sandbox_default)
    {
        sandbox_backends.push(sandbox_backend);
    }
    Ok(BasicExoHarnessConfig {
        root: cli.root.join("exoharness"),
        secret_backend,
        sandbox_default,
        sandbox_backends,
    })
}

/// Default providers: the OS-local container backend, local processes, and
/// Daytona (offered even with no key set — credentials resolve lazily).
fn default_sandbox_backends() -> Vec<SandboxBackendRegistration> {
    vec![
        default_sandbox_backend(),
        SandboxBackendRegistration::local_process(),
        SandboxBackendRegistration::daytona(DaytonaBackendSpec::default()),
        SandboxBackendRegistration::e2b(E2bBackendSpec::default()),
        SandboxBackendRegistration::sprites(SpritesBackendSpec::default()),
        SandboxBackendRegistration::vercel(VercelBackendSpec::with_conventional_secrets()),
        SandboxBackendRegistration::aws_agentcore(),
    ]
}

fn aws_region_from_arn(resource_arn: &str, expected_service: &str) -> Option<String> {
    let mut parts = resource_arn.split(':');
    let arn = parts.next()?;
    let _partition = parts.next()?;
    let service = parts.next()?;
    let region = parts.next()?;
    if arn == "arn" && service == expected_service && !region.is_empty() {
        return Some(region.to_string());
    }
    None
}

#[cfg(target_os = "macos")]
fn default_secret_backend() -> SecretBackendArg {
    SecretBackendArg::AppleKeychain
}

#[cfg(not(target_os = "macos"))]
fn default_secret_backend() -> SecretBackendArg {
    SecretBackendArg::File
}

#[cfg(target_os = "macos")]
fn default_sandbox_backend() -> SandboxBackendRegistration {
    SandboxBackendRegistration::apple_container()
}

#[cfg(not(target_os = "macos"))]
fn default_sandbox_backend() -> SandboxBackendRegistration {
    SandboxBackendRegistration::docker()
}

#[cfg(target_os = "macos")]
fn default_local_sandbox_provider() -> SandboxProvider {
    SandboxProvider::AppleContainer
}

#[cfg(not(target_os = "macos"))]
fn default_local_sandbox_provider() -> SandboxProvider {
    SandboxProvider::Docker
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EnabledDisabled {
    Enabled,
    Disabled,
}

impl EnabledDisabled {
    fn enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SandboxScopeArg {
    Agent,
    Conversation,
}

impl From<SandboxScopeArg> for SandboxScope {
    fn from(value: SandboxScopeArg) -> Self {
        match value {
            SandboxScopeArg::Agent => SandboxScope::Agent,
            SandboxScopeArg::Conversation => SandboxScope::Conversation,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Manage agents and their executor configuration.
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Manage conversations, mounts, events, and one-shot sends.
    Conversation {
        #[command(subcommand)]
        command: ConversationCommands,
    },
    /// Register and list model bindings.
    Model {
        #[command(subcommand)]
        command: ModelCommands,
    },
    /// Configure and list sandbox provider bindings.
    Provider {
        #[command(subcommand)]
        command: ProviderCommands,
    },
    /// Manage local stored secrets.
    Secret {
        #[command(subcommand)]
        command: SecretCommands,
    },
    /// Log in to and out of OAuth-backed model providers.
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Start an interactive REPL, creating a default agent and conversation when needed.
    Repl {
        /// Model binding to use (defaults to the first registered model).
        #[arg(long)]
        model: Option<String>,
        /// Agent slug to use or create (default: "repl", or the harness preset name).
        #[arg(long)]
        agent: Option<String>,
        /// Conversation slug to use or create (default: a fresh generated slug).
        #[arg(long)]
        conversation: Option<String>,
    },
    Adapters {
        #[command(subcommand)]
        command: adapters::AdapterCommands,
    },
    Serve {
        #[arg(long, default_value = "127.0.0.1:4766")]
        bind: SocketAddr,
        #[arg(short, long, action = ArgAction::Count)]
        verbose: u8,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommands {
    List,
    Create {
        name: String,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        module: Option<PathBuf>,
        #[arg(long = "tool-module")]
        tool_modules: Vec<PathBuf>,
        #[arg(long, value_enum)]
        tool_creation: Option<EnabledDisabled>,
        #[arg(long)]
        sandbox_image: Option<String>,
        #[arg(long, value_enum)]
        sandbox_provider: Option<SandboxProviderArg>,
        #[arg(long, value_enum)]
        sandbox_scope: Option<SandboxScopeArg>,
        #[arg(long, value_enum)]
        networking: Option<EnabledDisabled>,
        #[arg(long)]
        model: String,
        #[arg(long)]
        max_output_tokens: Option<i64>,
        #[arg(long)]
        max_tool_round_trips: Option<u32>,
        #[arg(long)]
        braintrust_org: Option<String>,
        #[arg(long)]
        braintrust_project: Option<String>,
        #[arg(long)]
        braintrust_project_id: Option<String>,
    },
    Update {
        agent: String,
        #[arg(long, value_name = "HARNESS")]
        set_harness: Option<HarnessSelection>,
        #[arg(long)]
        module: Option<PathBuf>,
        #[arg(long)]
        clear_module: bool,
        #[arg(long = "tool-module")]
        tool_modules: Vec<PathBuf>,
        #[arg(long)]
        clear_tool_modules: bool,
        #[arg(long, value_enum)]
        tool_creation: Option<EnabledDisabled>,
        #[arg(long)]
        sandbox_image: Option<String>,
        #[arg(long)]
        clear_sandbox_image: bool,
        #[arg(long, value_enum)]
        sandbox_provider: Option<SandboxProviderArg>,
        #[arg(long, value_enum)]
        sandbox_scope: Option<SandboxScopeArg>,
        #[arg(long, value_enum)]
        networking: Option<EnabledDisabled>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        max_output_tokens: Option<i64>,
        #[arg(long)]
        clear_max_output_tokens: bool,
        #[arg(long)]
        max_tool_round_trips: Option<u32>,
        #[arg(long)]
        clear_max_tool_round_trips: bool,
        #[arg(long)]
        clear_braintrust: bool,
        #[arg(long)]
        braintrust_org: Option<String>,
        #[arg(long)]
        braintrust_project: Option<String>,
        #[arg(long)]
        braintrust_project_id: Option<String>,
    },
    Mount {
        #[command(subcommand)]
        command: AgentMountCommands,
    },
    Show {
        agent: String,
    },
    Delete {
        agent: String,
    },
}

#[derive(Debug, Subcommand)]
enum AgentMountCommands {
    List {
        agent: String,
    },
    Add {
        agent: String,
        host_path: PathBuf,
        mount_path: Option<String>,
        #[arg(long)]
        rw: bool,
        #[arg(long)]
        internal: bool,
    },
    Remove {
        agent: String,
        mount_path: String,
    },
}

#[derive(Debug, Subcommand)]
enum ConversationCommands {
    List {
        agent: String,
    },
    Create {
        agent: String,
        name: Option<String>,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long, value_enum)]
        sandbox_scope: Option<SandboxScopeArg>,
        #[command(flatten)]
        sandbox_runtime: ConversationSandboxRuntimeArgs,
        #[arg(long)]
        repl: bool,
    },
    Fork {
        agent: String,
        conversation: String,
        name: Option<String>,
        #[arg(long)]
        slug: Option<String>,
        #[arg(long)]
        up_to: Option<String>,
        #[arg(long)]
        repl: bool,
    },
    Update {
        agent: String,
        conversation: String,
        #[command(flatten)]
        sandbox_runtime: ConversationSandboxRuntimeUpdateArgs,
        #[arg(long, value_enum)]
        sandbox_scope: Option<SandboxScopeArg>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        max_output_tokens: Option<i64>,
        #[arg(long)]
        clear_max_output_tokens: bool,
        #[arg(long)]
        clear_model_override: bool,
    },
    Mount {
        #[command(subcommand)]
        command: ConversationMountCommands,
    },
    Sandbox {
        #[command(subcommand)]
        command: ConversationSandboxCommands,
    },
    Show {
        agent: String,
        conversation: String,
    },
    Events {
        agent: String,
        conversation: String,
        #[arg(long = "type")]
        types: Vec<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        desc: bool,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        turn_id: Option<String>,
    },
    Send {
        agent: String,
        conversation: String,
        prompt: String,
    },
    Delete {
        agent: String,
        conversation: String,
    },
}

#[derive(Debug, Subcommand)]
enum ConversationSandboxCommands {
    Run {
        agent: String,
        conversation: String,
        command: String,
    },
}

#[derive(Debug, Subcommand)]
enum SecretCommands {
    List,
    #[command(hide = true)]
    Check,
    Set {
        name: String,
        #[arg(long, value_parser = parse_env_var_name)]
        env: Option<String>,
        #[arg(long)]
        value: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommands {
    Login {
        provider: String,
        #[arg(long)]
        name: Option<String>,
    },
    Logout {
        provider: String,
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ModelCommands {
    List,
    Register {
        name: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        secret: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderCommands {
    /// List configured sandbox provider bindings.
    List,
    /// Configure a sandbox provider (writes a Binding::Sandbox).
    Configure(Box<ProviderConfigureArgs>),
}

#[derive(Debug, Args)]
struct ProviderConfigureArgs {
    #[arg(long, value_enum)]
    provider: SandboxProviderArg,
    /// Binding name (default: the provider name).
    #[arg(long)]
    name: Option<String>,
    /// Secret (by name) holding the provider's API key/token. Required for remote providers.
    #[arg(long)]
    secret: Option<String>,
    /// Region/target. Daytona: us | eu | experimental.
    #[arg(long)]
    region: Option<String>,
    /// Daytona organization id, or Sprites organization slug.
    #[arg(long)]
    organization_id: Option<String>,
    #[arg(long)]
    project_id: Option<String>,
    #[arg(long)]
    api_url: Option<String>,
    #[arg(long = "runtime-arn")]
    runtime_arn: Option<String>,
    #[arg(long)]
    qualifier: Option<String>,
    /// AgentCore managed session storage mount path configured on the runtime.
    #[arg(long = "session-storage-mount-path")]
    session_storage_mount_path: Option<String>,
    /// Default base image for sandboxes that don't request one.
    #[arg(long)]
    default_image: Option<String>,
    /// Sprites sprite HTTP URL auth: sprite | public.
    #[arg(long)]
    url_auth: Option<String>,
    /// Extra Sprites labels (repeatable). Exo resume labels are added on create.
    #[arg(long = "label")]
    labels: Vec<String>,
}

#[derive(Debug, Clone, Default, Args)]
struct ConversationSandboxRuntimeArgs {
    #[arg(long)]
    sandbox_image: Option<String>,
    #[arg(long, value_enum)]
    sandbox_provider: Option<SandboxProviderArg>,
    #[arg(long)]
    shell_program: Option<String>,
}

impl ConversationSandboxRuntimeArgs {
    fn validate(&self) -> Result<()> {
        if self
            .sandbox_image
            .as_ref()
            .is_some_and(|image| image.trim().is_empty())
        {
            bail!("sandbox image must not be empty");
        }
        if self
            .shell_program
            .as_ref()
            .is_some_and(|program| program.trim().is_empty())
        {
            bail!("shell program must not be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Args)]
struct ConversationSandboxRuntimeUpdateArgs {
    #[command(flatten)]
    runtime: ConversationSandboxRuntimeArgs,
    #[arg(long)]
    clear_shell_program: bool,
    #[arg(long)]
    clear_sandbox_image: bool,
    #[arg(long)]
    clear_sandbox_provider: bool,
}

impl ConversationSandboxRuntimeUpdateArgs {
    fn apply(self, config: &mut executor::ConversationConfig) -> Result<bool> {
        if self.clear_shell_program && self.runtime.shell_program.is_some() {
            bail!("provide either --clear-shell-program or --shell-program, not both");
        }
        if self.clear_sandbox_image && self.runtime.sandbox_image.is_some() {
            bail!("provide either --clear-sandbox-image or --sandbox-image, not both");
        }
        if self.clear_sandbox_provider && self.runtime.sandbox_provider.is_some() {
            bail!("provide either --clear-sandbox-provider or --sandbox-provider, not both");
        }
        self.runtime.validate()?;

        let mut changed = false;
        if self.clear_shell_program {
            config.shell_program = None;
            changed = true;
        } else if let Some(shell_program) = self.runtime.shell_program {
            config.shell_program = Some(shell_program);
            changed = true;
        }

        if self.clear_sandbox_image {
            config.sandbox_image = None;
            changed = true;
        } else if let Some(sandbox_image) = self.runtime.sandbox_image {
            config.sandbox_image = Some(sandbox_image);
            changed = true;
        }

        if self.clear_sandbox_provider {
            config.sandbox_provider = None;
            changed = true;
        } else if let Some(sandbox_provider) = self.runtime.sandbox_provider {
            config.sandbox_provider = Some(SandboxProvider::from(sandbox_provider));
            changed = true;
        }

        Ok(changed)
    }
}

#[derive(Debug, Subcommand)]
enum ConversationMountCommands {
    List {
        agent: String,
        conversation: String,
    },
    Add {
        agent: String,
        conversation: String,
        host_path: PathBuf,
        mount_path: Option<String>,
        #[arg(long)]
        rw: bool,
        #[arg(long)]
        internal: bool,
    },
    Remove {
        agent: String,
        conversation: String,
        mount_path: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let exo_config = build_exo_config(&cli)?;
    let env = CliEnvironment::load(cli.env_file_if_exists.as_deref(), cli.env_file.as_deref())?;
    let runtime_config = env.braintrust_runtime_config(
        cli.braintrust_api_key,
        cli.braintrust_app_url,
        cli.braintrust_api_url,
    );
    let env_vars = env.into_vars();
    let harness_selection = cli.harness.clone();
    if let Some(config) = serve_config(&cli.command) {
        serve_exoharness_http(&exo_config, config).await?;
        return Ok(());
    }

    let bearer_token = cli
        .bearer_env
        .as_deref()
        .map(|env| env_value_from_arg("--bearer-env", env, &env_vars))
        .transpose()?;
    let default_sandbox_provider = default_local_sandbox_provider();
    let exoharness =
        instantiate_exoharness(&exo_config, cli.exoharness_url.as_deref(), bearer_token).await?;
    let harness_kind = determine_harness_kind(
        exoharness.as_ref(),
        harness_selection.as_ref(),
        &cli.command,
    )
    .await?;
    let pricing = Arc::new(cost::load(cli.pricing_path.clone(), cli.pricing_url.clone()).await);
    let harness = instantiate_harness(
        &cli.root,
        &exo_config,
        exoharness,
        harness_kind,
        runtime_config.clone(),
        env_vars.clone(),
        pricing,
    )
    .await?;

    match cli.command {
        Commands::Adapters { command } => {
            adapters::handle_adapter_command(&cli.root, Arc::clone(&harness), command).await?;
        }
        Commands::Repl {
            model,
            agent,
            conversation,
        } => {
            let agent_slug =
                agent.unwrap_or_else(|| default_repl_agent_slug(harness_selection.as_ref()));
            // Without --conversation, start a fresh session each run (the usual CLI
            // behavior); pass --conversation <slug> to resume or target a specific one.
            let conversation_slug = conversation.unwrap_or_else(generate_fun_slug);

            let agent = match harness.get_agent(&agent_slug).await? {
                Some(agent) => {
                    if let Some(selection) = harness_selection.as_ref() {
                        ensure_agent_matches_harness_selection(agent.as_ref(), selection).await?;
                    }
                    ensure_existing_repl_agent_model(
                        harness.as_ref(),
                        agent.as_ref(),
                        model.clone(),
                    )
                    .await?;
                    agent
                }
                None => {
                    let model = ensure_repl_model(harness.as_ref(), model).await?;
                    let typescript = if matches!(
                        harness_selection.as_ref(),
                        Some(HarnessSelection::Kind(HarnessKind::TypeScript))
                    ) {
                        None
                    } else {
                        build_typescript_harness_config(harness_selection.as_ref(), None, &[])?
                    };
                    if matches!(harness_kind, HarnessKind::TypeScript) && typescript.is_none() {
                        bail!(
                            "repl --harness typescript needs an existing TypeScript agent; use --harness codex, --harness claude-code, --harness cursor, or --harness <module.ts> to create one"
                        );
                    }
                    harness
                        .create_agent(CreateAgentRequest {
                            slug: agent_slug.clone(),
                            name: Some(agent_slug),
                            harness: to_agent_harness_kind(harness_kind),
                            typescript,
                            enable_agent_tool_creation: true,
                            sandbox_image: harness_selection
                                .as_ref()
                                .and_then(HarnessSelection::default_sandbox_image)
                                .map(str::to_string),
                            sandbox_provider: default_sandbox_provider,
                            sandbox_scope: None,
                            enable_networking: harness_selection
                                .as_ref()
                                .is_some_and(HarnessSelection::default_enable_networking),
                            model,
                            max_output_tokens: None,
                            max_tool_round_trips: None,
                            braintrust: None,
                        })
                        .await?
                }
            };

            let conversation = match agent.get_conversation(&conversation_slug).await? {
                Some(conversation) => conversation,
                None => {
                    agent
                        .create_conversation(CreateConversationRequest {
                            slug: Some(conversation_slug.clone()),
                            name: Some(conversation_slug),
                            ..Default::default()
                        })
                        .await?
                }
            };

            run_chat_repl(Arc::clone(&agent), conversation).await?;
        }
        Commands::Agent { command } => match command {
            AgentCommands::List => {
                let agents = harness.list_agents().await?;
                print_table(
                    &["AGENT", "ID", "NAME"],
                    agents
                        .into_iter()
                        .map(|agent| vec![agent.slug, agent.id.to_string(), agent.name])
                        .collect(),
                )?;
            }
            AgentCommands::Create {
                name,
                slug,
                module,
                tool_modules,
                tool_creation,
                sandbox_image,
                sandbox_provider,
                sandbox_scope,
                networking,
                model,
                max_output_tokens,
                max_tool_round_trips,
                braintrust_org,
                braintrust_project,
                braintrust_project_id,
            } => {
                let slug = slug.unwrap_or_else(|| slugify(&name));
                if slug.is_empty() {
                    bail!("agent slug resolved to an empty value");
                }
                if sandbox_image
                    .as_ref()
                    .is_some_and(|image| image.trim().is_empty())
                {
                    bail!("sandbox image must not be empty");
                }
                let agent_harness_kind = to_agent_harness_kind(harness_kind);
                let typescript = build_typescript_harness_config(
                    harness_selection.as_ref(),
                    module.as_deref(),
                    &tool_modules,
                )?;
                let sandbox_image = sandbox_image.or_else(|| {
                    harness_selection
                        .as_ref()
                        .and_then(HarnessSelection::default_sandbox_image)
                        .map(str::to_string)
                });
                let enable_networking =
                    networking.map(EnabledDisabled::enabled).unwrap_or_else(|| {
                        harness_selection
                            .as_ref()
                            .is_some_and(HarnessSelection::default_enable_networking)
                    });
                let agent = harness
                    .create_agent(CreateAgentRequest {
                        slug,
                        name: Some(name),
                        harness: agent_harness_kind,
                        typescript,
                        enable_agent_tool_creation: tool_creation
                            .map(EnabledDisabled::enabled)
                            .unwrap_or(true),
                        sandbox_image,
                        sandbox_provider: sandbox_provider
                            .map(SandboxProvider::from)
                            .unwrap_or(default_sandbox_provider),
                        sandbox_scope: sandbox_scope.map(SandboxScope::from),
                        enable_networking,
                        model,
                        max_output_tokens,
                        max_tool_round_trips,
                        braintrust: build_braintrust_tracing_config(
                            braintrust_org,
                            braintrust_project,
                            braintrust_project_id,
                        )?,
                    })
                    .await?;
                println!(
                    "created agent {} ({})",
                    agent.record().slug,
                    agent.record().id
                );
            }
            AgentCommands::Update {
                agent,
                set_harness,
                module,
                clear_module,
                tool_modules,
                clear_tool_modules,
                tool_creation,
                sandbox_image,
                clear_sandbox_image,
                sandbox_provider,
                sandbox_scope,
                networking,
                model,
                max_output_tokens,
                clear_max_output_tokens,
                max_tool_round_trips,
                clear_max_tool_round_trips,
                clear_braintrust,
                braintrust_org,
                braintrust_project,
                braintrust_project_id,
            } => {
                if clear_module && module.is_some() {
                    bail!("provide either --clear-module or --module, not both");
                }
                if clear_tool_modules && !tool_modules.is_empty() {
                    bail!("provide either --clear-tool-modules or --tool-module, not both");
                }
                if clear_module && !tool_modules.is_empty() {
                    bail!("provide either --clear-module or --tool-module, not both");
                }
                if clear_sandbox_image && sandbox_image.is_some() {
                    bail!("provide either --clear-sandbox-image or --sandbox-image, not both");
                }
                if clear_max_output_tokens && max_output_tokens.is_some() {
                    bail!(
                        "provide either --clear-max-output-tokens or --max-output-tokens, not both"
                    );
                }
                if clear_max_tool_round_trips && max_tool_round_trips.is_some() {
                    bail!(
                        "provide either --clear-max-tool-round-trips or --max-tool-round-trips, not both"
                    );
                }
                if clear_braintrust
                    && (braintrust_org.is_some()
                        || braintrust_project.is_some()
                        || braintrust_project_id.is_some())
                {
                    bail!(
                        "provide either --clear-braintrust or Braintrust project flags, not both"
                    );
                }
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                let mut config = agent.config().await?;
                let mut changed = false;
                if let Some(set_harness) = set_harness.as_ref() {
                    if clear_module {
                        bail!("provide either --set-harness or --clear-module, not both");
                    }
                    let new_harness = to_agent_harness_kind(set_harness.harness_kind());
                    if config.harness != new_harness {
                        config.harness = new_harness;
                        changed = true;
                    }
                    let typescript = build_typescript_harness_config(
                        Some(set_harness),
                        module.as_deref(),
                        &tool_modules,
                    )?;
                    if config.typescript != typescript {
                        config.typescript = typescript;
                        changed = true;
                    }
                }
                if set_harness.is_some() {
                    if clear_tool_modules {
                        bail!("provide either --set-harness or --clear-tool-modules, not both");
                    }
                } else if clear_module {
                    config.typescript = None;
                    changed = true;
                } else if let Some(module) = module.as_deref() {
                    if !matches!(
                        config.harness,
                        AgentHarnessKind::TypeScript | AgentHarnessKind::Exo
                    ) {
                        bail!("--module is only valid with TypeScript or Exo agents");
                    }
                    let existing_tool_modules = config
                        .typescript
                        .as_ref()
                        .map(|typescript| typescript.tool_module_paths.clone())
                        .unwrap_or_default();
                    let tool_module_paths = if tool_modules.is_empty() {
                        existing_tool_modules
                    } else {
                        resolve_typescript_tool_module_paths(&tool_modules)?
                    };
                    let typescript = Some(resolve_typescript_harness_config(
                        module,
                        tool_module_paths,
                    )?);
                    if config.typescript != typescript {
                        config.typescript = typescript;
                        changed = true;
                    }
                } else if clear_tool_modules {
                    if let Some(typescript) = config.typescript.as_mut()
                        && !typescript.tool_module_paths.is_empty()
                    {
                        typescript.tool_module_paths.clear();
                        changed = true;
                    }
                } else if !tool_modules.is_empty() {
                    if !matches!(
                        config.harness,
                        AgentHarnessKind::TypeScript | AgentHarnessKind::Exo
                    ) {
                        bail!("--tool-module is only valid with TypeScript or Exo agents");
                    }
                    let Some(typescript) = config.typescript.as_mut() else {
                        bail!("typescript agents require a module path; pass --module <path>");
                    };
                    let tool_module_paths = resolve_typescript_tool_module_paths(&tool_modules)?;
                    if typescript.tool_module_paths != tool_module_paths {
                        typescript.tool_module_paths = tool_module_paths;
                        changed = true;
                    }
                }
                if let Some(tool_creation) = tool_creation {
                    let enable_agent_tool_creation = tool_creation.enabled();
                    if config.enable_agent_tool_creation != enable_agent_tool_creation {
                        config.enable_agent_tool_creation = enable_agent_tool_creation;
                        changed = true;
                    }
                }
                if clear_sandbox_image {
                    config.sandbox.image = None;
                    changed = true;
                } else if let Some(sandbox_image) = sandbox_image {
                    if sandbox_image.trim().is_empty() {
                        bail!("sandbox image must not be empty");
                    }
                    config.sandbox.image = Some(sandbox_image);
                    changed = true;
                }

                if let Some(sandbox_provider) = sandbox_provider {
                    let sandbox_provider = SandboxProvider::from(sandbox_provider);
                    if config.sandbox.provider != sandbox_provider {
                        config.sandbox.provider = sandbox_provider;
                        changed = true;
                    }
                }

                if let Some(sandbox_scope) = sandbox_scope {
                    let sandbox_scope = SandboxScope::from(sandbox_scope);
                    if config.sandbox.scope != sandbox_scope {
                        config.sandbox.scope = sandbox_scope;
                        changed = true;
                    }
                }

                if let Some(networking) = networking {
                    let enable_networking = networking.enabled();
                    if config.sandbox.enable_networking != enable_networking {
                        config.sandbox.enable_networking = enable_networking;
                        changed = true;
                    }
                }

                if let Some(model) = model {
                    if model.trim().is_empty() {
                        bail!("model must not be empty");
                    }
                    if config.model != model {
                        config.model = model;
                        changed = true;
                    }
                }
                if clear_max_output_tokens {
                    if config.max_output_tokens.is_some() {
                        config.max_output_tokens = None;
                        changed = true;
                    }
                } else if let Some(max_output_tokens) = max_output_tokens
                    && config.max_output_tokens != Some(max_output_tokens)
                {
                    config.max_output_tokens = Some(max_output_tokens);
                    changed = true;
                }
                if clear_max_tool_round_trips {
                    if config.max_tool_round_trips.is_some() {
                        config.max_tool_round_trips = None;
                        changed = true;
                    }
                } else if let Some(max_tool_round_trips) = max_tool_round_trips
                    && config.max_tool_round_trips != Some(max_tool_round_trips)
                {
                    config.max_tool_round_trips = Some(max_tool_round_trips);
                    changed = true;
                }

                if clear_braintrust {
                    config.braintrust = None;
                    changed = true;
                } else {
                    let updated_braintrust = build_braintrust_tracing_config(
                        braintrust_org,
                        braintrust_project,
                        braintrust_project_id,
                    )?;
                    if updated_braintrust.is_none() && !changed {
                        bail!(
                            "no changes provided; pass --set-harness, --module, --tool-module, --clear-tool-modules, --tool-creation, --sandbox-image, --networking, model flags, --clear-braintrust, or Braintrust project flags"
                        );
                    }
                    if updated_braintrust.is_some() {
                        config.braintrust = updated_braintrust;
                        changed = true;
                    }
                }
                if !changed {
                    bail!("no changes provided");
                }
                if matches!(
                    config.harness,
                    AgentHarnessKind::TypeScript | AgentHarnessKind::Exo
                ) && config.typescript.is_none()
                {
                    bail!("TypeScript and Exo agents require a module path; pass --module <path>");
                }
                agent.put_config(config).await?;
                println!("updated agent {}", agent.record().slug);
            }
            AgentCommands::Mount { command } => match command {
                AgentMountCommands::List { agent } => {
                    let agent = must_get_agent(harness.as_ref(), &agent).await?;
                    let config = agent.config().await?;
                    print_mounts(&config.sandbox.mounts);
                }
                AgentMountCommands::Add {
                    agent,
                    host_path,
                    mount_path,
                    rw,
                    internal,
                } => {
                    let agent = must_get_agent(harness.as_ref(), &agent).await?;
                    let canonical_host_path = canonicalize_directory(&host_path)?;

                    let mut config = agent.config().await?;
                    let mount_path = match mount_path {
                        Some(mount_path) => {
                            validate_mount_path(&mount_path)?;
                            mount_path
                        }
                        None => default_mount_path(&canonical_host_path, &config.sandbox.mounts),
                    };
                    let new_mount = FileSystemMount {
                        host_path: canonical_host_path.display().to_string(),
                        mount_path: mount_path.clone(),
                        mode: if rw {
                            FileSystemMountMode::ReadWrite
                        } else {
                            FileSystemMountMode::ReadOnly
                        },
                        internal: Some(internal),
                    };

                    if let Some(existing) = config
                        .sandbox
                        .mounts
                        .iter_mut()
                        .find(|mount| mount.mount_path == mount_path)
                    {
                        *existing = new_mount;
                    } else {
                        config.sandbox.mounts.push(new_mount);
                    }

                    agent.put_config(config).await?;
                    println!(
                        "mounted {} -> {} ({}) for agent {}",
                        canonical_host_path.display(),
                        mount_path,
                        if rw { "rw" } else { "ro" },
                        agent.record().slug
                    );
                }
                AgentMountCommands::Remove { agent, mount_path } => {
                    let agent = must_get_agent(harness.as_ref(), &agent).await?;
                    let mut config = agent.config().await?;
                    let before = config.sandbox.mounts.len();
                    config
                        .sandbox
                        .mounts
                        .retain(|mount| mount.mount_path != mount_path);
                    if config.sandbox.mounts.len() == before {
                        bail!("mount not found: {mount_path}");
                    }
                    agent.put_config(config).await?;
                    println!(
                        "removed mount {} from agent {}",
                        mount_path,
                        agent.record().slug
                    );
                }
            },
            AgentCommands::Show { agent } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                let config = agent.config().await?;
                println!("id: {}", agent.record().id);
                println!("slug: {}", agent.record().slug);
                println!("name: {}", agent.record().name);
                println!("harness: {}", format_harness_kind(config.harness));
                println!(
                    "typescript_module: {}",
                    config
                        .typescript
                        .as_ref()
                        .map(|config| config.module_path.as_str())
                        .unwrap_or("none")
                );
                let tool_module_paths = config
                    .typescript
                    .as_ref()
                    .map(|config| config.tool_module_paths.as_slice())
                    .unwrap_or_default();
                println!("typescript_tool_modules: {}", tool_module_paths.len());
                for tool_module_path in tool_module_paths {
                    println!("  - {}", tool_module_path);
                }
                println!(
                    "tool_creation: {}",
                    if config.enable_agent_tool_creation {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                println!(
                    "sandbox_image: {}",
                    config.sandbox.image.as_deref().unwrap_or("default")
                );
                println!(
                    "sandbox_provider: {}",
                    format_sandbox_provider(config.sandbox.provider)
                );
                println!(
                    "sandbox_scope: {}",
                    match config.sandbox.scope {
                        executor::SandboxScope::Agent => "agent",
                        executor::SandboxScope::Conversation => "conversation",
                    }
                );
                println!("enable_networking: {}", config.sandbox.enable_networking);
                println!("sandbox_mounts:");
                print_mounts(&config.sandbox.mounts);
                println!("model: {}", config.model);
                println!(
                    "max_output_tokens: {}",
                    config
                        .max_output_tokens
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!(
                    "max_tool_round_trips: {}",
                    config
                        .max_tool_round_trips
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!(
                    "braintrust: {}",
                    format_braintrust_tracing_config(config.braintrust.as_ref())
                );
            }
            AgentCommands::Delete { agent } => {
                if !harness.delete_agent(&agent).await? {
                    bail!("agent not found: {agent}");
                }
                println!("deleted agent {}", agent);
            }
        },
        Commands::Conversation { command } => match command {
            ConversationCommands::List { agent } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                let conversations = agent.list_conversations().await?;
                print_table(
                    &["CONVERSATION", "ID", "NAME"],
                    conversations
                        .into_iter()
                        .map(|conversation| {
                            vec![
                                conversation.slug,
                                conversation.id.to_string(),
                                conversation.name,
                            ]
                        })
                        .collect(),
                )?;
            }
            ConversationCommands::Create {
                agent,
                name,
                slug,
                sandbox_scope,
                sandbox_runtime,
                repl,
            } => {
                sandbox_runtime.validate()?;
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                let slug = slug.unwrap_or_else(|| {
                    name.as_deref()
                        .map(slugify)
                        .filter(|slug| !slug.is_empty())
                        .unwrap_or_else(generate_fun_slug)
                });
                if slug.is_empty() {
                    bail!("conversation slug resolved to an empty value");
                }
                let conversation = agent
                    .create_conversation(CreateConversationRequest {
                        slug: Some(slug),
                        name,
                        sandbox_image: sandbox_runtime.sandbox_image,
                        sandbox_provider: sandbox_runtime
                            .sandbox_provider
                            .map(SandboxProvider::from),
                        shell_program: sandbox_runtime.shell_program,
                    })
                    .await?;
                if let Some(sandbox_scope) = sandbox_scope {
                    let mut config = conversation.config().await?;
                    config.sandbox_scope = Some(sandbox_scope.into());
                    conversation.put_config(config).await?;
                }
                println!(
                    "created conversation {} ({})",
                    conversation.record().slug,
                    conversation.record().id
                );
                if repl {
                    run_chat_repl(Arc::clone(&agent), conversation).await?;
                } else {
                    println!(
                        "start chatting with it via `{}`",
                        repl_command(
                            agent.record().slug.as_str(),
                            conversation.record().slug.as_str(),
                        )
                    );
                }
            }
            ConversationCommands::Fork {
                agent,
                conversation,
                name,
                slug,
                up_to,
                repl,
            } => {
                let source = must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                let forked = source
                    .exoharness_handle()
                    .fork(ForkConversationRequest {
                        up_to_inclusive: parse_optional_uuid7(up_to.as_deref(), "up_to")?,
                        slug,
                        name,
                    })
                    .await?;
                println!(
                    "forked conversation {} ({})",
                    forked.record().slug,
                    forked.record().id
                );
                if repl {
                    let agent = must_get_agent(harness.as_ref(), &agent).await?;
                    let conversation = agent
                        .get_conversation(&forked.record().slug)
                        .await?
                        .ok_or_else(|| {
                            anyhow!("forked conversation not found: {}", forked.record().slug)
                        })?;
                    run_chat_repl(agent, conversation).await?;
                } else {
                    println!(
                        "start chatting with it via `{}`",
                        repl_command(agent.as_str(), forked.record().slug.as_str())
                    );
                }
            }
            ConversationCommands::Update {
                agent,
                conversation,
                sandbox_scope,
                sandbox_runtime,
                model,
                max_output_tokens,
                clear_max_output_tokens,
                clear_model_override,
            } => {
                if clear_model_override
                    && (model.is_some() || max_output_tokens.is_some() || clear_max_output_tokens)
                {
                    bail!(
                        "provide either --clear-model-override or model override flags, not both"
                    );
                }
                if clear_max_output_tokens && max_output_tokens.is_some() {
                    bail!(
                        "provide either --clear-max-output-tokens or --max-output-tokens, not both"
                    );
                }

                let agent_handle = must_get_agent(harness.as_ref(), &agent).await?;
                let conversation = agent_handle
                    .get_conversation(&conversation)
                    .await?
                    .ok_or_else(|| anyhow!("conversation not found: {}", conversation))?;
                let mut config = conversation.config().await?;
                let mut changed = sandbox_runtime.apply(&mut config)?;

                if let Some(sandbox_scope) = sandbox_scope {
                    config.sandbox_scope = Some(sandbox_scope.into());
                    changed = true;
                }

                let updated_model_override = if clear_model_override {
                    changed = true;
                    Some(None)
                } else if model.is_some() || max_output_tokens.is_some() || clear_max_output_tokens
                {
                    let agent_config = agent_handle.config().await?;
                    let mut model_override =
                        conversation
                            .model_override()
                            .await?
                            .unwrap_or(ConversationModelConfig {
                                model: agent_config.model,
                                max_output_tokens: agent_config.max_output_tokens,
                            });

                    if let Some(model) = model {
                        if model.trim().is_empty() {
                            bail!("model must not be empty");
                        }
                        model_override.model = model;
                    }
                    if clear_max_output_tokens {
                        model_override.max_output_tokens = None;
                    } else if let Some(max_output_tokens) = max_output_tokens {
                        model_override.max_output_tokens = Some(max_output_tokens);
                    }

                    changed = true;
                    Some(Some(model_override))
                } else {
                    None
                };

                if !changed {
                    bail!("no changes provided");
                }

                conversation.put_config(config).await?;
                if let Some(model_override) = updated_model_override {
                    conversation.put_model_override(model_override).await?;
                }
                println!("updated conversation {}", conversation.record().slug);
            }
            ConversationCommands::Mount { command } => match command {
                ConversationMountCommands::List {
                    agent,
                    conversation,
                } => {
                    let conversation =
                        must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                    let config = conversation.config().await?;
                    print_mounts(&config.mounts);
                }
                ConversationMountCommands::Add {
                    agent,
                    conversation,
                    host_path,
                    mount_path,
                    rw,
                    internal,
                } => {
                    let conversation =
                        must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                    let canonical_host_path = canonicalize_directory(&host_path)?;

                    let mut config = conversation.config().await?;
                    let mount_path = match mount_path {
                        Some(mount_path) => {
                            validate_mount_path(&mount_path)?;
                            mount_path
                        }
                        None => default_mount_path(&canonical_host_path, &config.mounts),
                    };
                    let new_mount = FileSystemMount {
                        host_path: canonical_host_path.display().to_string(),
                        mount_path: mount_path.clone(),
                        mode: if rw {
                            FileSystemMountMode::ReadWrite
                        } else {
                            FileSystemMountMode::ReadOnly
                        },
                        internal: Some(internal),
                    };

                    if let Some(existing) = config
                        .mounts
                        .iter_mut()
                        .find(|mount| mount.mount_path == mount_path)
                    {
                        *existing = new_mount;
                    } else {
                        config.mounts.push(new_mount);
                    }

                    conversation.put_config(config).await?;
                    println!(
                        "mounted {} -> {} ({}) for {}",
                        canonical_host_path.display(),
                        mount_path,
                        if rw { "rw" } else { "ro" },
                        conversation.record().slug
                    );
                }
                ConversationMountCommands::Remove {
                    agent,
                    conversation,
                    mount_path,
                } => {
                    let conversation =
                        must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                    let mut config = conversation.config().await?;
                    let before = config.mounts.len();
                    config.mounts.retain(|mount| mount.mount_path != mount_path);
                    if config.mounts.len() == before {
                        bail!("mount not found: {mount_path}");
                    }
                    conversation.put_config(config).await?;
                    println!(
                        "removed mount {} from {}",
                        mount_path,
                        conversation.record().slug
                    );
                }
            },
            ConversationCommands::Sandbox { command } => match command {
                ConversationSandboxCommands::Run {
                    agent,
                    conversation,
                    command,
                } => {
                    let agent_handle = must_get_agent(harness.as_ref(), &agent).await?;
                    let conversation = agent_handle
                        .get_conversation(&conversation)
                        .await?
                        .ok_or_else(|| anyhow!("conversation not found: {}", conversation))?;
                    let output = run_sandbox_shell_command(
                        agent_handle.as_ref(),
                        conversation.as_ref(),
                        command,
                    )
                    .await?;
                    io::stdout().write_all(output.stdout.as_bytes())?;
                    io::stderr().write_all(output.stderr.as_bytes())?;
                    if output.exit_code != 0 {
                        bail!("sandbox command exited with status {}", output.exit_code);
                    }
                }
            },
            ConversationCommands::Show {
                agent,
                conversation,
            } => {
                let agent_handle = must_get_agent(harness.as_ref(), &agent).await?;
                let agent_config = agent_handle.config().await?;
                let conversation = agent_handle
                    .get_conversation(&conversation)
                    .await?
                    .ok_or_else(|| anyhow!("conversation not found: {}", conversation))?;
                let config = conversation.config().await?;
                let model_override = conversation.model_override().await?;
                let messages = conversation.messages().await?;
                let effective_model =
                    model_override
                        .clone()
                        .unwrap_or_else(|| ConversationModelConfig {
                            model: agent_config.model.clone(),
                            max_output_tokens: agent_config.max_output_tokens,
                        });
                println!("id: {}", conversation.record().id);
                println!("slug: {}", conversation.record().slug);
                println!("name: {}", conversation.record().name);
                println!(
                    "latest_event_id: {}",
                    conversation
                        .record()
                        .latest_event_id
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!("message_count: {}", messages.len());
                println!(
                    "shell_program: {}",
                    config.shell_program.as_deref().unwrap_or("none")
                );
                println!(
                    "sandbox_scope: {}",
                    config
                        .sandbox_scope
                        .map(sandbox_scope_name)
                        .unwrap_or("default")
                );
                println!(
                    "effective_sandbox_scope: {}",
                    sandbox_scope_name(effective_sandbox_scope(&agent_config, &config))
                );
                println!(
                    "sandbox_image: {}",
                    config.sandbox_image.as_deref().unwrap_or("inherit")
                );
                println!(
                    "effective_sandbox_image: {}",
                    config
                        .effective_sandbox_image(&agent_config)
                        .unwrap_or("default")
                );
                println!(
                    "sandbox_provider: {}",
                    config
                        .sandbox_provider
                        .map(format_sandbox_provider)
                        .unwrap_or("inherit")
                );
                println!(
                    "effective_sandbox_provider: {}",
                    format_sandbox_provider(config.effective_sandbox_provider(&agent_config))
                );
                println!(
                    "model_override: {}",
                    model_override
                        .as_ref()
                        .map(|config| config.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!("effective_model: {}", effective_model.model);
                println!(
                    "effective_max_output_tokens: {}",
                    effective_model
                        .max_output_tokens
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                println!("mounts:");
                print_mounts(&config.mounts);
            }
            ConversationCommands::Events {
                agent,
                conversation,
                types,
                limit,
                desc,
                cursor,
                session_id,
                turn_id,
            } => {
                let conversation =
                    must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                let result = conversation
                    .exoharness_handle()
                    .get_events(Some(EventQuery {
                        cursor: parse_optional_uuid7(cursor.as_deref(), "cursor")?,
                        direction: Some(if desc {
                            EventQueryDirection::Desc
                        } else {
                            EventQueryDirection::Asc
                        }),
                        limit,
                        session_id: parse_optional_uuid7(session_id.as_deref(), "session_id")?,
                        turn_id: parse_optional_uuid7(turn_id.as_deref(), "turn_id")?,
                        types: if types.is_empty() {
                            None
                        } else {
                            // User-supplied strings; `EventKind::custom` matches
                            // both known kinds (by name) and Custom events.
                            Some(types.into_iter().map(EventKind::custom).collect())
                        },
                    }))
                    .await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            ConversationCommands::Send {
                agent,
                conversation,
                prompt,
            } => {
                let conversation =
                    must_get_conversation(harness.as_ref(), &agent, &conversation).await?;
                let previous_messages = conversation.messages().await?;
                send_conversation_wakeup(conversation.as_ref(), prompt).await?;
                let messages = conversation.messages().await?;
                for message in &messages[previous_messages.len()..] {
                    print_message(message);
                }
            }
            ConversationCommands::Delete {
                agent,
                conversation,
            } => {
                let agent = must_get_agent(harness.as_ref(), &agent).await?;
                if !agent.delete_conversation(&conversation).await? {
                    println!("conversation {} not found; nothing to delete", conversation);
                } else {
                    println!("deleted conversation {}", conversation);
                }
            }
        },
        Commands::Secret { command } => match command {
            SecretCommands::List => {
                let secrets = harness.exoharness_handle().list_secrets().await?;
                print_table(
                    &["SECRET", "TYPE", "CREATED_AT"],
                    secrets
                        .into_iter()
                        .map(|secret| {
                            vec![
                                secret.name,
                                format!("{:?}", secret.r#type),
                                secret.created_at.to_string(),
                            ]
                        })
                        .collect(),
                )?;
            }
            SecretCommands::Check => {
                harness
                    .exoharness_handle()
                    .preflight_secret_storage()
                    .await?;
            }
            SecretCommands::Set { name, env, value } => {
                let value = match (env, value) {
                    (Some(env), None) => secret_value_from_env_arg(&env, &env_vars)?,
                    (None, Some(value)) => value,
                    (Some(_), Some(_)) => {
                        bail!("provide either --env or --value, not both");
                    }
                    (None, None) => bail!("provide --env or --value"),
                };
                let id = harness
                    .exoharness_handle()
                    .put_secret(PutSecretRequest {
                        name: name.clone(),
                        secret: Secret::Key { value },
                    })
                    .await?;
                println!("set secret {} ({})", name, id);
            }
        },
        Commands::Auth { command } => match command {
            AuthCommands::Login { provider, name } => {
                require_openai_chatgpt_provider(&provider)?;
                let name = oauth_credential_name(name);
                harness
                    .exoharness_handle()
                    .preflight_secret_storage()
                    .await
                    .context("secret storage is unavailable; device login was not started")?;
                if let Some(secret_id) =
                    find_secret_id(harness.exoharness_handle().as_ref(), &name).await?
                    && let Some(secret) = harness.exoharness_handle().get_secret(&secret_id).await?
                {
                    match secret {
                        Secret::Oauth {
                            access_token: Some(_),
                            ..
                        } => bail!(
                            "OAuth credential `{name}` is already logged in; run `exo auth logout {provider} --name {name}` first"
                        ),
                        Secret::Oauth {
                            provider: Some(existing_provider),
                            ..
                        } if existing_provider != provider => bail!(
                            "credential `{name}` belongs to OAuth provider `{existing_provider}`"
                        ),
                        Secret::Oauth { .. } => {}
                        Secret::Key { .. } => {
                            bail!("credential `{name}` is an API key, not an OAuth credential")
                        }
                    }
                }

                let oauth = OpenAiChatGptOAuthProvider::new();
                let tokens = oauth
                    .login_device_code(|prompt| {
                        println!("Open {}", prompt.verification_url);
                        println!("Enter code: {}", prompt.user_code);
                        println!("Waiting for authorization…");
                    })
                    .await?;
                let secret_id = harness
                    .exoharness_handle()
                    .put_secret(PutSecretRequest {
                        name: name.clone(),
                        secret: Secret::Oauth {
                            provider: Some(provider),
                            account_id: tokens.account_id,
                            access_token: Some(tokens.access_token),
                            refresh_token: tokens.refresh_token,
                            expires_at: Some(tokens.expires_at),
                        },
                    })
                    .await?;
                println!("logged in {name} ({secret_id})");
            }
            AuthCommands::Logout { provider, name } => {
                require_openai_chatgpt_provider(&provider)?;
                let name = oauth_credential_name(name);
                let secret_id = find_secret_id(harness.exoharness_handle().as_ref(), &name)
                    .await?
                    .ok_or_else(|| anyhow!("OAuth credential not found: {name}"))?;
                match harness
                    .exoharness_handle()
                    .get_secret(&secret_id)
                    .await?
                    .ok_or_else(|| anyhow!("OAuth credential not found: {name}"))?
                {
                    Secret::Oauth {
                        provider: Some(existing_provider),
                        ..
                    } if existing_provider != provider => {
                        bail!("credential `{name}` belongs to OAuth provider `{existing_provider}`")
                    }
                    Secret::Oauth { .. } => {}
                    Secret::Key { .. } => {
                        bail!("credential `{name}` is an API key, not an OAuth credential")
                    }
                }
                let result = harness
                    .exoharness_handle()
                    .logout_oauth_secret(&secret_id)
                    .await?;
                if !result.was_logged_in {
                    println!("{name} is already logged out");
                } else {
                    println!("logged out {name} locally");
                }
                if result.was_logged_in && !result.remote_revocation_confirmed {
                    eprintln!(
                        "warning: OpenAI did not confirm remote token revocation; local tokens were still cleared"
                    );
                }
            }
        },
        Commands::Model { command } => match command {
            ModelCommands::List => {
                let models = list_model_bindings(harness.exoharness_handle().as_ref()).await?;
                print_table(
                    &["MODEL", "UPSTREAM_MODEL", "PROVIDER", "SECRET", "BASE_URL"],
                    models
                        .into_iter()
                        .map(|model| {
                            vec![
                                model.name,
                                model.model,
                                model.provider.unwrap_or_else(|| "inferred".to_string()),
                                model.secret_name.unwrap_or_else(|| "none".to_string()),
                                model.base_url.unwrap_or_else(|| "default".to_string()),
                            ]
                        })
                        .collect(),
                )?;
            }
            ModelCommands::Register {
                name,
                model,
                secret,
                provider,
                base_url,
            } => {
                let secret_id = find_secret_id(harness.exoharness_handle().as_ref(), &secret)
                    .await?
                    .ok_or_else(|| anyhow!("secret not found: {secret}"))?;
                let upstream_model = model.unwrap_or_else(|| name.clone());
                let id = harness
                    .exoharness_handle()
                    .put_binding(Binding::Llm {
                        name: name.clone(),
                        model: upstream_model,
                        provider,
                        base_url,
                        secret_id: Some(secret_id),
                    })
                    .await?;
                println!("registered model {} ({})", name, id);
            }
        },
        Commands::Provider { command } => match command {
            ProviderCommands::List => {
                for record in harness.exoharness_handle().list_bindings().await? {
                    if let Binding::Sandbox { name, config } = record.binding {
                        println!("{name}\t{config:?}");
                    }
                }
            }
            ProviderCommands::Configure(args) => {
                let ProviderConfigureArgs {
                    provider,
                    name,
                    secret,
                    region,
                    organization_id,
                    project_id,
                    api_url,
                    runtime_arn,
                    qualifier,
                    session_storage_mount_path,
                    default_image,
                    url_auth,
                    labels,
                } = *args;
                let binding_name =
                    name.unwrap_or_else(|| SandboxProvider::from(provider).as_str().to_string());
                if !matches!(provider, SandboxProviderArg::AwsAgentCore)
                    && session_storage_mount_path.is_some()
                {
                    bail!("--session-storage-mount-path is only valid for aws-agentcore");
                }
                let config = match provider {
                    SandboxProviderArg::Daytona => {
                        let secret =
                            secret.ok_or_else(|| anyhow!("--secret is required for daytona"))?;
                        let secret_id =
                            find_secret_id(harness.exoharness_handle().as_ref(), &secret)
                                .await?
                                .ok_or_else(|| anyhow!("secret not found: {secret}"))?;
                        SandboxProviderConfig::Daytona {
                            api_key_secret_id: secret_id,
                            region,
                            organization_id,
                            api_url,
                            default_image: default_image.unwrap_or_else(default_daytona_image),
                        }
                    }
                    SandboxProviderArg::Vercel => {
                        let secret =
                            secret.ok_or_else(|| anyhow!("--secret is required for vercel"))?;
                        let secret_id =
                            find_secret_id(harness.exoharness_handle().as_ref(), &secret)
                                .await?
                                .ok_or_else(|| anyhow!("secret not found: {secret}"))?;
                        let team_id = organization_id
                            .ok_or_else(|| anyhow!("--organization-id is required for vercel"))?;
                        let project_id = project_id
                            .ok_or_else(|| anyhow!("--project-id is required for vercel"))?;
                        SandboxProviderConfig::Vercel {
                            api_token_secret_id: secret_id,
                            team_id,
                            project_id,
                            api_url,
                            default_image: default_image.unwrap_or_else(default_vercel_image),
                        }
                    }
                    SandboxProviderArg::AwsAgentCore => {
                        let runtime_arn = runtime_arn.ok_or_else(|| {
                            anyhow!("--runtime-arn is required for aws-agentcore")
                        })?;
                        let region = match region {
                            Some(region) => region,
                            None => aws_region_from_arn(&runtime_arn, "bedrock-agentcore").ok_or_else(|| {
                                anyhow!(
                                    "--region is required when the AgentCore runtime ARN does not include a region"
                                )
                            })?,
                        };
                        SandboxProviderConfig::AwsAgentCore {
                            runtime_arn,
                            region,
                            qualifier,
                            endpoint_url: api_url,
                            session_storage_mount_path,
                            default_image: default_image
                                .unwrap_or_else(default_aws_agentcore_image),
                        }
                    }
                    SandboxProviderArg::Docker => SandboxProviderConfig::Docker {
                        default_image: default_image.unwrap_or_else(default_docker_image),
                    },
                    SandboxProviderArg::E2b => {
                        let secret =
                            secret.ok_or_else(|| anyhow!("--secret is required for e2b"))?;
                        let secret_id =
                            find_secret_id(harness.exoharness_handle().as_ref(), &secret)
                                .await?
                                .ok_or_else(|| anyhow!("secret not found: {secret}"))?;
                        SandboxProviderConfig::E2b {
                            api_key_secret_id: secret_id,
                            api_url,
                            default_image: default_image.unwrap_or_else(default_e2b_template),
                        }
                    }
                    SandboxProviderArg::Sprites => {
                        let secret =
                            secret.ok_or_else(|| anyhow!("--secret is required for sprites"))?;
                        let secret_id =
                            find_secret_id(harness.exoharness_handle().as_ref(), &secret)
                                .await?
                                .ok_or_else(|| anyhow!("secret not found: {secret}"))?;
                        SandboxProviderConfig::Sprites {
                            token_secret_id: secret_id,
                            api_url,
                            url_auth,
                            organization: organization_id,
                            labels,
                        }
                    }
                    other => bail!("provider {other:?} has no binding-based config yet"),
                };
                let id = harness
                    .exoharness_handle()
                    .put_binding(Binding::Sandbox {
                        name: binding_name.clone(),
                        config,
                    })
                    .await?;
                println!("configured sandbox provider {binding_name} ({id})");
            }
        },
        Commands::Serve { .. } => {
            unreachable!("serve commands are handled before harness instantiation")
        }
    }

    harness.flush_tracing().await?;
    Ok(())
}

async fn determine_harness_kind(
    exoharness: &dyn ExoHarness,
    selection: Option<&HarnessSelection>,
    command: &Commands,
) -> Result<HarnessKind> {
    if let Some(selection) = selection {
        return Ok(selection.harness_kind());
    }
    let Some(agent_ref) = command_agent_ref(command) else {
        return Ok(HarnessKind::Basic);
    };

    Ok(infer_agent_harness_kind(exoharness, agent_ref)
        .await?
        .unwrap_or(HarnessKind::Basic))
}

fn command_agent_ref(command: &Commands) -> Option<&str> {
    match command {
        Commands::Agent { command } => match command {
            AgentCommands::Update { agent, .. }
            | AgentCommands::Show { agent }
            | AgentCommands::Delete { agent } => Some(agent.as_str()),
            AgentCommands::Mount { command } => match command {
                AgentMountCommands::List { agent }
                | AgentMountCommands::Add { agent, .. }
                | AgentMountCommands::Remove { agent, .. } => Some(agent.as_str()),
            },
            AgentCommands::List | AgentCommands::Create { .. } => None,
        },
        Commands::Conversation { command } => match command {
            ConversationCommands::List { agent }
            | ConversationCommands::Create { agent, .. }
            | ConversationCommands::Fork { agent, .. }
            | ConversationCommands::Update { agent, .. }
            | ConversationCommands::Show { agent, .. }
            | ConversationCommands::Events { agent, .. }
            | ConversationCommands::Send { agent, .. }
            | ConversationCommands::Delete { agent, .. } => Some(agent.as_str()),
            ConversationCommands::Mount { command } => match command {
                ConversationMountCommands::List { agent, .. }
                | ConversationMountCommands::Add { agent, .. }
                | ConversationMountCommands::Remove { agent, .. } => Some(agent.as_str()),
            },
            ConversationCommands::Sandbox { command } => match command {
                ConversationSandboxCommands::Run { agent, .. } => Some(agent.as_str()),
            },
        },
        Commands::Repl { agent, .. } => Some(agent.as_deref().unwrap_or(DEFAULT_REPL_SLUG)),
        Commands::Secret { .. }
        | Commands::Auth { .. }
        | Commands::Model { .. }
        | Commands::Provider { .. }
        | Commands::Adapters { .. }
        | Commands::Serve { .. } => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct ServeConfig {
    bind: SocketAddr,
    verbosity: u8,
}

fn serve_config(command: &Commands) -> Option<ServeConfig> {
    match command {
        Commands::Serve { bind, verbose } => Some(ServeConfig {
            bind: *bind,
            verbosity: *verbose,
        }),
        _ => None,
    }
}

async fn serve_exoharness_http(
    exo_config: &BasicExoHarnessConfig,
    config: ServeConfig,
) -> Result<()> {
    init_serve_tracing(config.verbosity);
    if !config.bind.ip().is_loopback() {
        anyhow::bail!(
            "exo serve only binds loopback addresses; got {}",
            config.bind
        );
    }
    let exoharness = Arc::new(BasicExoHarness::new(exo_config.clone()).await?);
    let listener = TcpListener::bind(config.bind)?;
    let addr = listener.local_addr()?;
    tracing::info!(
        target: HTTP_EXOHARNESS_TRACING_TARGET,
        %addr,
        "serving exoharness HTTP"
    );
    serve_exoharness_http_listener_with_options(
        listener,
        exoharness,
        ExoHarnessHttpServeOptions {
            verbosity: config.verbosity,
        },
    )
    .await?;
    Ok(())
}

fn init_serve_tracing(verbosity: u8) {
    if verbosity == 0 {
        return;
    }
    let level = if verbosity > 1 {
        tracing_subscriber::filter::LevelFilter::DEBUG
    } else {
        tracing_subscriber::filter::LevelFilter::INFO
    };
    let filter = tracing_subscriber::filter::Targets::new()
        .with_target(HTTP_EXOHARNESS_TRACING_TARGET, level);
    let layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .with_filter(filter);
    match tracing_subscriber::registry().with(layer).try_init() {
        Ok(()) | Err(_) => {}
    }
}

async fn infer_agent_harness_kind(
    exoharness: &dyn ExoHarness,
    agent_ref: &str,
) -> Result<Option<HarnessKind>> {
    let agent = if let Ok(agent_id) = agent_ref.parse::<Uuid7>() {
        exoharness.get_agent(&agent_id).await?
    } else {
        exoharness
            .list_agents()
            .await?
            .into_iter()
            .find(|agent| agent.record().slug == agent_ref)
    };
    let Some(agent) = agent else {
        return Ok(None);
    };

    let config = load_agent_config(agent.as_ref()).await?;
    Ok(Some(from_agent_harness_kind(config.harness)))
}

async fn instantiate_exoharness(
    exo_config: &BasicExoHarnessConfig,
    http_url: Option<&str>,
    bearer_token: Option<String>,
) -> Result<Arc<dyn ExoHarness>> {
    if let Some(http_url) = http_url {
        let mut harness = HttpExoHarness::new(http_url)?;
        if let Some(bearer_token) = bearer_token {
            harness = harness.with_bearer_token(bearer_token);
        }
        let remote: Arc<dyn ExoHarness> = Arc::new(harness);
        let local: Arc<dyn ExoHarness> = Arc::new(BasicExoHarness::new(exo_config.clone()).await?);
        return Ok(Arc::new(LocalSandboxExoHarness::new_with_force_local(
            remote, local, false,
        )));
    }
    Ok(Arc::new(BasicExoHarness::new(exo_config.clone()).await?))
}

async fn instantiate_harness(
    root: &Path,
    exo_config: &BasicExoHarnessConfig,
    exoharness: Arc<dyn ExoHarness>,
    kind: HarnessKind,
    runtime_config: Option<BraintrustRuntimeConfig>,
    env_vars: HashMap<String, String>,
    pricing: Arc<cost::PricingTable>,
) -> Result<Arc<dyn Harness>> {
    let harness: Arc<dyn Harness> = match kind {
        HarnessKind::Basic => Arc::new(BasicHarness::from_exoharness(
            exoharness,
            runtime_config,
            env_vars,
            pricing,
        )),
        HarnessKind::Rlm => Arc::new(RlmHarness::from_exoharness(
            exoharness,
            runtime_config,
            env_vars,
        )),
        HarnessKind::Exo => Arc::new(
            TypeScriptHarness::<ExoToolRuntime>::exo_from_root(
                root,
                exo_config.clone(),
                runtime_config,
                env_vars,
            )
            .await?,
        ),
        HarnessKind::TypeScript => Arc::new(TypeScriptHarness::from_exoharness(
            exoharness,
            runtime_config,
            env_vars,
        )?),
    };
    Ok(harness)
}

fn to_agent_harness_kind(kind: HarnessKind) -> AgentHarnessKind {
    match kind {
        HarnessKind::Basic => AgentHarnessKind::Basic,
        HarnessKind::Rlm => AgentHarnessKind::Rlm,
        HarnessKind::TypeScript => AgentHarnessKind::TypeScript,
        HarnessKind::Exo => AgentHarnessKind::Exo,
    }
}

fn from_agent_harness_kind(kind: AgentHarnessKind) -> HarnessKind {
    match kind {
        AgentHarnessKind::Basic => HarnessKind::Basic,
        AgentHarnessKind::Rlm => HarnessKind::Rlm,
        AgentHarnessKind::TypeScript => HarnessKind::TypeScript,
        AgentHarnessKind::Exo => HarnessKind::Exo,
    }
}

fn format_harness_kind(kind: AgentHarnessKind) -> &'static str {
    match kind {
        AgentHarnessKind::Basic => "basic",
        AgentHarnessKind::Rlm => "rlm",
        AgentHarnessKind::TypeScript => "typescript",
        AgentHarnessKind::Exo => "exo",
    }
}

fn format_sandbox_provider(provider: SandboxProvider) -> &'static str {
    provider.as_str()
}

fn build_typescript_harness_config(
    selection: Option<&HarnessSelection>,
    module: Option<&Path>,
    tool_modules: &[PathBuf],
) -> Result<Option<TypeScriptHarnessConfig>> {
    let harness_kind = selection
        .map(HarnessSelection::harness_kind)
        .unwrap_or(HarnessKind::Basic);
    if !matches!(harness_kind, HarnessKind::TypeScript | HarnessKind::Exo)
        && !tool_modules.is_empty()
    {
        bail!("--tool-module is only valid with --harness typescript or exo");
    }
    match (selection, harness_kind, module) {
        (Some(HarnessSelection::TypeScriptPreset(_)), _, Some(_))
        | (Some(HarnessSelection::TypeScriptModule(_)), _, Some(_)) => Err(anyhow!(
            "--module cannot be combined with a TypeScript module selected by --harness"
        )),
        (Some(HarnessSelection::TypeScriptPreset(preset)), _, None) => {
            Ok(Some(resolve_typescript_harness_config(
                preset.module_path(),
                resolve_typescript_tool_module_paths(tool_modules)?,
            )?))
        }
        (Some(HarnessSelection::TypeScriptModule(module)), _, None) => {
            Ok(Some(resolve_typescript_harness_config(
                module,
                resolve_typescript_tool_module_paths(tool_modules)?,
            )?))
        }
        (_, HarnessKind::TypeScript | HarnessKind::Exo, Some(module)) => {
            Ok(Some(resolve_typescript_harness_config(
                module,
                resolve_typescript_tool_module_paths(tool_modules)?,
            )?))
        }
        (_, HarnessKind::TypeScript, None) => Err(anyhow!(
            "typescript agents require --module <path>, or use --harness codex, --harness claude-code, --harness cursor, or --harness <module.ts>"
        )),
        (_, HarnessKind::Exo, None) => Err(anyhow!("exo agents require --module <path>")),
        (_, _, Some(_)) => Err(anyhow!(
            "--module is only valid with --harness typescript or exo"
        )),
        (_, _, None) => Ok(None),
    }
}

fn default_repl_agent_slug(selection: Option<&HarnessSelection>) -> String {
    selection
        .and_then(HarnessSelection::default_agent_slug)
        .unwrap_or_else(|| DEFAULT_REPL_SLUG.to_string())
}

async fn ensure_agent_matches_harness_selection(
    agent: &dyn HarnessAgent,
    selection: &HarnessSelection,
) -> Result<()> {
    let config = agent.config().await?;
    let expected = to_agent_harness_kind(selection.harness_kind());
    if config.harness != expected {
        bail!(
            "agent {} is configured for {}; --harness {} requires {}",
            agent.record().slug,
            format_harness_kind(config.harness),
            format_harness_selection(selection),
            format_harness_kind(expected)
        );
    }

    if matches!(
        selection.harness_kind(),
        HarnessKind::TypeScript | HarnessKind::Exo
    ) && config.typescript.is_none()
    {
        bail!(
            "agent {} is configured for {} but has no module path",
            agent.record().slug,
            format_harness_selection(selection)
        );
    }

    let expected_typescript = match selection {
        HarnessSelection::TypeScriptPreset(_) | HarnessSelection::TypeScriptModule(_) => {
            build_typescript_harness_config(Some(selection), None, &[])?
        }
        HarnessSelection::Kind(_) => None,
    };
    if let Some(expected_typescript) = expected_typescript {
        let Some(actual_typescript) = config.typescript.as_ref() else {
            bail!(
                "agent {} is missing TypeScript module {}",
                agent.record().slug,
                expected_typescript.module_path
            );
        };
        if actual_typescript.module_path != expected_typescript.module_path {
            bail!(
                "agent {} uses TypeScript module {}; --harness {} resolved to {}",
                agent.record().slug,
                actual_typescript.module_path,
                format_harness_selection(selection),
                expected_typescript.module_path
            );
        }
    }

    Ok(())
}

fn resolve_typescript_harness_config(
    module_path: &Path,
    tool_module_paths: Vec<String>,
) -> Result<TypeScriptHarnessConfig> {
    let module_path = std::fs::canonicalize(module_path)?;
    Ok(TypeScriptHarnessConfig {
        module_path: module_path.to_string_lossy().into_owned(),
        tool_module_paths,
    })
}

fn looks_like_typescript_module_path(value: &str) -> bool {
    let path = Path::new(value);
    value.contains(std::path::MAIN_SEPARATOR)
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "ts" | "tsx" | "js" | "mjs" | "cjs"))
}

fn format_harness_selection(selection: &HarnessSelection) -> String {
    match selection {
        HarnessSelection::Kind(kind) => match kind {
            HarnessKind::Basic => "basic".to_string(),
            HarnessKind::Rlm => "rlm".to_string(),
            HarnessKind::TypeScript => "typescript".to_string(),
            HarnessKind::Exo => "exo".to_string(),
        },
        HarnessSelection::TypeScriptPreset(preset) => match preset {
            TypeScriptHarnessPreset::Codex => "codex".to_string(),
            TypeScriptHarnessPreset::ClaudeCode => "claude-code".to_string(),
            TypeScriptHarnessPreset::Cursor => "cursor".to_string(),
        },
        HarnessSelection::TypeScriptModule(path) => path.display().to_string(),
    }
}

fn resolve_typescript_tool_module_paths(paths: &[PathBuf]) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|path| {
            let path = std::fs::canonicalize(path)?;
            Ok(path.to_string_lossy().into_owned())
        })
        .collect()
}

fn print_table(headers: &[&str], rows: Vec<Vec<String>>) -> Result<()> {
    let stdout = io::stdout();
    let mut writer = TabWriter::new(stdout.lock()).padding(2);
    write_table_row(&mut writer, headers)?;
    for row in rows {
        write_table_row(&mut writer, &row)?;
    }
    writer.flush()?;
    Ok(())
}

fn write_table_row<T: AsRef<str>, W: Write>(writer: &mut W, values: &[T]) -> io::Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            write!(writer, "\t")?;
        }
        write!(writer, "{}", value.as_ref())?;
    }
    writeln!(writer)
}

struct RegisteredModel {
    name: String,
    model: String,
    provider: Option<String>,
    secret_name: Option<String>,
    base_url: Option<String>,
}

async fn list_model_bindings(exoharness: &dyn ExoHarness) -> Result<Vec<RegisteredModel>> {
    let secrets = exoharness.list_secrets().await?;
    let mut models = Vec::new();
    for metadata in exoharness.list_bindings().await? {
        let Binding::Llm {
            name,
            model,
            provider,
            base_url,
            secret_id,
        } = metadata.binding
        else {
            continue;
        };
        let secret_name = secret_id.and_then(|secret_id| {
            secrets
                .iter()
                .find(|secret| secret.id == secret_id)
                .map(|secret| secret.name.clone())
        });
        models.push(RegisteredModel {
            name,
            model,
            provider,
            secret_name,
            base_url,
        });
    }
    let mut deduped = Vec::<RegisteredModel>::new();
    for model in models {
        if let Some(existing) = deduped
            .iter_mut()
            .find(|existing| existing.name == model.name)
        {
            *existing = model;
        } else {
            deduped.push(model);
        }
    }
    Ok(deduped)
}

const DEFAULT_REPL_SLUG: &str = "repl";

/// Resolves the model binding a quickstart REPL agent should use. Registering a
/// model is left to `exo secret set` / `exo model register`, so the substrate
/// never reads credentials from the environment on its own.
async fn ensure_repl_model(harness: &dyn Harness, requested: Option<String>) -> Result<String> {
    let registered: Vec<String> = list_model_bindings(harness.exoharness_handle().as_ref())
        .await?
        .into_iter()
        .map(|binding| binding.name)
        .collect();
    pick_repl_model(&registered, requested)
}

async fn ensure_existing_repl_agent_model(
    harness: &dyn Harness,
    agent: &dyn HarnessAgent,
    requested: Option<String>,
) -> Result<()> {
    let mut config = agent.config().await?;
    if !repl_agent_model_needs_update(&config.model, requested.as_deref()) {
        return Ok(());
    }
    let model = ensure_repl_model(harness, requested).await?;
    if config.model == model {
        return Ok(());
    }
    config.model = model;
    agent.put_config(config).await
}

fn repl_agent_model_needs_update(current: &str, requested: Option<&str>) -> bool {
    requested.is_some() || current.trim().is_empty()
}

/// Picks the model an explicit request names, falling back to the first
/// registered binding. Errors with setup guidance when neither is available.
fn pick_repl_model(registered: &[String], requested: Option<String>) -> Result<String> {
    if let Some(requested) = requested {
        if registered.iter().any(|name| name == &requested) {
            return Ok(requested);
        }
        bail!(
            "model is not registered: {requested}; register it with `exo model register {requested} --secret <secret>`"
        );
    }
    registered.first().cloned().ok_or_else(|| {
        anyhow!(
            "no model is registered; set one up first:\n  \
             exo secret set openai --env OPENAI_API_KEY\n  \
             exo model register gpt-5.5 --secret openai"
        )
    })
}

async fn find_secret_id(exoharness: &dyn ExoHarness, name: &str) -> Result<Option<Uuid7>> {
    Ok(exoharness
        .list_secrets()
        .await?
        .into_iter()
        .rev()
        .find(|secret| secret.name == name)
        .map(|secret| secret.id))
}

fn require_openai_chatgpt_provider(provider: &str) -> Result<()> {
    if provider != OPENAI_CHATGPT_PROVIDER_ID {
        bail!("unsupported OAuth provider `{provider}`; expected `{OPENAI_CHATGPT_PROVIDER_ID}`");
    }
    Ok(())
}

fn oauth_credential_name(name: Option<String>) -> String {
    name.unwrap_or_else(|| OPENAI_CHATGPT_PROVIDER_ID.to_string())
}

fn build_braintrust_tracing_config(
    org_name: Option<String>,
    project_name: Option<String>,
    project_id: Option<String>,
) -> Result<Option<BraintrustTracingConfig>> {
    match (project_name, project_id) {
        (Some(_), Some(_)) => Err(anyhow!(
            "provide either --braintrust-project or --braintrust-project-id, not both"
        )),
        (Some(project_name), None) => Ok(Some(BraintrustTracingConfig {
            org_name,
            project: BraintrustProject::Name(project_name),
        })),
        (None, Some(project_id)) => Ok(Some(BraintrustTracingConfig {
            org_name,
            project: BraintrustProject::Id(project_id),
        })),
        (None, None) => Ok(None),
    }
}

fn format_braintrust_tracing_config(config: Option<&BraintrustTracingConfig>) -> String {
    let Some(config) = config else {
        return "none".to_string();
    };

    let project = match &config.project {
        BraintrustProject::Name(name) => format!("project={name}"),
        BraintrustProject::Id(id) => format!("project_id={id}"),
    };

    match &config.org_name {
        Some(org_name) => format!("org={org_name}, {project}"),
        None => project,
    }
}

fn parse_optional_uuid7(value: Option<&str>, field: &str) -> Result<Option<Uuid7>> {
    match value {
        Some(value) => Ok(Some(
            value
                .parse::<Uuid7>()
                .map_err(|error| anyhow!("invalid {field}: {error}"))?,
        )),
        None => Ok(None),
    }
}

fn canonicalize_directory(path: &PathBuf) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.is_dir() {
        bail!(
            "mount host path is not a directory: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn validate_mount_path(mount_path: &str) -> Result<()> {
    if mount_path.trim().is_empty() {
        bail!("mount path must not be empty");
    }
    if !mount_path.starts_with('/') {
        bail!("mount path must be absolute");
    }
    Ok(())
}

pub(crate) fn default_mount_path(host_path: &Path, existing_mounts: &[FileSystemMount]) -> String {
    let base_name = host_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("mount");

    let mut candidate = format!("{SANDBOX_MAIN_MOUNT_DIR}/{base_name}");
    let mut suffix = 2;
    while existing_mounts
        .iter()
        .any(|mount| mount.mount_path == candidate)
    {
        candidate = format!("{SANDBOX_MAIN_MOUNT_DIR}/{base_name}-{suffix}");
        suffix += 1;
    }
    candidate
}

fn print_mounts(mounts: &[FileSystemMount]) {
    if mounts.is_empty() {
        println!("  none");
        return;
    }

    for mount in mounts {
        let mode = match mount.mode {
            FileSystemMountMode::ReadOnly => "ro",
            FileSystemMountMode::ReadWrite => "rw",
        };
        let internal = mount.internal.unwrap_or(false);
        println!(
            "  {} -> {} ({mode}{})",
            mount.host_path,
            mount.mount_path,
            if internal { ", internal" } else { "" }
        );
    }
}

async fn must_get_agent(harness: &dyn Harness, agent_ref: &str) -> Result<Arc<dyn HarnessAgent>> {
    harness
        .get_agent(agent_ref)
        .await?
        .ok_or_else(|| anyhow!("agent not found: {agent_ref}"))
}

async fn must_get_conversation(
    harness: &dyn Harness,
    agent_ref: &str,
    conversation_ref: &str,
) -> Result<Arc<dyn HarnessConversation>> {
    let agent = must_get_agent(harness, agent_ref).await?;
    agent
        .get_conversation(conversation_ref)
        .await?
        .ok_or_else(|| anyhow!("conversation not found: {conversation_ref}"))
}

#[derive(Debug, Deserialize)]
struct SandboxShellOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

async fn run_sandbox_shell_command(
    agent: &dyn HarnessAgent,
    conversation: &dyn HarnessConversation,
    command: String,
) -> Result<SandboxShellOutput> {
    let agent_config = agent.config().await?;
    let config = conversation.config().await?;
    if config.shell_program.is_none() {
        bail!(
            "shell sandbox is not enabled for this conversation; run `exo conversation update {} {} --shell-program /bin/bash`",
            agent.record().slug,
            conversation.record().slug
        );
    }
    let agent_handle = agent.exoharness_handle();
    let conversation_handle = conversation.exoharness_handle();
    let runtime = BasicToolRuntime;

    let mut arguments = serde_json::Map::new();
    arguments.insert("command".to_string(), serde_json::Value::String(command));
    let result = runtime
        .execute(
            agent_handle.as_ref(),
            conversation_handle.as_ref(),
            None,
            &agent_config,
            &config,
            &ToolRequest {
                function_name: "shell".to_string(),
                arguments,
            },
        )
        .await?;
    Ok(serde_json::from_value(result)?)
}

pub(crate) fn print_message(message: &Message) {
    let timestamp = compact_timestamp();
    match message {
        Message::User { content } => {
            println!("{timestamp} user: {}", render_user_content(content));
        }
        Message::Assistant { content, .. } => {
            println!(
                "{timestamp} assistant: {}",
                render_assistant_content(content)
            );
        }
        Message::Tool { content } => {
            for part in content {
                let ToolContentPart::ToolResult(result) = part;
                println!("{timestamp} tool {}: {}", result.tool_name, result.output);
            }
        }
        Message::System { content } => {
            println!("{timestamp} system: {}", render_user_content(content));
        }
        Message::Developer { content } => {
            println!("{timestamp} developer: {}", render_user_content(content));
        }
    }
}

pub(crate) fn compact_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() % 86_400)
        .unwrap_or(0);
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    format!("[{hours:02}:{minutes:02}:{seconds:02}]")
}

fn render_user_content(content: &UserContent) -> String {
    match content {
        UserContent::String(text) => text.clone(),
        UserContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                lingua::universal::UserContentPart::Text(text) => text.text.clone(),
                _ => "[non-text user content]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

pub(crate) fn render_assistant_content(content: &AssistantContent) -> String {
    match content {
        AssistantContent::String(text) => text.clone(),
        AssistantContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                AssistantContentPart::Text(text) => text.text.clone(),
                AssistantContentPart::Reasoning { text, .. } => format!("[reasoning] {text}"),
                AssistantContentPart::ToolCall {
                    tool_name,
                    arguments,
                    ..
                } => format!("[tool_call {tool_name}] {arguments}"),
                AssistantContentPart::ToolResult {
                    tool_name, output, ..
                } => format!("[tool_result {tool_name}] {output}"),
                AssistantContentPart::File { .. } => "[file]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn repl_command(agent_slug: &str, conversation_slug: &str) -> String {
    format!("exo repl --agent {agent_slug} --conversation {conversation_slug}")
}

fn sandbox_scope_name(scope: SandboxScope) -> &'static str {
    match scope {
        SandboxScope::Agent => "agent",
        SandboxScope::Conversation => "conversation",
    }
}

fn slugify(input: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }

    while slug.ends_with('-') {
        slug.pop();
    }

    slug
}

pub(crate) fn secret_value_from_env_arg(
    env: &str,
    loaded_env: &HashMap<String, String>,
) -> Result<String> {
    env_value_from_arg("--env", env, loaded_env)
}

fn parse_env_var_name(value: &str) -> std::result::Result<String, String> {
    if is_env_var_name(value) {
        Ok(value.to_string())
    } else {
        Err(
            "pass an environment variable name such as OPENAI_API_KEY, not the secret value"
                .to_string(),
        )
    }
}

fn env_value_from_arg(
    flag: &str,
    env: &str,
    loaded_env: &HashMap<String, String>,
) -> Result<String> {
    if !is_env_var_name(env) {
        bail!(
            "invalid {flag} value; pass an environment variable name such as OPENAI_API_KEY, not the secret value"
        );
    }

    loaded_env
        .get(env)
        .cloned()
        .or_else(|| std::env::var(env).ok())
        .ok_or_else(|| anyhow!("environment variable passed to {flag} is not set"))
}

fn is_env_var_name(env: &str) -> bool {
    let mut chars = env.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }

    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

const SLUG_WORDS_A: &[&str] = &[
    "amber", "aster", "basil", "cedar", "cinder", "cobalt", "ember", "fable", "glacier", "harbor",
    "ivy", "juniper", "lilac", "marble", "north", "onyx", "pony", "quartz", "river", "solstice",
    "topaz", "velvet", "willow", "yarrow",
];

const SLUG_WORDS_B: &[&str] = &[
    "anchor", "beacon", "bloom", "cadence", "canyon", "drift", "echo", "feather", "forge",
    "gesture", "grove", "harvest", "lagoon", "lantern", "meadow", "orbit", "passage", "pebble",
    "ridge", "signal", "sparrow", "summit", "thistle", "window",
];

fn generate_fun_slug() -> String {
    generate_fun_slug_from_uuid(Uuid7::now())
}

pub(crate) fn generate_fun_slug_from_uuid(uuid: Uuid7) -> String {
    let bytes = uuid.0.as_bytes();
    let word_a = SLUG_WORDS_A[(bytes[0] as usize) % SLUG_WORDS_A.len()];
    let word_b = SLUG_WORDS_B[(bytes[1] as usize) % SLUG_WORDS_B.len()];
    let suffix = format!("{:02x}{:02x}", bytes[14], bytes[15]);
    format!("{word_a}-{word_b}-{suffix}")
}

#[cfg(test)]
mod create_tests {
    use super::repl_command;

    #[test]
    fn repl_command_uses_agent_and_conversation_slugs() {
        assert_eq!(
            repl_command("rlm", "aster-lantern-47db"),
            "exo repl --agent rlm --conversation aster-lantern-47db"
        );
    }

    #[test]
    fn repl_command_parses_without_arguments() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from(["exo", "repl"]).expect("repl parses with no args");
        assert!(matches!(
            cli.command,
            super::Commands::Repl {
                model: None,
                agent: None,
                conversation: None,
            }
        ));
    }

    #[test]
    fn repl_command_accepts_overrides() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from(["exo", "repl", "--model", "gpt-5.4"])
            .expect("repl parses with --model");
        assert!(matches!(
            cli.command,
            super::Commands::Repl { model: Some(model), .. } if model == "gpt-5.4"
        ));
    }

    #[test]
    fn repl_command_accepts_preset_harness_after_subcommand() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from(["exo", "repl", "--harness", "codex"])
            .expect("repl parses with a preset harness");
        assert!(matches!(
            cli.harness,
            Some(super::HarnessSelection::TypeScriptPreset(
                super::TypeScriptHarnessPreset::Codex
            ))
        ));
    }

    #[test]
    fn agent_update_accepts_preset_set_harness() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from([
            "exo",
            "agent",
            "update",
            "teleport2",
            "--set-harness",
            "codex",
        ])
        .expect("agent update parses with a preset harness");
        assert!(matches!(
            cli.command,
            super::Commands::Agent {
                command: super::AgentCommands::Update {
                    set_harness: Some(super::HarnessSelection::TypeScriptPreset(
                        super::TypeScriptHarnessPreset::Codex
                    )),
                    ..
                }
            }
        ));
    }

    #[test]
    fn repl_command_accepts_preset_harness_and_conversation() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from([
            "exo",
            "repl",
            "--harness",
            "codex",
            "--conversation",
            "existing",
        ])
        .expect("repl parses with a preset harness and conversation");
        assert!(matches!(
            cli.command,
            super::Commands::Repl {
                agent: None,
                conversation: Some(conversation),
                ..
            } if conversation == "existing"
        ));
    }

    #[test]
    fn preset_harness_defaults_repl_agent_slug() {
        assert_eq!(
            super::default_repl_agent_slug(Some(&super::HarnessSelection::TypeScriptPreset(
                super::TypeScriptHarnessPreset::Codex,
            ))),
            "codex"
        );
    }

    #[test]
    fn repl_command_accepts_module_path_harness_after_subcommand() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from(["exo", "repl", "--harness", "./my-harness.ts"])
            .expect("repl parses with a TypeScript module path");
        assert!(matches!(
            cli.harness,
            Some(super::HarnessSelection::TypeScriptModule(path))
                if path.as_path() == std::path::Path::new("./my-harness.ts")
        ));
    }

    #[test]
    fn conversation_send_command_parses() {
        use clap::Parser;
        let cli =
            super::Cli::try_parse_from(["exo", "conversation", "send", "agent", "conv", "hello"])
                .expect("conversation send parses");
        assert!(matches!(
            cli.command,
            super::Commands::Conversation {
                command: super::ConversationCommands::Send {
                    agent,
                    conversation,
                    prompt,
                }
            } if agent == "agent" && conversation == "conv" && prompt == "hello"
        ));
    }

    #[test]
    fn conversation_sandbox_run_command_parses() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from([
            "exo",
            "conversation",
            "sandbox",
            "run",
            "agent",
            "conv",
            "pwd && git status",
        ])
        .expect("conversation sandbox run parses");
        assert!(matches!(
            cli.command,
            super::Commands::Conversation {
                command: super::ConversationCommands::Sandbox {
                    command: super::ConversationSandboxCommands::Run {
                        agent,
                        conversation,
                        command,
                    },
                }
            } if agent == "agent" && conversation == "conv" && command == "pwd && git status"
        ));
    }

    #[test]
    fn exoharness_http_aliases_parse() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from([
            "exo",
            "--url",
            "http://localhost:8000/exo/v1/projects/project-id",
            "--bearer-env",
            "BRAINTRUST_API_KEY",
            "agent",
            "list",
        ])
        .expect("HTTP exoharness aliases parse");
        assert_eq!(
            cli.exoharness_url.as_deref(),
            Some("http://localhost:8000/exo/v1/projects/project-id")
        );
        assert_eq!(cli.bearer_env.as_deref(), Some("BRAINTRUST_API_KEY"));
    }

    #[test]
    fn bearer_env_requires_exoharness_url() {
        use clap::Parser;
        let error = super::Cli::try_parse_from([
            "exo",
            "--bearer-env",
            "BRAINTRUST_API_KEY",
            "agent",
            "list",
        ])
        .expect_err("bearer env should require an exoharness URL");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn sandbox_provider_local_process_parses() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from([
            "exo",
            "agent",
            "create",
            "test",
            "--sandbox-provider",
            "local-process",
            "--model",
            "test-model",
        ])
        .expect("local-process sandbox provider parses");
        assert!(matches!(
            cli.command,
            super::Commands::Agent {
                command: super::AgentCommands::Create {
                    sandbox_provider: Some(super::SandboxProviderArg::LocalProcess),
                    ..
                }
            }
        ));
    }
}

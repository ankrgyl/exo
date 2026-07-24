use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use executor::{
    BasicExoHarnessConfig, BraintrustRuntimeConfig, ExoToolRuntime, Harness,
    SandboxBackendRegistration, SchedulerRunOptions, SchedulerStore, SecretBackendChoice,
    TypeScriptHarness, run_due_tasks,
};

#[derive(Debug, Parser)]
#[command(name = "exo-scheduler-runner")]
#[command(about = "Example-local scheduler runner for Exo")]
struct Cli {
    #[arg(long, global = true, default_value = ".exo")]
    root: PathBuf,
    #[arg(long, global = true)]
    env_file_if_exists: Option<PathBuf>,
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
    #[arg(long, global = true, value_enum, env = "EXO_SANDBOX_BACKEND")]
    sandbox_backend: Option<SandboxBackendArg>,
    #[arg(long, global = true, env = "BRAINTRUST_API_KEY", hide = true)]
    braintrust_api_key: Option<String>,
    #[arg(long, global = true, env = "BRAINTRUST_APP_URL", hide = true)]
    braintrust_app_url: Option<String>,
    #[arg(long, global = true, env = "BRAINTRUST_API_URL", hide = true)]
    braintrust_api_url: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Run {
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 60)]
        interval_seconds: u64,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let env = load_env(cli.env_file_if_exists.as_deref(), cli.env_file.as_deref())?;
    let runtime_config = braintrust_runtime_config(
        &env,
        cli.braintrust_api_key,
        cli.braintrust_app_url,
        cli.braintrust_api_url,
    );
    let harness = exo_harness(&cli.root, runtime_config, env, cli.sandbox_backend).await?;

    match cli.command {
        Commands::Run {
            watch,
            interval_seconds,
            limit,
        } => {
            let _lock = SchedulerRunnerLock::acquire(&cli.root)?;
            let store = SchedulerStore::new(cli.root.join("scheduled-tasks"));
            loop {
                let runs =
                    run_due_tasks(Arc::clone(&harness), &store, SchedulerRunOptions { limit })
                        .await?;
                for run in runs {
                    println!(
                        "{}\t{}\texit={}\terror={}",
                        run.task_id,
                        run.id,
                        run.exit_code
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "none".to_string()),
                        run.error.unwrap_or_else(|| "none".to_string())
                    );
                }
                if !watch {
                    break;
                }
                if claim_restart_marker(&cli.root) {
                    println!("restart marker claimed; exiting so the guardian can restart us");
                    break;
                }
                tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
            }
        }
    }

    harness.flush_tracing().await?;
    Ok(())
}

struct SchedulerRunnerLock {
    path: PathBuf,
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

impl SchedulerRunnerLock {
    fn acquire(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let path = root.join("exo-scheduler.lock");
        let pid = std::process::id().to_string();
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                writeln!(file, "{pid}")?;
                Ok(Self { path })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing_pid = fs::read_to_string(&path).unwrap_or_default();
                if process_is_running(existing_pid.trim()) {
                    return Err(anyhow!(
                        "scheduler runner already appears to be running with pid {}",
                        existing_pid.trim()
                    ));
                }
                fs::remove_file(&path)?;
                Self::acquire(root)
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for SchedulerRunnerLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            tracing::error!(
                path = %self.path.display(),
                %error,
                "failed to remove scheduler lock"
            );
        }
    }
}

/// The guardian writes this marker to request a graceful restart: finish the
/// current scheduler pass, claim the marker (remove it), and exit so the
/// guardian can start a fresh build.
fn claim_restart_marker(root: &Path) -> bool {
    fs::remove_file(root.join("exo-scheduler.restart")).is_ok()
}

fn process_is_running(pid: &str) -> bool {
    !pid.is_empty()
        && Command::new("kill")
            .arg("-0")
            .arg(pid)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

async fn exo_harness(
    root: &Path,
    runtime_config: Option<BraintrustRuntimeConfig>,
    env: HashMap<String, String>,
    sandbox_backend_arg: Option<SandboxBackendArg>,
) -> Result<Arc<dyn Harness>> {
    let sandbox_backend = sandbox_backend_arg
        .map(SandboxBackendRegistration::from)
        .unwrap_or_else(default_sandbox_backend);
    let exo_config = BasicExoHarnessConfig {
        root: root.join("exoharness"),
        secret_backend: default_secret_backend(),
        sandbox_default: sandbox_backend.provider(),
        sandbox_backends: vec![sandbox_backend],
        // The runner must write to the same event store as the main process,
        // so it honors the same environment selection.
        groundhog: groundhog_store_config_from_env(),
    };
    Ok(Arc::new(
        TypeScriptHarness::<ExoToolRuntime>::exo_from_root(root, exo_config, runtime_config, env)
            .await?,
    ))
}

/// Conversation events go to a Groundhog engine when `EXO_GROUNDHOG_SOCKET`
/// points at a running `groundhog serve` Unix socket. `EXO_GROUNDHOG_SOURCE`
/// overrides the Groundhog source name (default `exo`), and
/// `EXO_GROUNDHOG_KERNEL_CONFIG` names a file whose content hash binds the
/// harness identity. Must match the exo CLI's settings so both processes
/// write under the same identity.
fn groundhog_store_config_from_env() -> Option<executor::GroundhogStoreConfig> {
    let socket = std::env::var_os("EXO_GROUNDHOG_SOCKET")?;
    Some(executor::GroundhogStoreConfig {
        socket: std::path::PathBuf::from(socket),
        source: std::env::var("EXO_GROUNDHOG_SOURCE").unwrap_or_else(|_| "exo".to_string()),
        kernel_config: std::env::var_os("EXO_GROUNDHOG_KERNEL_CONFIG")
            .map(std::path::PathBuf::from),
    })
}

#[cfg(target_os = "macos")]
fn default_secret_backend() -> SecretBackendChoice {
    SecretBackendChoice::AppleKeychain
}

#[cfg(not(target_os = "macos"))]
fn default_secret_backend() -> SecretBackendChoice {
    SecretBackendChoice::File { path: None }
}

#[cfg(target_os = "macos")]
fn default_sandbox_backend() -> SandboxBackendRegistration {
    SandboxBackendRegistration::apple_container()
}

#[cfg(not(target_os = "macos"))]
fn default_sandbox_backend() -> SandboxBackendRegistration {
    SandboxBackendRegistration::docker()
}

fn load_env(
    env_file_if_exists: Option<&Path>,
    env_file: Option<&Path>,
) -> Result<HashMap<String, String>> {
    let mut vars = HashMap::new();
    if let Some(path) = env_file_if_exists
        && path.exists()
    {
        vars.extend(parse_env_file(path)?);
    }
    if let Some(path) = env_file {
        vars.extend(parse_env_file(path)?);
    }
    Ok(vars)
}

fn parse_env_file(path: &Path) -> Result<HashMap<String, String>> {
    let contents = fs::read_to_string(path)?;
    let mut vars = HashMap::new();
    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return Err(anyhow!(
                "invalid env file line {} in {}",
                index + 1,
                path.display()
            ));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(anyhow!(
                "invalid empty env key on line {} in {}",
                index + 1,
                path.display()
            ));
        }
        vars.insert(key.to_string(), strip_quotes(value.trim()).to_string());
    }
    Ok(vars)
}

fn strip_quotes(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn braintrust_runtime_config(
    env: &HashMap<String, String>,
    api_key: Option<String>,
    app_url: Option<String>,
    api_url: Option<String>,
) -> Option<BraintrustRuntimeConfig> {
    let api_key = api_key.or_else(|| env.get("BRAINTRUST_API_KEY").cloned())?;
    Some(BraintrustRuntimeConfig {
        api_key,
        app_url: app_url.or_else(|| env.get("BRAINTRUST_APP_URL").cloned()),
        api_url: api_url.or_else(|| env.get("BRAINTRUST_API_URL").cloned()),
    })
}

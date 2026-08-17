use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use executor::{
    BasicExoHarnessConfig, BraintrustRuntimeConfig, ExoToolRuntime, Harness,
    SandboxBackendRegistration, SandboxProvider, SchedulerRunOptions, SchedulerStore,
    SecretBackendChoice, TypeScriptHarness, redeliver_pending_wakes, run_due_tasks,
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
    let harness = exo_harness(&cli.root, runtime_config, env).await?;

    match cli.command {
        Commands::Run {
            watch,
            interval_seconds,
            limit,
        } => {
            let _lock = SchedulerRunnerLock::acquire(&cli.root)?;
            let store = SchedulerStore::new(cli.root.join("scheduled-tasks"));
            // A previous runner may have died between finishing a task and
            // waking its conversation. Settle that before taking new work.
            let redelivered = redeliver_pending_wakes(Arc::clone(&harness), &store).await?;
            if redelivered > 0 {
                println!("redelivered {redelivered} pending conversation wakeup(s)");
            }
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
) -> Result<Arc<dyn Harness>> {
    let exo_config = BasicExoHarnessConfig {
        root: root.join("exoharness"),
        secret_backend: default_secret_backend(),
        sandbox_default: default_sandbox_provider(),
        sandbox_backends: vec![
            SandboxBackendRegistration::apple_container(),
            SandboxBackendRegistration::docker(),
            SandboxBackendRegistration::local_process(),
        ],
    };
    Ok(Arc::new(
        TypeScriptHarness::<ExoToolRuntime>::exo_from_root(root, exo_config, runtime_config, env)
            .await?,
    ))
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
fn default_sandbox_provider() -> SandboxProvider {
    SandboxProvider::AppleContainer
}

#[cfg(not(target_os = "macos"))]
fn default_sandbox_provider() -> SandboxProvider {
    SandboxProvider::Docker
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

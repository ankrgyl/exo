use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Subcommand;
use executor::{
    AdapterConfig, AdapterRunOptions, AdapterSource, AdapterStore, Harness, NewAdapter,
    run_adapters_watch,
};
use tabwriter::TabWriter;

#[derive(Debug, Subcommand)]
pub enum AdapterCommands {
    EnsureHarbor {
        agent: String,
        conversation: String,
        #[arg(long, default_value = "harbor")]
        name: String,
        #[arg(long)]
        socket_path: Option<PathBuf>,
        #[arg(long, default_value = "examples/exo/adapters/harbor/worker.ts")]
        worker_path: PathBuf,
        #[arg(long, default_value = "node_modules/.bin/tsx")]
        tsx_path: PathBuf,
    },
    List {
        #[arg(long)]
        include_disabled: bool,
    },
    Run {
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        lock_file: Option<PathBuf>,
        #[arg(long)]
        drain_marker: Option<PathBuf>,
        #[arg(long)]
        reboot_notice: Option<PathBuf>,
    },
    Disable {
        adapter_id: String,
    },
    Delete {
        adapter_id: String,
    },
}

pub async fn handle_adapter_command(
    root: &Path,
    harness: Arc<dyn Harness>,
    command: AdapterCommands,
) -> Result<()> {
    let store = AdapterStore::new(root.join("adapters"));
    match command {
        AdapterCommands::EnsureHarbor {
            agent,
            conversation,
            name,
            socket_path,
            worker_path,
            tsx_path,
        } => {
            let agent = harness
                .get_agent(&agent)
                .await?
                .ok_or_else(|| anyhow::anyhow!("agent not found"))?;
            let conversation = agent
                .get_conversation(&conversation)
                .await?
                .ok_or_else(|| anyhow::anyhow!("conversation not found"))?;
            let worker_path = worker_path.canonicalize().map_err(|error| {
                anyhow::anyhow!(
                    "failed to resolve Harbor adapter worker {}: {error}",
                    worker_path.display()
                )
            })?;
            let tsx_path = tsx_path.canonicalize().map_err(|error| {
                anyhow::anyhow!(
                    "failed to resolve tsx executable {}: {error}",
                    tsx_path.display()
                )
            })?;
            let socket_path =
                std::path::absolute(socket_path.unwrap_or_else(|| root.join("harbor.sock")))?;
            let state_dir = std::path::absolute(
                root.join("adapters")
                    .join("harbor-state")
                    .join(conversation.record().id.to_string()),
            )?;
            let config = harbor_adapter_config(&tsx_path, &worker_path, &socket_path, &state_dir);
            let existing = store
                .list_adapters_for_conversation(
                    &agent.record().id.to_string(),
                    &conversation.record().id.to_string(),
                    true,
                )
                .await?
                .into_iter()
                .find(|adapter| adapter.name == name);
            let adapter = match existing {
                Some(adapter) if !adapter.enabled => {
                    bail!(
                        "Harbor adapter {} exists but is disabled; delete it before recreating",
                        adapter.id
                    )
                }
                Some(adapter) if adapter.config != config => {
                    bail!(
                        "Harbor adapter {} exists with different configuration",
                        adapter.id
                    )
                }
                Some(adapter) => adapter,
                None => {
                    store
                        .create_adapter(NewAdapter {
                            agent_id: agent.record().id.to_string(),
                            conversation_id: conversation.record().id.to_string(),
                            name,
                            source: AdapterSource::BuiltIn,
                            config,
                        })
                        .await?
                }
            };
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "adapter_id": adapter.id,
                    "socket_path": socket_path,
                }))?
            );
        }
        AdapterCommands::List { include_disabled } => {
            let mut writer = TabWriter::new(std::io::stdout());
            writeln!(writer, "ADAPTER\tENABLED\tSOURCE\tNAME")?;
            for adapter in store
                .list_adapters()
                .await?
                .into_iter()
                .filter(|adapter| include_disabled || adapter.enabled)
            {
                writeln!(
                    writer,
                    "{}\t{}\t{:?}\t{}",
                    adapter.id, adapter.enabled, adapter.source, adapter.name
                )?;
            }
            writer.flush()?;
        }
        AdapterCommands::Run {
            limit,
            lock_file,
            drain_marker,
            reboot_notice,
        } => {
            let _lock = AdapterRunnerLock::acquire(
                lock_file.unwrap_or_else(|| root.join("adapters.lock")),
            )?;
            run_adapters_watch(
                harness,
                store,
                AdapterRunOptions {
                    limit,
                    drain_marker,
                    reboot_notice,
                },
            )
            .await?;
        }
        AdapterCommands::Disable { adapter_id } => {
            if store.disable_adapter(&adapter_id).await?.is_some() {
                println!("disabled adapter {}", adapter_id);
            } else {
                bail!("adapter not found: {adapter_id}");
            }
        }
        AdapterCommands::Delete { adapter_id } => {
            if store.delete_adapter(&adapter_id).await?.is_some() {
                println!("deleted adapter {}", adapter_id);
            } else {
                bail!("adapter not found: {adapter_id}");
            }
        }
    }
    Ok(())
}

fn harbor_adapter_config(
    tsx_path: &Path,
    worker_path: &Path,
    socket_path: &Path,
    state_dir: &Path,
) -> AdapterConfig {
    AdapterConfig {
        adapter_type: "harbor".to_string(),
        worker_command: vec![
            tsx_path.to_string_lossy().into_owned(),
            worker_path.to_string_lossy().into_owned(),
        ],
        initialization: serde_json::json!({
            "socketPath": socket_path,
        }),
        state_dir: Some(state_dir.to_string_lossy().into_owned()),
        secret_env: Vec::new(),
    }
}

#[derive(Debug)]
struct AdapterRunnerLock {
    path: std::path::PathBuf,
}

impl AdapterRunnerLock {
    fn acquire(path: PathBuf) -> Result<Self> {
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
                    bail!(
                        "adapter runner already appears to be running with pid {}",
                        existing_pid.trim()
                    );
                }
                fs::remove_file(&path)?;
                Self::acquire(path)
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for AdapterRunnerLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "failed to remove adapter runner lock"
            );
        }
    }
}

fn process_is_running(pid: &str) -> bool {
    !pid.is_empty()
        && Command::new("kill")
            .arg("-0")
            .arg(pid)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn harbor_adapter_config_uses_explicit_paths() {
        let config = harbor_adapter_config(
            Path::new("/repo/node_modules/.bin/tsx"),
            Path::new("/repo/examples/exo/adapters/harbor/worker.ts"),
            Path::new("/tmp/harbor.sock"),
            Path::new("/tmp/harbor-state"),
        );
        assert_eq!(config.adapter_type, "harbor");
        assert_eq!(
            config.worker_command,
            vec![
                "/repo/node_modules/.bin/tsx",
                "/repo/examples/exo/adapters/harbor/worker.ts"
            ]
        );
        assert_eq!(
            config.initialization,
            serde_json::json!({ "socketPath": "/tmp/harbor.sock" })
        );
        assert_eq!(config.state_dir.as_deref(), Some("/tmp/harbor-state"));
    }

    #[test]
    fn adapter_runner_lock_rejects_concurrent_holder() {
        let tempdir = TempDir::new().unwrap();
        let lock_file = tempdir.path().join("adapters.lock");
        let first = AdapterRunnerLock::acquire(lock_file.clone()).unwrap();

        let error = AdapterRunnerLock::acquire(lock_file.clone()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("adapter runner already appears to be running")
        );

        drop(first);
        AdapterRunnerLock::acquire(lock_file).unwrap();
    }

    #[test]
    fn adapter_runner_lock_reclaims_stale_pid_file() {
        let tempdir = TempDir::new().unwrap();
        let lock_file = tempdir.path().join("adapters.lock");
        fs::write(&lock_file, "999999999").unwrap();

        AdapterRunnerLock::acquire(lock_file).unwrap();
    }
}

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Subcommand;
use executor::{AdapterRunOptions, AdapterStore, Harness, run_adapters_watch};
use tabwriter::TabWriter;

#[derive(Debug, Subcommand)]
pub enum AdapterCommands {
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

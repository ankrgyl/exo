//! Initialize an Excode sandbox pool on Vercel and exercise every sandbox.
//!
//! From the repository root:
//!
//! ```text
//! set -a; source .env; set +a
//! cargo run -p excode --example run_sandbox_pool -- --workers 4 --data hello
//! ```
//!
//! Each worker leases a separate sandbox, writes the supplied data inside it,
//! reads the data back, and prints the sandbox ID and result.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use excode::{EmptySandboxPoolProvisioner, LocalSandboxPool, PoolCapacity, SandboxPoolKey};
use exoharness::{
    ManagedSandboxBackend, SandboxCommand, SandboxNetworkPolicy, SandboxProvider, SandboxSpec,
    VercelConfig, VercelSandboxBackend, default_vercel_image,
};
use futures::future::join_all;
use tokio::sync::watch;

struct Options {
    workers: usize,
    data: String,
}

fn options() -> Result<Options> {
    let mut workers = 4;
    let mut data = "hello from excode".to_string();
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workers" => {
                workers = args
                    .next()
                    .context("--workers requires a positive integer")?
                    .parse()
                    .context("--workers requires a positive integer")?;
            }
            "--data" => {
                data = args.next().context("--data requires a value")?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: cargo run -p excode --example run_sandbox_pool -- [OPTIONS]\n\nOptions:\n  --workers N   Number of sandboxes to initialize and exercise (default: 4)\n  --data TEXT   Data each sandbox writes and reads (default: \"hello from excode\")"
                );
                std::process::exit(0);
            }
            other => bail!("unknown option {other}; use --help for usage"),
        }
    }

    if workers == 0 {
        bail!("--workers must be positive");
    }
    Ok(Options { workers, data })
}

fn required_env(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} is required"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = options()?;
    let token = env::var("VERCEL_API_TOKEN").or_else(|_| required_env("VERCEL_TOKEN"))?;
    let team_id = required_env("VERCEL_TEAM_ID")?;
    let project_id = required_env("VERCEL_PROJECT_ID")?;
    let api_url =
        env::var("VERCEL_API_URL").unwrap_or_else(|_| "https://vercel.com/api".to_string());
    let image = env::var("VERCEL_IMAGE").unwrap_or_else(|_| default_vercel_image());

    let backend: Arc<dyn ManagedSandboxBackend> =
        Arc::new(VercelSandboxBackend::new(VercelConfig {
            api_token: token,
            api_url,
            team_id,
            project_id,
        })?);
    let pool = Arc::new(LocalSandboxPool::new(
        SandboxPoolKey {
            pool_id: format!("excode-example-{}", std::process::id()),
            provider: SandboxProvider::Vercel,
            spec: SandboxSpec {
                image,
                resources: Default::default(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                network: SandboxNetworkPolicy::Enabled,
                default_workdir: "/vercel/sandbox".to_string(),
            },
        },
        backend,
        PoolCapacity {
            min_ready: 0,
            target_ready: options.workers,
            max_total: options.workers,
            lease_ttl: Duration::from_secs(300),
            idle_ttl: Duration::from_secs(30),
        },
        Arc::new(EmptySandboxPoolProvisioner),
    )?);

    // Surface authentication/provider errors before workers wait for capacity.
    pool.reconcile_once()
        .await
        .context("initializing the Vercel sandbox pool")?;

    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let reconciler = {
        let pool = Arc::clone(&pool);
        tokio::spawn(async move { pool.run_reconciler(shutdown_receiver).await })
    };

    println!("initialized {} Excode sandboxes", options.workers);
    let tasks = (0..options.workers)
        .map(|worker| {
            let pool = Arc::clone(&pool);
            let data = options.data.clone();
            tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(180), async move {
                    let (lease, sandbox) = pool.acquire_any(format!("worker-{worker}")).await?;
                    let sandbox_id = sandbox.id().to_string();
                    let path = format!("/tmp/excode-worker-{worker}.txt");
                    let mut env = HashMap::new();
                    env.insert("EXCODE_TEST_DATA".to_string(), data);
                    let command = SandboxCommand {
                        argv: vec![
                            "/bin/sh".to_string(),
                            "-lc".to_string(),
                            format!("printf '%s\\n' \"$EXCODE_TEST_DATA\" > {path}; cat {path}"),
                        ],
                        env,
                        display_argv: None,
                        cwd: None,
                        timeout: None,
                    };
                    let output_result = sandbox.exec(&command).await;
                    let release_result = pool.release(&lease).await;
                    release_result?;
                    let output = output_result?;
                    Ok::<_, anyhow::Error>((worker, sandbox_id, output.stdout))
                })
                .await
                .context("sandbox worker timed out")?
            })
        })
        .collect::<Vec<_>>();

    for result in join_all(tasks).await {
        let (worker, sandbox_id, data) = result.context("sandbox worker task failed")??;
        println!("worker {worker} -> {sandbox_id}: {data}");
    }

    shutdown_sender
        .send(true)
        .context("stopping sandbox pool reconciler")?;
    reconciler
        .await
        .context("sandbox pool reconciler task failed")?;
    pool.drain().await.context("stopping sandbox pool")?;
    println!("all sandboxes stopped");
    Ok(())
}

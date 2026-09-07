//! Real-provider lifecycle coverage for `SandboxPool`.
//!
//! The integration workflow selects `local-process` and Docker through
//! `EXO_TEST_SANDBOX_BACKEND` and runs ignored tests. Run locally with:
//!
//! ```text
//! EXO_TEST_SANDBOX_BACKEND=local-process cargo test -p exo --test sandbox_pool_e2e -- --ignored
//! EXO_TEST_SANDBOX_BACKEND=docker cargo test -p exo --test sandbox_pool_e2e -- --ignored
//! ```

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;
use exoharness::{
    CliContainerSandboxBackend, EmptySandboxPoolRecipe, LeasedSandbox, LocalProcessSandboxBackend,
    ManagedSandboxBackend, PoolCapacity, SandboxCommand, SandboxNetworkPolicy, SandboxPool,
    SandboxPoolKey, SandboxPoolRecipe, SandboxProvider, SandboxSpec,
};
use tokio::sync::watch;
use tokio::time::{self, timeout};

const SANDBOX_IMAGE: &str = "docker.io/library/ubuntu:24.04";
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(180);

fn backend_from_environment() -> Option<(SandboxProvider, Arc<dyn ManagedSandboxBackend>, String)> {
    match std::env::var("EXO_TEST_SANDBOX_BACKEND")
        .unwrap_or_else(|_| "docker".to_string())
        .as_str()
    {
        "local-process" => Some((
            SandboxProvider::LocalProcess,
            Arc::new(LocalProcessSandboxBackend::new()),
            "/".to_string(),
        )),
        "docker" => {
            let available = Command::new("docker")
                .arg("info")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false);
            if !available {
                eprintln!("skipping sandbox pool e2e: docker is not available");
                return None;
            }
            Some((
                SandboxProvider::Docker,
                Arc::new(CliContainerSandboxBackend::docker()),
                "/".to_string(),
            ))
        }
        other => panic!("unknown EXO_TEST_SANDBOX_BACKEND={other}"),
    }
}

fn pool(
    provider: SandboxProvider,
    backend: Arc<dyn ManagedSandboxBackend>,
    default_workdir: String,
    recipe: Arc<dyn SandboxPoolRecipe>,
) -> SandboxPool {
    SandboxPool::new(
        SandboxPoolKey {
            pool_id: "sandbox-pool-e2e".to_string(),
            provider,
            spec: SandboxSpec {
                image: SANDBOX_IMAGE.to_string(),
                resources: Default::default(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                network: SandboxNetworkPolicy::Disabled,
                default_workdir,
            },
        },
        backend,
        PoolCapacity {
            min_ready: 0,
            target_ready: 2,
            max_total: 2,
            lease_ttl: Duration::from_secs(60),
            idle_ttl: Duration::from_secs(120),
        },
        recipe,
    )
    .expect("valid pool capacity")
}

struct CommandSeeder;

#[async_trait]
impl SandboxPoolRecipe for CommandSeeder {
    async fn acquire(
        &self,
        backend: &dyn ManagedSandboxBackend,
        request: exoharness::SandboxRequest,
    ) -> exoharness::Result<Arc<dyn exoharness::ManagedSandboxHandle>> {
        let sandbox = backend.acquire(request.clone()).await?;
        let output = sandbox
            .exec(&SandboxCommand {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf recipe-seeded > /tmp/exo-pool-recipe-marker".to_string(),
                ],
                env: HashMap::new(),
                display_argv: None,
                cwd: None,
                timeout: Some(Duration::from_secs(20)),
            })
            .await?;
        if !output.ok {
            backend.terminate(request).await?;
            bail!("pool recipe seed command failed: {}", output.stderr.trim());
        }
        Ok(sandbox)
    }
}

/// Helper to print out a value for testing
fn command(value: &str) -> SandboxCommand {
    SandboxCommand {
        argv: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("printf '{value}'"),
        ],
        env: HashMap::new(),
        display_argv: None,
        cwd: None,
        timeout: Some(Duration::from_secs(20)),
    }
}

fn running_docker_container(sandbox: &LeasedSandbox) -> String {
    let key = sandbox
        .id()
        .strip_prefix("warm:")
        .expect("Docker pool runtime should be warm");
    let filter = format!("label=exo.sandbox.key={key}");
    let output = Command::new("docker")
        .args(["ps", "-q", "--filter", &filter])
        .output()
        .expect("docker ps should run");
    assert!(
        output.status.success(),
        "docker ps failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let containers = stdout
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        containers.len(),
        1,
        "expected one running Docker container for {key}, found {containers:?}"
    );
    containers[0].to_string()
}

async fn acquire(pool: &SandboxPool, worker: &str) -> (exoharness::SandboxLease, LeasedSandbox) {
    timeout(ACQUIRE_TIMEOUT, pool.acquire_any(worker))
        .await
        .expect("pool acquisition timed out")
        .expect("pool acquisition failed")
}

#[tokio::test]
#[ignore = "spawns real local-process or Docker sandboxes; CI runs ignored integration tests"]
async fn warm_pool_acquires_executes_replaces_and_drains() {
    let Some((provider, backend, default_workdir)) = backend_from_environment() else {
        return;
    };
    let pool = Arc::new(pool(
        provider,
        backend,
        default_workdir,
        Arc::new(EmptySandboxPoolRecipe),
    ));
    let (shutdown, receiver) = watch::channel(false);
    let reconciler = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { pool.run_reconciler(receiver).await }
    });

    let (first_lease, first) = acquire(&pool, "worker-one").await;
    let (second_lease, second) = acquire(&pool, "worker-two").await;
    assert_ne!(
        first.id(),
        second.id(),
        "the warm pool should provide distinct runtimes"
    );
    assert_eq!(first.exec(&command("first")).await.unwrap().stdout, "first");
    assert_eq!(
        second.exec(&command("second")).await.unwrap().stdout,
        "second"
    );
    pool.heartbeat(&first_lease).await.unwrap();

    let retired_id = first.id().to_string();
    pool.release(&first_lease).await.unwrap();
    assert!(first.exec(&command("stale")).await.is_err());

    let (replacement_lease, replacement) = acquire(&pool, "worker-three").await;
    assert_ne!(
        replacement.id(),
        retired_id,
        "released runtime must be replaced"
    );
    assert_eq!(
        replacement
            .exec(&command("replacement"))
            .await
            .unwrap()
            .stdout,
        "replacement"
    );

    pool.release(&second_lease).await.unwrap();
    pool.release(&replacement_lease).await.unwrap();
    pool.drain().await.unwrap();
    assert_eq!(pool.entry_count().await, 0);

    shutdown.send(true).unwrap();
    time::timeout(Duration::from_secs(10), reconciler)
        .await
        .expect("reconciler should stop")
        .expect("reconciler task should not panic");
}

#[tokio::test]
#[ignore = "spawns real local-process or Docker sandboxes; CI runs ignored integration tests"]
async fn recipe_seeded_pool_exposes_the_initialized_filesystem() {
    let Some((provider, backend, default_workdir)) = backend_from_environment() else {
        return;
    };
    let pool = Arc::new(pool(
        provider,
        backend,
        default_workdir,
        Arc::new(CommandSeeder),
    ));
    let (shutdown, receiver) = watch::channel(false);
    let reconciler = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { pool.run_reconciler(receiver).await }
    });

    let (lease, sandbox) = acquire(&pool, "recipe-worker").await;
    assert_eq!(
        sandbox
            .exec(&SandboxCommand {
                argv: vec![
                    "/bin/cat".to_string(),
                    "/tmp/exo-pool-recipe-marker".to_string()
                ],
                env: HashMap::new(),
                display_argv: None,
                cwd: None,
                timeout: Some(Duration::from_secs(20)),
            })
            .await
            .unwrap()
            .stdout,
        "recipe-seeded"
    );

    pool.release(&lease).await.unwrap();
    pool.drain().await.unwrap();
    shutdown.send(true).unwrap();
    time::timeout(Duration::from_secs(10), reconciler)
        .await
        .expect("reconciler should stop")
        .expect("reconciler task should not panic");
}

#[tokio::test]
#[ignore = "spawns and kills a real Docker sandbox; CI runs ignored integration tests"]
async fn docker_runtime_loss_is_recovered_for_an_active_pool_lease() {
    let Some((provider, backend, default_workdir)) = backend_from_environment() else {
        return;
    };
    if provider != SandboxProvider::Docker {
        eprintln!("skipping Docker runtime-loss test for {provider}");
        return;
    }

    let pool = Arc::new(pool(
        provider,
        backend,
        default_workdir,
        Arc::new(EmptySandboxPoolRecipe),
    ));
    let (shutdown, receiver) = watch::channel(false);
    let reconciler = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { pool.run_reconciler(receiver).await }
    });

    let (lease, sandbox) = acquire(&pool, "runtime-loss-worker").await;
    assert_eq!(
        sandbox.exec(&command("before")).await.unwrap().stdout,
        "before"
    );
    let old_container = running_docker_container(&sandbox);
    let killed = Command::new("docker")
        .args(["kill", &old_container])
        .output()
        .expect("docker kill should run");
    assert!(
        killed.status.success(),
        "docker kill failed: {}",
        String::from_utf8_lossy(&killed.stderr)
    );

    assert_eq!(
        sandbox.exec(&command("after")).await.unwrap().stdout,
        "after"
    );
    let replacement_container = running_docker_container(&sandbox);
    assert_ne!(old_container, replacement_container);
    pool.heartbeat(&lease).await.unwrap();

    pool.release(&lease).await.unwrap();
    pool.drain().await.unwrap();
    shutdown.send(true).unwrap();
    time::timeout(Duration::from_secs(10), reconciler)
        .await
        .expect("reconciler should stop")
        .expect("reconciler task should not panic");
}

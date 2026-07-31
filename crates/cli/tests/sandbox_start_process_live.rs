//! Live `start_process` contract tests for native streaming backends.
//!
//! E2B:
//! `E2B_API_KEY=... E2B_SECURE=0 E2B_TEMPLATE_ID=base cargo test -p exo --test sandbox_start_process_live e2b_ -- --ignored --nocapture`
//!
//! Sprites (set `SPRITES_ORGANIZATION` when your token spans multiple orgs):
//! `SPRITES_TOKEN=... cargo test -p exo --test sandbox_start_process_live sprites_ -- --ignored --nocapture`
//!
//! Tensorlake:
//! `TENSORLAKE_API_KEY=... cargo test -p exo --test sandbox_start_process_live tensorlake_ -- --ignored --nocapture`

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use exoharness::{
    E2bConfig, E2bSandboxBackend, ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand,
    SandboxKey, SandboxLifecycleConfig, SandboxNetworkPolicy, SandboxRequest, SandboxSpec,
    SpritesConfig, SpritesSandboxBackend, TensorlakeConfig, TensorlakeSandboxBackend,
};
use futures::io::AsyncReadExt;
use tokio::time::timeout;

fn e2b_template_id() -> String {
    env::var("E2B_TEMPLATE_ID").unwrap_or_else(|_| "base".into())
}

fn live_provider_secret(provider: &str, secret_name: &str) -> Option<String> {
    match env::var(secret_name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => {
            eprintln!("skipping real {provider} start_process test: {secret_name} is not set");
            None
        }
    }
}

fn make_e2b_request(conversation_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: conversation_id.into(),
            sandbox_id: sandbox_id.into(),
        },
        spec: SandboxSpec {
            image: e2b_template_id(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            network: SandboxNetworkPolicy::Enabled,
            default_workdir: "/home/user".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

fn e2b_backend_from_env() -> Option<E2bSandboxBackend> {
    let api_key = live_provider_secret("e2b", "E2B_API_KEY")?;
    let template_id = e2b_template_id();
    Some(
        E2bSandboxBackend::new(E2bConfig {
            api_key,
            api_url: exoharness::DEFAULT_E2B_API_URL.into(),
            template_id,
            envd_port: exoharness::DEFAULT_E2B_ENVD_PORT,
            envd_base_url: None,
            secure: env::var("E2B_SECURE")
                .ok()
                .is_some_and(|value| value != "0"),
        })
        .expect("E2bSandboxBackend::new"),
    )
}

fn sprites_config_from_env() -> Option<SpritesConfig> {
    Some(SpritesConfig {
        token: live_provider_secret("sprites", "SPRITES_TOKEN")?,
        api_url: env::var("SPRITES_API_URL")
            .unwrap_or_else(|_| exoharness::DEFAULT_SPRITES_API_URL.into()),
        url_auth: env::var("SPRITES_URL_AUTH").ok(),
        organization: env::var("SPRITES_ORGANIZATION").ok(),
        extra_labels: Vec::new(),
    })
}

fn make_sprites_request(conversation_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: conversation_id.into(),
            sandbox_id: sandbox_id.into(),
        },
        spec: SandboxSpec {
            image: "default".into(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            network: SandboxNetworkPolicy::Enabled,
            default_workdir: "/home/sprite".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

fn tensorlake_image() -> String {
    env::var("TENSORLAKE_IMAGE").unwrap_or_else(|_| exoharness::DEFAULT_TENSORLAKE_IMAGE.into())
}

fn tensorlake_config_from_env() -> Option<TensorlakeConfig> {
    Some(TensorlakeConfig {
        api_key: live_provider_secret("tensorlake", "TENSORLAKE_API_KEY")?,
        api_url: env::var("TENSORLAKE_API_URL")
            .unwrap_or_else(|_| exoharness::DEFAULT_TENSORLAKE_API_URL.into()),
        default_image: tensorlake_image(),
        cpus: None,
        memory_mb: None,
        sandbox_base_url: None,
    })
}

fn make_tensorlake_request(conversation_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: conversation_id.into(),
            sandbox_id: sandbox_id.into(),
        },
        spec: SandboxSpec {
            image: tensorlake_image(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            network: SandboxNetworkPolicy::Enabled,
            default_workdir: "/workspace".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

#[tokio::test]
#[ignore = "requires E2B_API_KEY"]
async fn e2b_start_process_streams_incrementally() {
    let Some(backend) = e2b_backend_from_env() else {
        return;
    };

    let handle = backend
        .acquire(make_e2b_request("live-e2b-stream", "sandbox-live-stream"))
        .await
        .expect("acquire E2B sandbox");
    assert_streaming_script(handle, "E2B", "/home/user").await;
}

#[tokio::test]
#[ignore = "requires SPRITES_TOKEN"]
async fn sprites_start_process_streams_incrementally() {
    let Some(config) = sprites_config_from_env() else {
        return;
    };
    let backend = SpritesSandboxBackend::new(config).expect("SpritesSandboxBackend::new");

    let handle = backend
        .acquire(make_sprites_request(
            "live-sprites-stream",
            "sandbox-live-stream",
        ))
        .await
        .expect("acquire Sprites sprite");
    assert_streaming_script(handle, "Sprites", "/home/sprite").await;
}

#[tokio::test]
#[ignore = "requires E2B_API_KEY"]
async fn e2b_start_process_contract() {
    let Some(backend) = e2b_backend_from_env() else {
        return;
    };

    let handle = backend
        .acquire(make_e2b_request(
            "live-e2b-contract",
            "sandbox-live-contract",
        ))
        .await
        .expect("acquire E2B sandbox");
    exoharness::contract_tests::sandbox_handle_start_process_supports_interactive_stdio_and_env(
        handle,
    )
    .await
    .expect("E2B start_process contract");
}

#[tokio::test]
#[ignore = "requires SPRITES_TOKEN"]
async fn sprites_start_process_contract() {
    let Some(config) = sprites_config_from_env() else {
        return;
    };
    let backend = SpritesSandboxBackend::new(config).expect("SpritesSandboxBackend::new");

    let handle = backend
        .acquire(make_sprites_request(
            "live-sprites-contract",
            "sandbox-live-contract",
        ))
        .await
        .expect("acquire Sprites sprite");
    exoharness::contract_tests::sandbox_handle_start_process_supports_interactive_stdio_and_env(
        handle,
    )
    .await
    .expect("Sprites start_process contract");
}

#[tokio::test]
#[ignore = "requires TENSORLAKE_API_KEY"]
async fn tensorlake_start_process_streams_incrementally() {
    let Some(config) = tensorlake_config_from_env() else {
        return;
    };
    let backend = TensorlakeSandboxBackend::new(config).expect("TensorlakeSandboxBackend::new");

    let handle = backend
        .acquire(make_tensorlake_request(
            "live-tensorlake-stream",
            "sandbox-live-stream",
        ))
        .await
        .expect("acquire Tensorlake sandbox");
    assert_streaming_script(handle, "Tensorlake", "/workspace").await;
}

#[tokio::test]
#[ignore = "requires TENSORLAKE_API_KEY"]
async fn tensorlake_start_process_contract() {
    let Some(config) = tensorlake_config_from_env() else {
        return;
    };
    let backend = TensorlakeSandboxBackend::new(config).expect("TensorlakeSandboxBackend::new");

    let handle = backend
        .acquire(make_tensorlake_request(
            "live-tensorlake-contract",
            "sandbox-live-contract",
        ))
        .await
        .expect("acquire Tensorlake sandbox");
    exoharness::contract_tests::sandbox_handle_start_process_supports_interactive_stdio_and_env(
        handle,
    )
    .await
    .expect("Tensorlake start_process contract");
}

async fn assert_streaming_script(handle: Arc<dyn ManagedSandboxHandle>, provider: &str, cwd: &str) {
    let mut process = handle
        .start_process(&SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf 'first\\n'; sleep 1; printf 'second\\n'".to_string(),
            ],
            env: HashMap::new(),
            display_argv: None,
            cwd: Some(cwd.into()),
            timeout: Some(Duration::from_secs(30)),
        })
        .await
        .expect("start_process");

    let mut first = [0u8; 6];
    timeout(
        Duration::from_secs(10),
        process.stdout.read_exact(&mut first),
    )
    .await
    .expect("first chunk should arrive quickly")
    .expect("read first chunk");
    assert_eq!(
        std::str::from_utf8(&first).expect("utf8"),
        "first\n",
        "{provider} should stream the first line before the process exits"
    );

    let started = Instant::now();
    let mut second = [0u8; 7];
    timeout(
        Duration::from_secs(10),
        process.stdout.read_exact(&mut second),
    )
    .await
    .expect("second chunk should arrive after sleep")
    .expect("read second chunk");
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "{provider} second line arrived too quickly; output may have been buffered"
    );
    assert_eq!(
        std::str::from_utf8(&second).expect("utf8"),
        "second\n",
        "{provider} should stream the second line"
    );

    let exit_code = timeout(Duration::from_secs(30), process.wait)
        .await
        .expect("process should exit")
        .expect("wait");
    assert_eq!(exit_code, 0, "{provider} process should exit successfully");

    println!("{provider} streaming start_process ok");
}

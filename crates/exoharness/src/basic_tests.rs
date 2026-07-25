use std::env;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, Cursor};
use lingua::Message;
use lingua::universal::{AssistantContent, UserContent};
use serde_json::Value;
use tempfile::TempDir;
use tokio::fs;
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio::time::{sleep, timeout};

use crate::test_support::{local_test_config, local_test_config_with_daytona};
use crate::{
    Artifact, ArtifactVersion, BasicExoHarness, BeginTurnRequest, Binding, BoxAsyncRead,
    BoxAsyncWrite, CloseSandboxProcessInputRequest, CreateSandboxRequest, DurableFileSystem,
    EventData, EventKind, EventQuery, EventQueryDirection, ExoHarness, FileSystemMountMode,
    ForkConversationRequest, ManagedSandboxBackend, ManagedSandboxHandle, NewAgentRequest,
    NewConversationRequest, PutSecretRequest, RunInSandboxRequest, SandboxCommand,
    SandboxCommandOutput, SandboxKey, SandboxLifecycleConfig, SandboxNetworkPolicy,
    SandboxProcessEvent, SandboxProcessEventQuery, SandboxProcessParts, SandboxProcessStatus,
    SandboxProcessStdin, SandboxProvider, SandboxProviderConfig, SandboxRequest, SandboxSpec,
    Secret, SecretBackendChoice, SnapshotKind, SnapshotPayload, StartSandboxProcessRequest,
    StartSandboxRequest, Uuid7, WaitSandboxProcessRequest, WriteArtifactRequest,
    WriteSandboxProcessInputRequest,
};

const DEFAULT_DURABLE_CONTRACT_MOUNT_PATH: &str = "/home/exo/workspace";
#[cfg(feature = "aws-agentcore")]
const DEFAULT_AGENTCORE_DURABLE_CONTRACT_MOUNT_PATH: &str = "/mnt/workspace";

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_supports_agent_and_conversation_crud() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness: std::sync::Arc<dyn ExoHarness> = std::sync::Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path()))
            .await
            .expect("harness should initialize"),
    );
    crate::contract_tests::supports_agent_and_conversation_crud(harness).await;
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_lists_conversations_recent_first_and_paginates() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness: std::sync::Arc<dyn ExoHarness> = std::sync::Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path()))
            .await
            .expect("harness should initialize"),
    );
    crate::contract_tests::list_conversations_returns_recent_first_and_paginates(harness).await;
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_contract_begin_turn_tracks_events_through_finish() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness: std::sync::Arc<dyn ExoHarness> = std::sync::Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path()))
            .await
            .expect("harness should initialize"),
    );
    crate::contract_tests::begin_turn_tracks_events_through_finish(harness).await;
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_contract_turn_events_continue_after_artifact_writes() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness: std::sync::Arc<dyn ExoHarness> = std::sync::Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path()))
            .await
            .expect("harness should initialize"),
    );
    crate::contract_tests::turn_events_continue_after_artifact_writes(harness).await;
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_contract_conversation_scope_overrides_and_forks() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness: std::sync::Arc<dyn ExoHarness> = std::sync::Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path()))
            .await
            .expect("harness should initialize"),
    );
    crate::contract_tests::conversation_scope_overrides_agent_scope_and_fork_copies_bindings(
        harness,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn local_process_sandbox_contract_start_process_stdio_and_env() {
    let tempdir = TempDir::new().expect("tempdir");
    let handle = local_process_contract_handle(&tempdir, "stdio-and-env").await;
    crate::contract_tests::sandbox_handle_start_process_supports_interactive_stdio_and_env(handle)
        .await
        .expect("sandbox start_process contract");
}

#[tokio::test(flavor = "current_thread")]
async fn local_process_sandbox_contract_start_process_long_running_protocol() {
    let tempdir = TempDir::new().expect("tempdir");
    let handle = local_process_contract_handle(&tempdir, "long-running-protocol").await;
    crate::contract_tests::sandbox_handle_start_process_supports_long_running_request_response_protocol(
        handle,
    )
    .await
    .expect("sandbox long-running protocol contract");
}

#[tokio::test(flavor = "current_thread")]
async fn local_process_sandbox_rejects_durable_file_systems() {
    let tempdir = TempDir::new().expect("tempdir");
    let backend: Arc<dyn ManagedSandboxBackend> =
        Arc::new(crate::LocalProcessSandboxBackend::new());
    let result = backend
        .acquire(durable_provider_contract_request(
            "local-process",
            "durable-file-system",
            "local-process".to_string(),
            &tempdir.path().display().to_string(),
        ))
        .await;
    match result {
        Ok(handle) => panic!(
            "local-process unexpectedly acquired durable filesystem sandbox {}",
            handle.id()
        ),
        Err(error) => assert!(
            error
                .to_string()
                .contains("does not support durable file systems")
        ),
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "uses a real Daytona sandbox; source sandbox-vars.sh and run this test explicitly"]
async fn daytona_sandbox_contract_start_process_stdio_and_env() {
    let Some(handle) = daytona_contract_handle("stdio-and-env").await else {
        return;
    };
    crate::contract_tests::sandbox_handle_start_process_supports_interactive_stdio_and_env(handle)
        .await
        .expect("Daytona sandbox start_process contract");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "uses a real Daytona sandbox; source sandbox-vars.sh and run this test explicitly"]
async fn daytona_sandbox_contract_start_process_long_running_protocol() {
    let Some(handle) = daytona_contract_handle("long-running-protocol").await else {
        return;
    };
    crate::contract_tests::sandbox_handle_start_process_supports_long_running_request_response_protocol(
        handle,
    )
    .await
    .expect("Daytona sandbox long-running protocol contract");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "uses a real Vercel sandbox; source sandbox-vars.sh and run this test explicitly"]
async fn vercel_sandbox_contract_start_process_stdio_and_env() {
    let Some(handle) = vercel_contract_handle("stdio-and-env").await else {
        return;
    };
    crate::contract_tests::sandbox_handle_start_process_supports_interactive_stdio_and_env(handle)
        .await
        .expect("Vercel sandbox start_process contract");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "uses a real Vercel sandbox; source sandbox-vars.sh and run this test explicitly"]
async fn vercel_sandbox_contract_start_process_long_running_protocol() {
    let Some(handle) = vercel_contract_handle("long-running-protocol").await else {
        return;
    };
    crate::contract_tests::sandbox_handle_start_process_supports_long_running_request_response_protocol(
        handle,
    )
    .await
    .expect("Vercel sandbox long-running protocol contract");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "uses a real Docker sandbox; run this test explicitly"]
async fn docker_sandbox_contract_durable_file_system_survives_stop_and_reacquire() {
    let tempdir = TempDir::new().expect("tempdir");
    let backend: Arc<dyn ManagedSandboxBackend> = Arc::new(
        crate::CliContainerSandboxBackend::docker()
            .with_durable_file_system_root(tempdir.path().join("durable-filesystems")),
    );
    let mount_path = durable_contract_mount_path();
    crate::contract_tests::sandbox_backend_durable_file_system_survives_stop_and_reacquire(
        backend,
        durable_provider_contract_request(
            "docker",
            "durable-file-system",
            env_or("DOCKER_IMAGE", &crate::default_docker_image()),
            &mount_path,
        ),
    )
    .await
    .expect("Docker sandbox durable filesystem contract");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "uses a real Apple container sandbox; run this test explicitly"]
async fn apple_container_sandbox_contract_durable_file_system_survives_stop_and_reacquire() {
    let tempdir = TempDir::new().expect("tempdir");
    let backend: Arc<dyn ManagedSandboxBackend> = Arc::new(
        crate::CliContainerSandboxBackend::apple_container()
            .with_durable_file_system_root(tempdir.path().join("durable-filesystems")),
    );
    let mount_path = durable_contract_mount_path();
    crate::contract_tests::sandbox_backend_durable_file_system_survives_stop_and_reacquire(
        backend,
        durable_provider_contract_request(
            "apple-container",
            "durable-file-system",
            env_or("APPLE_CONTAINER_IMAGE", &crate::default_docker_image()),
            &mount_path,
        ),
    )
    .await
    .expect("Apple container sandbox durable filesystem contract");
}

#[cfg(feature = "aws-agentcore")]
#[tokio::test(flavor = "current_thread")]
#[ignore = "uses a real AgentCore runtime configured with managed session storage at EXO_AGENTCORE_DURABLE_CONTRACT_MOUNT_PATH or /mnt/workspace; run this test explicitly"]
async fn aws_agentcore_sandbox_contract_durable_file_system_survives_stop_and_reacquire() {
    let Some(backend) = aws_agentcore_contract_backend().await else {
        return;
    };
    let mount_path = agentcore_durable_contract_mount_path();
    crate::contract_tests::sandbox_backend_durable_file_system_survives_stop_and_reacquire(
        backend,
        durable_provider_contract_request(
            "aws-agentcore",
            "durable-file-system",
            env_or("AGENTCORE_IMAGE", &crate::default_aws_agentcore_image()),
            &mount_path,
        ),
    )
    .await
    .expect("AgentCore sandbox durable filesystem contract");
}

async fn local_process_contract_handle(
    tempdir: &TempDir,
    sandbox_id: &str,
) -> Arc<dyn ManagedSandboxHandle> {
    let backend: Arc<dyn ManagedSandboxBackend> =
        Arc::new(crate::LocalProcessSandboxBackend::new());
    backend
        .acquire(SandboxRequest {
            key: SandboxKey::ConversationSandbox {
                conversation_id: Uuid7::now().to_string(),
                sandbox_id: sandbox_id.to_string(),
            },
            spec: SandboxSpec {
                image: "local-process".to_string(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                network: SandboxNetworkPolicy::Enabled,
                default_workdir: tempdir.path().display().to_string(),
            },
            lifecycle: SandboxLifecycleConfig::default(),
            provider_state: None,
        })
        .await
        .expect("acquire sandbox")
}

async fn daytona_contract_handle(contract: &str) -> Option<Arc<dyn ManagedSandboxHandle>> {
    let Some(api_key) = nonempty_env("DAYTONA_API_KEY") else {
        eprintln!("skipping real Daytona sandbox contract: DAYTONA_API_KEY is not set");
        return None;
    };
    let backend: Arc<dyn ManagedSandboxBackend> = Arc::new(
        crate::DaytonaSandboxBackend::new(crate::DaytonaConfig {
            api_key,
            api_url: env_or("DAYTONA_API_URL", crate::DEFAULT_DAYTONA_API_URL),
            toolbox_url: env_or("DAYTONA_TOOLBOX_URL", crate::DEFAULT_DAYTONA_TOOLBOX_URL),
            target: nonempty_env("DAYTONA_TARGET"),
            organization_id: nonempty_env("DAYTONA_ORGANIZATION_ID"),
        })
        .expect("DaytonaSandboxBackend::new"),
    );
    Some(
        backend
            .acquire(provider_contract_request(
                "daytona",
                contract,
                env_or("DAYTONA_IMAGE", &crate::default_daytona_image()),
                "/",
            ))
            .await
            .expect("acquire Daytona sandbox"),
    )
}

async fn vercel_contract_handle(contract: &str) -> Option<Arc<dyn ManagedSandboxHandle>> {
    let Some(api_token) = nonempty_env("VERCEL_API_TOKEN").or_else(|| nonempty_env("VERCEL_TOKEN"))
    else {
        eprintln!(
            "skipping real Vercel sandbox contract: VERCEL_API_TOKEN or VERCEL_TOKEN is not set"
        );
        return None;
    };
    let Some(team_id) = nonempty_env("VERCEL_TEAM_ID") else {
        eprintln!("skipping real Vercel sandbox contract: VERCEL_TEAM_ID is not set");
        return None;
    };
    let Some(project_id) = nonempty_env("VERCEL_PROJECT_ID") else {
        eprintln!("skipping real Vercel sandbox contract: VERCEL_PROJECT_ID is not set");
        return None;
    };
    let backend: Arc<dyn ManagedSandboxBackend> = Arc::new(
        crate::VercelSandboxBackend::new(crate::VercelConfig {
            api_token,
            api_url: env_or("VERCEL_API_URL", crate::DEFAULT_VERCEL_API_URL),
            team_id,
            project_id,
        })
        .expect("VercelSandboxBackend::new"),
    );
    Some(
        backend
            .acquire(provider_contract_request(
                "vercel",
                contract,
                env_or("VERCEL_IMAGE", &crate::default_vercel_image()),
                "/vercel/sandbox",
            ))
            .await
            .expect("acquire Vercel sandbox"),
    )
}

#[cfg(feature = "aws-agentcore")]
async fn aws_agentcore_contract_backend() -> Option<Arc<dyn ManagedSandboxBackend>> {
    let Some(runtime_arn) =
        nonempty_env("AGENTCORE_RUNTIME_ARN").or_else(|| nonempty_env("AWS_AGENTCORE_RUNTIME_ARN"))
    else {
        eprintln!(
            "skipping real AgentCore sandbox contract: AGENTCORE_RUNTIME_ARN or AWS_AGENTCORE_RUNTIME_ARN is not set"
        );
        return None;
    };
    let Some(region) = nonempty_env("AWS_AGENTCORE_REGION")
        .or_else(|| nonempty_env("AWS_REGION"))
        .or_else(|| nonempty_env("AWS_DEFAULT_REGION"))
    else {
        eprintln!(
            "skipping real AgentCore sandbox contract: AWS_AGENTCORE_REGION or AWS_REGION is not set"
        );
        return None;
    };
    let credentials = match (
        nonempty_env("AWS_ACCESS_KEY_ID"),
        nonempty_env("AWS_SECRET_ACCESS_KEY"),
    ) {
        (Some(access_key_id), Some(secret_access_key)) => Some(crate::AwsAgentCoreCredentials {
            access_key_id,
            secret_access_key,
            session_token: nonempty_env("AWS_SESSION_TOKEN"),
        }),
        (None, None) => None,
        _ => {
            eprintln!(
                "skipping real AgentCore sandbox contract: set both AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or neither"
            );
            return None;
        }
    };
    let backend = crate::AwsAgentCoreSandboxBackend::new(crate::AwsAgentCoreConfig {
        runtime_arn,
        region,
        qualifier: nonempty_env("AGENTCORE_QUALIFIER")
            .or_else(|| nonempty_env("AWS_AGENTCORE_QUALIFIER")),
        endpoint_url: nonempty_env("AGENTCORE_ENDPOINT_URL")
            .or_else(|| nonempty_env("AWS_AGENTCORE_ENDPOINT_URL")),
        credentials,
        session_storage_mount_path: Some(agentcore_durable_contract_mount_path()),
    })
    .await
    .expect("AwsAgentCoreSandboxBackend::new");
    Some(Arc::new(backend))
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_or(name: &str, default: &str) -> String {
    nonempty_env(name).unwrap_or_else(|| default.to_string())
}

fn provider_contract_request(
    provider: &str,
    contract: &str,
    image: String,
    default_workdir: &str,
) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: Uuid7::now().to_string(),
            sandbox_id: format!("{provider}-{contract}-contract"),
        },
        spec: SandboxSpec {
            image,
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            network: SandboxNetworkPolicy::Enabled,
            default_workdir: default_workdir.to_string(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

fn durable_provider_contract_request(
    provider: &str,
    contract: &str,
    image: String,
    mount_path: &str,
) -> SandboxRequest {
    let mut request = provider_contract_request(provider, contract, image, mount_path);
    request.spec.durable_file_systems = vec![DurableFileSystem {
        name: "workspace".to_string(),
        mount_path: mount_path.to_string(),
        mode: FileSystemMountMode::ReadWrite,
    }];
    request
}

fn durable_contract_mount_path() -> String {
    env_or(
        "EXO_DURABLE_CONTRACT_MOUNT_PATH",
        DEFAULT_DURABLE_CONTRACT_MOUNT_PATH,
    )
}

#[cfg(feature = "aws-agentcore")]
fn agentcore_durable_contract_mount_path() -> String {
    nonempty_env("EXO_AGENTCORE_DURABLE_CONTRACT_MOUNT_PATH")
        .or_else(|| nonempty_env("AWS_AGENTCORE_SESSION_STORAGE_MOUNT_PATH"))
        .or_else(|| nonempty_env("AGENTCORE_SESSION_STORAGE_MOUNT_PATH"))
        .unwrap_or_else(|| DEFAULT_AGENTCORE_DURABLE_CONTRACT_MOUNT_PATH.to_string())
}

#[tokio::test(flavor = "current_thread")]
async fn turn_events_continue_after_artifact_writes() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("ping")],
        })
        .await
        .expect("turn");
    turn.write_artifact(WriteArtifactRequest {
        path: "tool-results/example.json".to_string(),
        contents: br#"{"ok":true}"#.to_vec(),
    })
    .await
    .expect("write artifact");
    turn.add_events(vec![EventData::Messages {
        messages: vec![assistant_message("pong")],
        response_id: None,
        usage: None,
    }])
    .await
    .expect("append after artifact write");
    turn.finish().await.expect("finish after artifact write");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::ARTIFACT_WRITTEN]),
        }))
        .await
        .expect("artifact event")
        .events;
    let artifact_event = events.first().expect("artifact_written event");
    assert_eq!(artifact_event.session_id, Some(turn.record().session_id));
    assert_eq!(artifact_event.turn_id, Some(turn.record().id));
}

#[tokio::test(flavor = "current_thread")]
async fn turn_artifact_write_allows_interleaved_conversation_writes() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");
    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("ping")],
        })
        .await
        .expect("turn");

    conversation
        .write_artifact(WriteArtifactRequest {
            path: "outside-turn.txt".to_string(),
            contents: b"outside".to_vec(),
        })
        .await
        .expect("write outside-turn artifact");
    turn.write_artifact(WriteArtifactRequest {
        path: "tool-results/example.json".to_string(),
        contents: br#"{"ok":true}"#.to_vec(),
    })
    .await
    .expect("turn artifact write should allow interleaved conversation writes");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await
        .expect("events")
        .events;
    let outside_artifact_event = events
        .iter()
        .find(|event| {
            matches!(
                &event.data,
                EventData::ArtifactWritten { path, .. } if path == "outside-turn.txt"
            )
        })
        .expect("outside artifact event");
    assert_eq!(outside_artifact_event.session_id, None);
    assert_eq!(outside_artifact_event.turn_id, None);

    let turn_artifact_event = events
        .iter()
        .find(|event| {
            matches!(
                &event.data,
                EventData::ArtifactWritten { path, .. } if path == "tool-results/example.json"
            )
        })
        .expect("turn artifact event");
    assert_eq!(
        turn_artifact_event.session_id,
        Some(turn.record().session_id)
    );
    assert_eq!(turn_artifact_event.turn_id, Some(turn.record().id));
}

#[tokio::test(flavor = "current_thread")]
async fn artifacts_are_versioned_by_path() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let first = conversation
        .write_artifact(crate::WriteArtifactRequest {
            path: "notes.txt".to_string(),
            contents: b"hello".to_vec(),
        })
        .await
        .expect("write first artifact");
    let second = conversation
        .write_artifact(crate::WriteArtifactRequest {
            path: "notes.txt".to_string(),
            contents: b"world".to_vec(),
        })
        .await
        .expect("write second artifact");

    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(first.version, 1);
    assert_eq!(second.version, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn artifacts_store_metadata_and_raw_contents_separately() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");

    let version = agent
        .write_artifact(crate::WriteArtifactRequest {
            path: "config/executor.json".to_string(),
            contents: br#"{"model":"gpt-5.4"}"#.to_vec(),
        })
        .await
        .expect("write artifact");

    let artifact_dir = tempdir
        .path()
        .join("agents")
        .join(agent.record().id.to_string())
        .join("artifacts")
        .join(version.artifact_id.to_string());
    let metadata = fs::read_to_string(artifact_dir.join("1.json"))
        .await
        .expect("metadata file should exist");
    let metadata_json: serde_json::Value =
        serde_json::from_str(&metadata).expect("metadata should be valid json");
    assert!(metadata_json.get("contents").is_none());

    let contents = fs::read(artifact_dir.join("1.bin"))
        .await
        .expect("contents file should exist");
    assert_eq!(contents, br#"{"model":"gpt-5.4"}"#);
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_json_artifacts_are_still_readable() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");

    let artifact_id = crate::Uuid7::now();
    let artifact_dir = tempdir
        .path()
        .join("agents")
        .join(agent.record().id.to_string())
        .join("artifacts")
        .join(artifact_id.to_string());
    fs::create_dir_all(&artifact_dir)
        .await
        .expect("artifact dir should exist");
    let legacy_artifact = Artifact {
        version: ArtifactVersion {
            artifact_id,
            path: "config/executor.json".to_string(),
            version: 1,
            created_at: crate::Uuid7::now().timestamp().expect("uuid7 timestamp"),
            size_bytes: 19,
        },
        contents: br#"{"model":"gpt-5.4"}"#.to_vec(),
    };
    fs::write(
        artifact_dir.join("1.json"),
        serde_json::to_vec_pretty(&legacy_artifact).expect("legacy artifact should serialize"),
    )
    .await
    .expect("legacy artifact should write");

    let loaded = agent
        .read_artifact(crate::ReadArtifactRequest {
            artifact_id,
            version: Some(1),
        })
        .await
        .expect("legacy artifact should read")
        .expect("legacy artifact should exist");
    assert_eq!(loaded.contents, br#"{"model":"gpt-5.4"}"#);
}

#[tokio::test(flavor = "current_thread")]
async fn conversation_scope_overrides_agent_scope_and_fork_copies_local_state() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some("base".to_string()),
            name: Some("Base".to_string()),
        })
        .await
        .expect("conversation");

    let agent_secret_id = agent
        .put_secret(PutSecretRequest {
            name: "OPENAI_API_KEY".to_string(),
            secret: Secret::Key {
                value: "agent".to_string(),
            },
        })
        .await
        .expect("agent secret");
    agent
        .put_binding(Binding::Env {
            name: "OPENAI_API_KEY".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            secret_id: agent_secret_id,
        })
        .await
        .expect("agent binding");

    let conversation_secret_id = conversation
        .put_secret(PutSecretRequest {
            name: "OPENAI_API_KEY".to_string(),
            secret: Secret::Key {
                value: "conversation".to_string(),
            },
        })
        .await
        .expect("conversation secret");
    conversation
        .put_binding(Binding::Env {
            name: "OPENAI_API_KEY".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            secret_id: conversation_secret_id,
        })
        .await
        .expect("conversation binding");

    let effective_secret = conversation
        .list_secrets()
        .await
        .expect("list secrets")
        .into_iter()
        .find(|secret| secret.name == "OPENAI_API_KEY")
        .expect("effective secret");
    assert_eq!(effective_secret.id, conversation_secret_id);

    let forked = conversation
        .fork(ForkConversationRequest {
            up_to_inclusive: None,
            slug: Some("fork".to_string()),
            name: Some("Fork".to_string()),
        })
        .await
        .expect("fork");
    let forked_secret = forked
        .list_secrets()
        .await
        .expect("list forked secrets")
        .into_iter()
        .find(|secret| secret.name == "OPENAI_API_KEY")
        .expect("forked effective secret");
    assert_eq!(forked_secret.name, "OPENAI_API_KEY");
    let events = forked
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await;
    let events = events.expect("get forked events").events;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::ConversationForked { .. }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn secrets_are_encrypted_at_rest() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");

    let secret_id = agent
        .put_secret(PutSecretRequest {
            name: "OPENAI_API_KEY".to_string(),
            secret: Secret::Key {
                value: "super-secret-token".to_string(),
            },
        })
        .await
        .expect("secret should be stored");

    let stored_path = tempdir
        .path()
        .join("agents")
        .join(agent.record().id.to_string())
        .join("secrets")
        .join(format!("{secret_id}.json"));
    let stored_bytes = fs::read(stored_path)
        .await
        .expect("stored secret should exist");
    let stored_text = String::from_utf8_lossy(&stored_bytes);

    assert!(!stored_text.contains("super-secret-token"));
    assert!(stored_text.contains("\"ciphertext\""));
    assert!(stored_text.contains("\"algorithm\""));
}

#[tokio::test(flavor = "current_thread")]
async fn secret_storage_preflight_fails_before_secret_persistence() {
    let tempdir = TempDir::new().expect("tempdir");
    let master_key_path = tempdir.path().join("invalid-master.key");
    fs::write(&master_key_path, b"not-a-valid-master-key")
        .await
        .expect("invalid master key fixture");
    let mut config = local_test_config(tempdir.path().join("store"));
    config.secret_backend = SecretBackendChoice::File {
        path: Some(master_key_path),
    };
    let harness = BasicExoHarness::new(config)
        .await
        .expect("harness initialization remains lazy");

    let error = harness
        .preflight_secret_storage()
        .await
        .expect_err("invalid master key should fail preflight");
    assert!(format!("{error:#}").contains("invalid master key length"));
    assert!(harness.list_secrets().await.unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_runs_commands_in_created_sandbox() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "basic-local-process".to_string(),
            default_workdir: Some(tempdir.path().display().to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox should be created");

    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec!["/bin/sh".to_string(), "-lc".to_string(), "cat".to_string()],
            env: Default::default(),
        })
        .await
        .expect("sandbox command should run");
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    let mut stdin = parts.stdin;
    stdin.write_all(b"hello").await.expect("stdin should write");
    drop(stdin);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = tokio::join!(
        stdout.read_to_end(&mut stdout_bytes),
        stderr.read_to_end(&mut stderr_bytes),
        parts.wait,
    );

    stdout_result.expect("stdout should read");
    stderr_result.expect("stderr should read");
    assert_eq!(String::from_utf8_lossy(&stdout_bytes), "hello");
    assert_eq!(String::from_utf8_lossy(&stderr_bytes), "");
    assert_eq!(wait_result.expect("process should exit"), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn agent_scoped_sandbox_is_shared_without_conversation_ownership() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let first_conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some("first".to_string()),
            name: Some("First".to_string()),
        })
        .await
        .expect("first conversation");
    let second_conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some("second".to_string()),
            name: Some("Second".to_string()),
        })
        .await
        .expect("second conversation");

    let create_request = CreateSandboxRequest {
        name: Some("shared-agent-sandbox".to_string()),
        provider: SandboxProvider::LocalProcess,
        image: "basic-local-process".to_string(),
        default_workdir: Some(tempdir.path().display().to_string()),
        file_system_mounts: None,
        durable_file_systems: None,
        enable_networking: Some(true),
        idle_seconds: Some(60),
    };
    let sandbox_id = agent
        .create_sandbox(create_request.clone())
        .await
        .expect("agent sandbox should be created");
    let write_process = agent
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "printf agent-owned > agent-sandbox-proof".to_string(),
            ],
            env: Default::default(),
        })
        .await
        .expect("agent sandbox command should run");
    assert_eq!(
        write_process
            .into_parts()
            .wait
            .await
            .expect("write should exit"),
        0
    );

    let reacquired = agent
        .create_sandbox(create_request)
        .await
        .expect("named agent sandbox should reacquire");
    assert_eq!(reacquired, sandbox_id);
    let read_process = agent
        .run_in_sandbox(RunInSandboxRequest {
            id: reacquired.clone(),
            command: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "cat agent-sandbox-proof".to_string(),
            ],
            env: Default::default(),
        })
        .await
        .expect("agent sandbox should retain filesystem state");
    let parts = read_process.into_parts();
    let mut stdout = parts.stdout;
    let mut stdout_bytes = Vec::new();
    drop(parts.stdin);
    let (stdout_result, wait_result) =
        tokio::join!(stdout.read_to_end(&mut stdout_bytes), parts.wait);
    stdout_result.expect("stdout should read");
    assert_eq!(wait_result.expect("read should exit"), 0);
    assert_eq!(String::from_utf8_lossy(&stdout_bytes), "agent-owned");

    let conversation_process = first_conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: reacquired,
            command: vec!["true".to_string()],
            env: Default::default(),
        })
        .await;
    assert!(conversation_process.is_err());

    for conversation in [first_conversation, second_conversation] {
        let events = conversation
            .get_events(Some(EventQuery {
                cursor: None,
                direction: Some(EventQueryDirection::Asc),
                limit: None,
                session_id: None,
                turn_id: None,
                types: Some(vec![
                    EventKind::SANDBOX_CREATED,
                    EventKind::SANDBOX_STARTED,
                    EventKind::SANDBOX_PROCESS_STARTED,
                    EventKind::SANDBOX_PROCESS_EVENT,
                    EventKind::SANDBOX_PROCESS_STATE_UPDATED,
                ]),
            }))
            .await
            .expect("conversation events should load")
            .events;
        assert!(events.is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn conversation_create_sandbox_is_not_turn_scoped() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");
    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("start turn")],
        })
        .await
        .expect("turn should begin");

    conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "basic-local-process".to_string(),
            default_workdir: Some(tempdir.path().display().to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox should be created");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED, EventKind::SANDBOX_STARTED]),
        }))
        .await
        .expect("sandbox lifecycle events")
        .events;

    assert_eq!(events.len(), 2);
    for event in events {
        assert_ne!(event.session_id, Some(turn.record().session_id));
        assert_ne!(event.turn_id, Some(turn.record().id));
        assert_eq!(event.session_id, None);
        assert_eq!(event.turn_id, None);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_reuses_named_sandbox() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let request = CreateSandboxRequest {
        name: Some("worker".to_string()),
        provider: SandboxProvider::LocalProcess,
        image: "basic-local-process".to_string(),
        default_workdir: Some(tempdir.path().display().to_string()),
        file_system_mounts: None,
        durable_file_systems: None,
        enable_networking: Some(true),
        idle_seconds: Some(60),
    };

    let first = conversation
        .create_sandbox(request.clone())
        .await
        .expect("first sandbox should be created");
    let second = conversation
        .create_sandbox(request.clone())
        .await
        .expect("matching sandbox should be reused");
    assert_eq!(first, second);

    let mut other_request = request;
    other_request.name = Some("other".to_string());
    let third = conversation
        .create_sandbox(other_request)
        .await
        .expect("different name should create a distinct sandbox");
    assert_ne!(first, third);
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_reattaches_running_sandbox_in_new_harness_process() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");
    let agent_id = agent.record().id;
    let conversation_id = conversation.record().id;

    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "basic-local-process".to_string(),
            default_workdir: Some(tempdir.path().display().to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox should be created");
    drop(conversation);
    drop(agent);
    drop(harness);

    let reloaded_harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should reload");
    let reloaded_agent = reloaded_harness
        .get_agent(&agent_id)
        .await
        .expect("get agent")
        .expect("agent exists");
    let reloaded_conversation = reloaded_agent
        .get_conversation(&conversation_id)
        .await
        .expect("get conversation")
        .expect("conversation exists");

    let process = reloaded_conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "printf reattached".to_string(),
            ],
            env: Default::default(),
        })
        .await
        .expect("sandbox command should run after reload");
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    drop(parts.stdin);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = tokio::join!(
        stdout.read_to_end(&mut stdout_bytes),
        stderr.read_to_end(&mut stderr_bytes),
        parts.wait,
    );

    stdout_result.expect("stdout should read");
    stderr_result.expect("stderr should read");
    assert_eq!(String::from_utf8_lossy(&stdout_bytes), "reattached");
    assert_eq!(String::from_utf8_lossy(&stderr_bytes), "");
    assert_eq!(wait_result.expect("process should exit"), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_exposes_process_events_and_input() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");
    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "basic-local-process".to_string(),
            default_workdir: Some(tempdir.path().display().to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox should be created");
    let process = conversation
        .start_sandbox_process(StartSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            name: None,
            command: vec!["/bin/sh".to_string(), "-lc".to_string(), "cat".to_string()],
            env: Default::default(),
            cwd: None,
            mode: Default::default(),
            stdin: SandboxProcessStdin::Open,
            output: Default::default(),
            lifecycle: Default::default(),
        })
        .await
        .expect("process should start");

    conversation
        .write_sandbox_process_input(WriteSandboxProcessInputRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
            data: b"hello process api".to_vec(),
        })
        .await
        .expect("stdin should write");
    conversation
        .close_sandbox_process_input(CloseSandboxProcessInputRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
        })
        .await
        .expect("stdin should close");

    let status = conversation
        .wait_sandbox_process(WaitSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
        })
        .await
        .expect("process should wait");
    assert_eq!(status, SandboxProcessStatus::Exited { exit_code: 0 });

    let events = conversation
        .get_sandbox_process_events(SandboxProcessEventQuery {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
            after: None,
            limit: None,
            follow: None,
        })
        .await
        .expect("process events should read");
    assert_eq!(events.status, SandboxProcessStatus::Exited { exit_code: 0 });
    assert!(events.events.iter().any(|event| matches!(
        event,
        SandboxProcessEvent::Stdout { data, .. }
            if String::from_utf8_lossy(data).contains("hello process api")
    )));
    assert!(matches!(
        events.events.last(),
        Some(SandboxProcessEvent::Exit { exit_code: 0, .. })
    ));

    let conversation_events = conversation
        .get_events(None)
        .await
        .expect("conversation events should read")
        .events;
    assert!(conversation_events.iter().any(|event| matches!(
        &event.data,
        EventData::SandboxProcessStarted {
            sandbox_id: event_sandbox_id,
            process_id,
            ..
        } if event_sandbox_id == &sandbox_id && process_id == &process.id
    )));
    assert!(conversation_events.iter().any(|event| matches!(
        &event.data,
        EventData::SandboxProcessEvent {
            sandbox_id: event_sandbox_id,
            process_id,
            event: SandboxProcessEvent::Stdout { data, .. },
        } if event_sandbox_id == &sandbox_id
            && process_id == &process.id
            && String::from_utf8_lossy(data).contains("hello process api")
    )));
    assert!(conversation_events.iter().any(|event| matches!(
        &event.data,
        EventData::SandboxProcessStateUpdated {
            sandbox_id: event_sandbox_id,
            process_id,
            status: SandboxProcessStatus::Exited { exit_code: 0 },
            ..
        } if event_sandbox_id == &sandbox_id && process_id == &process.id
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_records_process_name_metadata() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");
    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: Some("service-test".to_string()),
            provider: SandboxProvider::LocalProcess,
            image: "basic-local-process".to_string(),
            default_workdir: Some(tempdir.path().display().to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox should be created");

    let process = conversation
        .start_sandbox_process(StartSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            name: Some("echo-service".to_string()),
            command: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "printf service-ready".to_string(),
            ],
            env: Default::default(),
            cwd: None,
            mode: Default::default(),
            stdin: SandboxProcessStdin::None,
            output: Default::default(),
            lifecycle: Default::default(),
        })
        .await
        .expect("named process should start");
    assert_eq!(process.name.as_deref(), Some("echo-service"));

    let status = conversation
        .wait_sandbox_process(WaitSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
        })
        .await
        .expect("named process should wait");
    assert_eq!(status, SandboxProcessStatus::Exited { exit_code: 0 });

    let events = conversation
        .get_events(None)
        .await
        .expect("conversation events should read")
        .events;
    assert!(events.iter().any(|event| matches!(
        &event.data,
        EventData::SandboxProcessStarted {
            sandbox_id: event_sandbox_id,
            process_id,
            name: Some(name),
            ..
        } if event_sandbox_id == &sandbox_id
            && process_id == &process.id
            && name == "echo-service"
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn sandbox_process_terminal_event_waits_for_output_drain() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new_with_sandbox_backend(
        local_test_config(tempdir.path()),
        Arc::new(TestSandboxBackend::new(TestProcessSpec {
            stdout: Box::pin(DelayedRead::new(
                Duration::from_millis(50),
                b"late stdout".to_vec(),
            )),
            stderr: Box::pin(Cursor::new(Vec::new())),
            stdin: Box::pin(Cursor::new(Vec::new())),
            wait: Box::pin(async { Ok(0) }),
        })),
    )
    .await
    .expect("harness should initialize");
    let conversation = test_conversation(&harness).await;
    let sandbox_id = test_sandbox(&conversation).await;
    let process = conversation
        .start_sandbox_process(StartSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            name: None,
            command: vec!["test".to_string()],
            env: Default::default(),
            cwd: None,
            mode: Default::default(),
            stdin: SandboxProcessStdin::None,
            output: Default::default(),
            lifecycle: Default::default(),
        })
        .await
        .expect("process should start");

    let status = timeout(
        Duration::from_secs(1),
        conversation.wait_sandbox_process(WaitSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
        }),
    )
    .await
    .expect("wait should not hang")
    .expect("wait should succeed");
    assert_eq!(status, SandboxProcessStatus::Exited { exit_code: 0 });

    let events = conversation
        .get_sandbox_process_events(SandboxProcessEventQuery {
            sandbox_id,
            process_id: process.id,
            after: None,
            limit: None,
            follow: None,
        })
        .await
        .expect("events should read");
    assert_eq!(
        events
            .events
            .iter()
            .filter_map(|event| match event {
                SandboxProcessEvent::Stdout { data, .. } => {
                    Some(String::from_utf8_lossy(data).to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["late stdout".to_string()]
    );
    assert!(matches!(
        events.events.last(),
        Some(SandboxProcessEvent::Exit { exit_code: 0, .. })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn wait_sandbox_process_returns_after_concurrent_completion() {
    let tempdir = TempDir::new().expect("tempdir");
    let (finish_tx, finish_rx) = oneshot::channel();
    let harness = BasicExoHarness::new_with_sandbox_backend(
        local_test_config(tempdir.path()),
        Arc::new(TestSandboxBackend::new(TestProcessSpec {
            stdout: Box::pin(Cursor::new(Vec::new())),
            stderr: Box::pin(Cursor::new(Vec::new())),
            stdin: Box::pin(Cursor::new(Vec::new())),
            wait: Box::pin(async move {
                finish_rx.await.expect("finish signal should send");
                Ok(0)
            }),
        })),
    )
    .await
    .expect("harness should initialize");
    let conversation = test_conversation(&harness).await;
    let sandbox_id = test_sandbox(&conversation).await;
    let process = conversation
        .start_sandbox_process(StartSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            name: None,
            command: vec!["test".to_string()],
            env: Default::default(),
            cwd: None,
            mode: Default::default(),
            stdin: SandboxProcessStdin::None,
            output: Default::default(),
            lifecycle: Default::default(),
        })
        .await
        .expect("process should start");

    let wait_conversation = Arc::clone(&conversation);
    let wait_task = tokio::spawn(async move {
        wait_conversation
            .wait_sandbox_process(WaitSandboxProcessRequest {
                sandbox_id,
                process_id: process.id,
            })
            .await
    });
    tokio::task::yield_now().await;
    finish_tx
        .send(())
        .expect("finish signal should be received");
    let status = timeout(Duration::from_secs(1), wait_task)
        .await
        .expect("wait should not hang")
        .expect("wait task should not panic")
        .expect("wait should succeed");
    assert_eq!(status, SandboxProcessStatus::Exited { exit_code: 0 });
}

async fn test_conversation(harness: &BasicExoHarness) -> Arc<dyn crate::ConversationHandle> {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation")
}

async fn test_sandbox(conversation: &Arc<dyn crate::ConversationHandle>) -> String {
    conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "test-sandbox".to_string(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox should be created")
}

#[test]
fn create_sandbox_request_requires_provider() {
    let error = serde_json::from_value::<CreateSandboxRequest>(serde_json::json!({
        "image": "test-sandbox",
        "default_workdir": "/",
        "file_system_mounts": null,
        "enable_networking": true,
        "idle_seconds": 60,
    }))
    .expect_err("provider should be required");

    assert!(error.to_string().contains("missing field `provider`"));
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_rejects_daytona_provider() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let error = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::Daytona,
            image: "test-sandbox".to_string(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect_err("daytona should not be handled by BasicExoHarness");

    assert!(
        error
            .to_string()
            .contains("is not supported by this harness")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn advertised_daytona_without_secret_errors_at_first_use() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config_with_daytona(tempdir.path()))
        .await
        .expect("harness should initialize without any daytona secret set");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let error = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::Daytona,
            image: "test-sandbox".to_string(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            durable_file_systems: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect_err("daytona requires DAYTONA_API_KEY to be set");

    assert!(error.to_string().contains("DAYTONA_API_KEY"));
}

#[tokio::test(flavor = "current_thread")]
async fn sandbox_provider_state_persists_through_events_after_harness_reload() {
    let tempdir = TempDir::new().expect("tempdir");
    let state = serde_json::json!({
        "microvm_id": "microvm-test",
        "endpoint": "https://example.com"
    });
    let first_backend = Arc::new(TestProviderStateBackend::new(state.clone()));
    let harness =
        BasicExoHarness::new_with_sandbox_backend(local_test_config(tempdir.path()), first_backend)
            .await
            .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let agent_id = agent.record().id;
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");
    let conversation_id = conversation.record().id;
    let sandbox_id = conversation
        .create_sandbox(provider_state_test_create_request())
        .await
        .expect("sandbox should be created");
    let events = conversation
        .get_events(Some(EventQuery {
            types: Some(vec![EventKind::custom("sandbox_provider_state")]),
            ..Default::default()
        }))
        .await
        .expect("events should load")
        .events;
    assert_eq!(events.len(), 1);

    let second_backend = Arc::new(TestProviderStateBackend::new(state.clone()));
    let reloaded = BasicExoHarness::new_with_sandbox_backend(
        local_test_config(tempdir.path()),
        second_backend.clone(),
    )
    .await
    .expect("reloaded harness should initialize");
    let reloaded_agent = reloaded
        .get_agent(&agent_id)
        .await
        .expect("agent lookup should succeed")
        .expect("agent should exist");
    let reloaded_conversation = reloaded_agent
        .get_conversation(&conversation_id)
        .await
        .expect("conversation lookup should succeed")
        .expect("conversation should exist");
    let reused_sandbox_id = reloaded_conversation
        .create_sandbox(provider_state_test_create_request())
        .await
        .expect("sandbox should be reused");
    assert_eq!(reused_sandbox_id, sandbox_id);
    assert_eq!(
        second_backend.requests.lock().await.as_slice(),
        &[Some(state)]
    );
}

fn provider_state_test_create_request() -> CreateSandboxRequest {
    CreateSandboxRequest {
        name: Some("stateful".to_string()),
        provider: SandboxProvider::LocalProcess,
        image: "test-sandbox".to_string(),
        default_workdir: Some("/".to_string()),
        file_system_mounts: None,
        durable_file_systems: None,
        enable_networking: Some(true),
        idle_seconds: Some(60),
    }
}

struct TestProviderStateBackend {
    state: Value,
    requests: Arc<AsyncMutex<Vec<Option<Value>>>>,
}

impl TestProviderStateBackend {
    fn new(state: Value) -> Self {
        Self {
            state,
            requests: Arc::new(AsyncMutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ManagedSandboxBackend for TestProviderStateBackend {
    async fn acquire(
        &self,
        request: SandboxRequest,
    ) -> crate::Result<Arc<dyn ManagedSandboxHandle>> {
        self.requests.lock().await.push(request.provider_state);
        Ok(Arc::new(TestProviderStateHandle {
            state: self.state.clone(),
        }))
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> crate::Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("test provider-state backend does not support snapshot restore")
    }
}

struct TestProviderStateHandle {
    state: Value,
}

#[async_trait]
impl ManagedSandboxHandle for TestProviderStateHandle {
    fn id(&self) -> &str {
        "test-provider-state"
    }

    fn provider_state(&self) -> Option<Value> {
        Some(self.state.clone())
    }

    async fn exec(&self, _command: &SandboxCommand) -> crate::Result<SandboxCommandOutput> {
        bail!("test provider-state handle does not support exec")
    }

    async fn start_process(&self, _command: &SandboxCommand) -> crate::Result<SandboxProcessParts> {
        bail!("test provider-state handle does not support start_process")
    }

    async fn stop(&self) -> crate::Result<()> {
        Ok(())
    }

    async fn snapshot(&self) -> crate::Result<SnapshotPayload> {
        bail!("test provider-state handle does not support snapshots")
    }
}

struct TestSandboxBackend {
    process: Arc<AsyncMutex<Option<TestProcessSpec>>>,
}

impl TestSandboxBackend {
    fn new(process: TestProcessSpec) -> Self {
        Self {
            process: Arc::new(AsyncMutex::new(Some(process))),
        }
    }
}

#[async_trait]
impl ManagedSandboxBackend for TestSandboxBackend {
    async fn acquire(
        &self,
        _request: SandboxRequest,
    ) -> crate::Result<Arc<dyn ManagedSandboxHandle>> {
        Ok(Arc::new(TestSandboxHandle {
            process: Arc::clone(&self.process),
        }))
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> crate::Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("test sandbox backend does not support snapshot restore")
    }
}

struct TestSandboxHandle {
    process: Arc<AsyncMutex<Option<TestProcessSpec>>>,
}

#[async_trait]
impl ManagedSandboxHandle for TestSandboxHandle {
    fn id(&self) -> &str {
        "test-sandbox"
    }

    async fn exec(&self, _command: &SandboxCommand) -> crate::Result<SandboxCommandOutput> {
        bail!("test sandbox handle only supports start_process")
    }

    async fn start_process(&self, _command: &SandboxCommand) -> crate::Result<SandboxProcessParts> {
        let process = self
            .process
            .lock()
            .await
            .take()
            .expect("test process should only start once");
        Ok(SandboxProcessParts {
            stdout: process.stdout,
            stderr: process.stderr,
            stdin: process.stdin,
            wait: process.wait,
        })
    }

    async fn stop(&self) -> crate::Result<()> {
        Ok(())
    }

    async fn snapshot(&self) -> crate::Result<SnapshotPayload> {
        bail!("test sandbox handle does not support snapshots")
    }
}

struct TestProcessSpec {
    stdout: BoxAsyncRead,
    stderr: BoxAsyncRead,
    stdin: BoxAsyncWrite,
    wait: BoxFuture<'static, crate::Result<i32>>,
}

struct DelayedRead {
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    data: Vec<u8>,
    offset: usize,
}

impl DelayedRead {
    fn new(delay: Duration, data: Vec<u8>) -> Self {
        Self {
            sleep: Some(Box::pin(sleep(delay))),
            data,
            offset: 0,
        }
    }
}

impl AsyncRead for DelayedRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Some(sleep) = self.sleep.as_mut() {
            if sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.sleep = None;
        }
        if self.offset >= self.data.len() {
            return Poll::Ready(Ok(0));
        }
        let length = buffer.len().min(self.data.len() - self.offset);
        buffer[..length].copy_from_slice(&self.data[self.offset..self.offset + length]);
        self.offset += length;
        Poll::Ready(Ok(length))
    }
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        id: None,
        content: AssistantContent::String(text.to_string()),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn restored_sandbox_image_persists_for_cross_process_reattach() {
    let tempdir = TempDir::new().expect("tempdir");
    let first_backend = Arc::new(RestoreImageTestBackend::default());
    let harness =
        BasicExoHarness::new_with_sandbox_backend(local_test_config(tempdir.path()), first_backend)
            .await
            .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let agent_id = agent.record().id;

    let create_request = CreateSandboxRequest {
        name: Some("agent-sandbox".to_string()),
        provider: SandboxProvider::LocalProcess,
        image: "original-image".to_string(),
        default_workdir: Some("/".to_string()),
        file_system_mounts: None,
        durable_file_systems: None,
        enable_networking: Some(true),
        idle_seconds: Some(60),
    };
    let sandbox_id = agent
        .create_sandbox(create_request.clone())
        .await
        .expect("sandbox should be created");
    let snapshot_id = agent
        .snapshot_sandbox(sandbox_id.clone())
        .await
        .expect("snapshot should succeed");
    agent
        .start_sandbox(StartSandboxRequest {
            id: sandbox_id.clone(),
            snapshot_id,
            idle_seconds: None,
            provider: None,
        })
        .await
        .expect("restore from snapshot should succeed");

    // Simulate another process (e.g. the scheduler runner) resolving the same
    // named sandbox from its own harness instance.
    let second_backend = Arc::new(RestoreImageTestBackend::default());
    let reloaded = BasicExoHarness::new_with_sandbox_backend(
        local_test_config(tempdir.path()),
        second_backend.clone(),
    )
    .await
    .expect("reloaded harness should initialize");
    let reloaded_agent = reloaded
        .get_agent(&agent_id)
        .await
        .expect("agent lookup should succeed")
        .expect("agent should exist");
    let reused_sandbox_id = reloaded_agent
        .create_sandbox(create_request)
        .await
        .expect("named sandbox should still match the original request");
    assert_eq!(reused_sandbox_id, sandbox_id);
    // The reattach must target the restored image, not the originally
    // requested one; otherwise a real backend would boot a second container
    // with pre-restore state.
    assert_eq!(
        second_backend.acquired_images.lock().await.as_slice(),
        &["restored-image".to_string()]
    );
}

#[derive(Default)]
struct RestoreImageTestBackend {
    acquired_images: Arc<AsyncMutex<Vec<String>>>,
}

#[async_trait]
impl ManagedSandboxBackend for RestoreImageTestBackend {
    async fn acquire(
        &self,
        request: SandboxRequest,
    ) -> crate::Result<Arc<dyn ManagedSandboxHandle>> {
        self.acquired_images
            .lock()
            .await
            .push(request.spec.image.clone());
        Ok(Arc::new(RestoreImageTestHandle {
            image: request.spec.image,
        }))
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> crate::Result<Arc<dyn ManagedSandboxHandle>> {
        // Like the docker backend, a restore boots from a freshly loaded tag
        // rather than the requested image.
        Ok(Arc::new(RestoreImageTestHandle {
            image: "restored-image".to_string(),
        }))
    }
}

struct RestoreImageTestHandle {
    image: String,
}

#[async_trait]
impl ManagedSandboxHandle for RestoreImageTestHandle {
    fn id(&self) -> &str {
        "restore-image-test"
    }

    fn effective_image(&self) -> Option<String> {
        Some(self.image.clone())
    }

    async fn exec(&self, _command: &SandboxCommand) -> crate::Result<SandboxCommandOutput> {
        bail!("restore-image test handle does not support exec")
    }

    async fn start_process(&self, _command: &SandboxCommand) -> crate::Result<SandboxProcessParts> {
        bail!("restore-image test handle does not support start_process")
    }

    async fn stop(&self) -> crate::Result<()> {
        Ok(())
    }

    async fn snapshot(&self) -> crate::Result<SnapshotPayload> {
        Ok(SnapshotPayload {
            kind: SnapshotKind::DockerImageTar,
            bytes: bytes::Bytes::from_static(b"restore-image-test"),
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn daytona_sandbox_binding_drives_provider_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config_with_daytona(tempdir.path()))
        .await
        .expect("harness");

    // No binding yet: resolver returns None so the backend falls back to the spec.
    assert!(
        harness
            .daytona_config_from_binding_for_test()
            .await
            .expect("resolve")
            .is_none()
    );

    let secret_id = harness
        .put_secret(PutSecretRequest {
            name: "DAYTONA_API_KEY".to_string(),
            secret: Secret::Key {
                value: "key-123".to_string(),
            },
        })
        .await
        .expect("secret");
    harness
        .put_binding(Binding::Sandbox {
            name: "daytona".to_string(),
            config: SandboxProviderConfig::Daytona {
                api_key_secret_id: secret_id,
                region: Some("experimental".to_string()),
                organization_id: Some("org-1".to_string()),
                api_url: None,
                default_image: crate::default_daytona_image(),
            },
        })
        .await
        .expect("binding");

    let config = harness
        .daytona_config_from_binding_for_test()
        .await
        .expect("resolve")
        .expect("binding present");
    assert_eq!(config.api_key, "key-123");
    assert_eq!(config.target.as_deref(), Some("experimental"));
    assert_eq!(config.organization_id.as_deref(), Some("org-1"));
    assert_eq!(config.api_url, crate::DEFAULT_DAYTONA_API_URL);
}

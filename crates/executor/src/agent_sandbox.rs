use exoharness::{
    AgentHandle, Artifact, ArtifactVersion, CreateSandboxRequest, ReadArtifactRequest, Result,
    SandboxProvider, Uuid7, WriteArtifactRequest,
};
use serde::{Deserialize, Serialize};

use crate::AgentConfig;
use crate::conversation_sandbox::{ConversationSandboxSpec, agent_sandbox_spec};

// v1 was a conversation-owned handle with `conversationId`/`sandboxId`. The
// agent-owned record has a different shape, so keep it on a distinct path.
const AGENT_SANDBOX_ARTIFACT_PATH: &str = "config/agent-sandbox-v2.json";
const AGENT_SANDBOX_NAME_PREFIX: &str = "agent-sandbox";

#[derive(Clone)]
pub(crate) struct AgentSandboxHandle {
    pub(crate) sandbox_id: String,
}

// The durable identity of the agent's sandbox: its name plus the spec it was
// created with, so it can be recreated faithfully if the underlying sandbox
// is ever reaped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSandboxRecord {
    sandbox_name: String,
    provider: SandboxProvider,
    image: String,
    default_workdir: String,
    file_system_mounts: Vec<exoharness::FileSystemMount>,
    #[serde(default)]
    durable_file_systems: Vec<exoharness::DurableFileSystem>,
    enable_networking: bool,
    idle_seconds: u64,
}

impl AgentSandboxRecord {
    fn spec(&self) -> ConversationSandboxSpec {
        ConversationSandboxSpec {
            provider: self.provider,
            image: self.image.clone(),
            default_workdir: self.default_workdir.clone(),
            file_system_mounts: self.file_system_mounts.clone(),
            durable_file_systems: self.durable_file_systems.clone(),
            enable_networking: self.enable_networking,
            idle_seconds: self.idle_seconds,
        }
    }

    fn new(sandbox_name: String, spec: ConversationSandboxSpec) -> Self {
        Self {
            sandbox_name,
            provider: spec.provider,
            image: spec.image,
            default_workdir: spec.default_workdir,
            file_system_mounts: spec.file_system_mounts,
            durable_file_systems: spec.durable_file_systems,
            enable_networking: spec.enable_networking,
            idle_seconds: spec.idle_seconds,
        }
    }
}

/// Get the agent's sandbox, creating it on first use.
///
/// The recorded sandbox is THE agent sandbox: once created, every caller
/// attaches to it by its durable name regardless of how the agent config has
/// drifted since. Config changes never implicitly evict the sandbox (and the
/// state in its filesystem); applying a new spec requires explicitly deleting
/// or recreating it.
pub(crate) async fn ensure_agent_sandbox(
    agent: &dyn AgentHandle,
    agent_config: &AgentConfig,
) -> Result<AgentSandboxHandle> {
    if let Some(record) = load_agent_sandbox_record(agent).await? {
        let recorded_spec = record.spec();
        if recorded_spec != agent_sandbox_spec(agent_config) {
            tracing::info!(
                sandbox_name = %record.sandbox_name,
                "agent sandbox differs from current agent config; \
                 recreate the agent sandbox to apply the new config"
            );
        }
        return attach_agent_sandbox(agent, record.sandbox_name.clone(), &recorded_spec).await;
    }

    let spec = agent_sandbox_spec(agent_config);
    let sandbox_name = new_agent_sandbox_name();
    let handle = attach_agent_sandbox(agent, sandbox_name.clone(), &spec).await?;
    store_agent_sandbox_record(agent, &AgentSandboxRecord::new(sandbox_name, spec)).await?;
    Ok(handle)
}

/// The agent's sandbox if one has been created, without creating one.
pub(crate) async fn current_agent_sandbox(
    agent: &dyn AgentHandle,
) -> Result<Option<AgentSandboxHandle>> {
    let Some(record) = load_agent_sandbox_record(agent).await? else {
        return Ok(None);
    };
    let spec = record.spec();
    Ok(Some(
        attach_agent_sandbox(agent, record.sandbox_name.clone(), &spec).await?,
    ))
}

async fn attach_agent_sandbox(
    agent: &dyn AgentHandle,
    sandbox_name: String,
    spec: &ConversationSandboxSpec,
) -> Result<AgentSandboxHandle> {
    let sandbox_id = agent
        .create_sandbox(CreateSandboxRequest {
            name: Some(sandbox_name),
            provider: spec.provider,
            image: spec.image.clone(),
            default_workdir: Some(spec.default_workdir.clone()),
            file_system_mounts: Some(spec.file_system_mounts.clone()),
            durable_file_systems: Some(spec.durable_file_systems.clone()),
            enable_networking: Some(spec.enable_networking),
            idle_seconds: Some(spec.idle_seconds),
        })
        .await?;
    Ok(AgentSandboxHandle { sandbox_id })
}

async fn load_agent_sandbox_record(agent: &dyn AgentHandle) -> Result<Option<AgentSandboxRecord>> {
    let Some(artifact) = latest_agent_artifact(agent, AGENT_SANDBOX_ARTIFACT_PATH).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&artifact.contents)?))
}

async fn store_agent_sandbox_record(
    agent: &dyn AgentHandle,
    record: &AgentSandboxRecord,
) -> Result<()> {
    agent
        .write_artifact(WriteArtifactRequest {
            path: AGENT_SANDBOX_ARTIFACT_PATH.to_string(),
            contents: serde_json::to_vec_pretty(record)?,
        })
        .await?;
    Ok(())
}

async fn latest_agent_artifact(agent: &dyn AgentHandle, path: &str) -> Result<Option<Artifact>> {
    let Some(version) = latest_artifact_version(agent.list_artifacts().await?, path) else {
        return Ok(None);
    };
    agent
        .read_artifact(ReadArtifactRequest {
            artifact_id: version.artifact_id,
            version: Some(version.version),
        })
        .await
}

fn latest_artifact_version(artifacts: Vec<ArtifactVersion>, path: &str) -> Option<ArtifactVersion> {
    artifacts
        .into_iter()
        .filter(|artifact| artifact.path == path)
        .max_by_key(|artifact| artifact.version)
}

fn new_agent_sandbox_name() -> String {
    format!("{AGENT_SANDBOX_NAME_PREFIX}-{}", Uuid7::now())
}

#[cfg(test)]
mod tests {
    use exoharness::{
        BasicExoHarness, ExoHarness, FileSystemMount, FileSystemMountMode, NewAgentRequest,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::test_support::local_test_config;
    use crate::{AgentHarnessKind, AgentSandboxConfig, SandboxScope};

    async fn test_agent(tempdir: &TempDir) -> std::sync::Arc<dyn AgentHandle> {
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap()
    }

    fn test_agent_config(sandbox: AgentSandboxConfig) -> AgentConfig {
        AgentConfig {
            instructions: vec![],
            harness: AgentHarnessKind::Exo,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox,
            model: "test-model".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        }
    }

    fn test_sandbox_config(image: Option<&str>) -> AgentSandboxConfig {
        AgentSandboxConfig {
            image: image.map(str::to_string),
            provider: SandboxProvider::LocalProcess,
            mounts: vec![],
            enable_networking: false,
            scope: SandboxScope::Agent,
        }
    }

    #[tokio::test]
    async fn reattaches_to_the_same_agent_sandbox() {
        let tempdir = TempDir::new().unwrap();
        let agent = test_agent(&tempdir).await;
        let agent_config = test_agent_config(test_sandbox_config(None));

        let first = ensure_agent_sandbox(agent.as_ref(), &agent_config)
            .await
            .unwrap();
        let second = ensure_agent_sandbox(agent.as_ref(), &agent_config)
            .await
            .unwrap();
        assert_eq!(first.sandbox_id, second.sandbox_id);

        let current = current_agent_sandbox(agent.as_ref())
            .await
            .unwrap()
            .expect("agent sandbox should be recorded");
        assert_eq!(current.sandbox_id, first.sandbox_id);
    }

    #[tokio::test]
    async fn config_drift_does_not_evict_the_agent_sandbox() {
        let tempdir = TempDir::new().unwrap();
        let agent = test_agent(&tempdir).await;

        let original = ensure_agent_sandbox(
            agent.as_ref(),
            &test_agent_config(test_sandbox_config(Some("image-a"))),
        )
        .await
        .unwrap();
        // A changed config must attach to the existing sandbox, not replace it.
        let after_drift = ensure_agent_sandbox(
            agent.as_ref(),
            &test_agent_config(test_sandbox_config(Some("image-b"))),
        )
        .await
        .unwrap();
        assert_eq!(after_drift.sandbox_id, original.sandbox_id);
    }

    #[tokio::test]
    async fn current_agent_sandbox_is_none_until_created() {
        let tempdir = TempDir::new().unwrap();
        let agent = test_agent(&tempdir).await;

        assert!(
            current_agent_sandbox(agent.as_ref())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn agent_mounts_shape_the_agent_sandbox_spec() {
        let tempdir = TempDir::new().unwrap();
        let mount = FileSystemMount {
            host_path: tempdir.path().display().to_string(),
            mount_path: "/workspace/exo".to_string(),
            mode: FileSystemMountMode::ReadWrite,
            internal: Some(false),
        };
        let mut sandbox = test_sandbox_config(None);
        sandbox.mounts = vec![mount.clone()];
        let agent_config = test_agent_config(sandbox);

        let spec = agent_sandbox_spec(&agent_config);
        assert_eq!(spec.default_workdir, "/workspace/exo");
        assert_eq!(spec.file_system_mounts, vec![mount]);
    }
}

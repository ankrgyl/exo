use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use excode::{
    CreateSandboxFromRecipeRequest, RecipeService, SandboxRecipe, SandboxRecipeStep, SecretResolver,
};
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, CreateSandboxRequest, ExoHarness, NewAgentRequest,
    NewConversationRequest, RunInSandboxRequest, SandboxProvider, SecretId,
};
use futures::io::AsyncReadExt;
use tempfile::TempDir;

struct NoSecrets;

#[async_trait]
impl SecretResolver for NoSecrets {
    async fn resolve_key(&self, _secret_id: &SecretId) -> Result<String> {
        bail!("no secrets are configured for this test")
    }
}

fn local_config(root: &TempDir) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.path().to_path_buf(),
        secret_backend: exoharness::SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_backends: vec![exoharness::SandboxBackendRegistration::local_process()],
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "starts a real local-process sandbox"]
async fn recipe_service_creates_and_initializes_a_sandbox() {
    let root = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_config(&root))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "recipe-e2e".to_string(),
            name: "Recipe E2E".to_string(),
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation should be created");
    let recipe_service = RecipeService::new(conversation.clone(), Arc::new(NoSecrets));
    let repository_path = root.path().join("repository").display().to_string();
    let sandbox_id = recipe_service
        .create_sandbox(CreateSandboxFromRecipeRequest {
            sandbox: CreateSandboxRequest {
                name: None,
                provider: SandboxProvider::LocalProcess,
                image: "basic-local-process".to_string(),
                resources: Default::default(),
                default_workdir: Some("/".to_string()),
                file_system_mounts: None,
                durable_file_systems: None,
                enable_networking: Some(true),
                idle_seconds: Some(60),
            },
            recipe: SandboxRecipe {
                snapshot_id: None,
                steps: vec![
                    SandboxRecipeStep::GithubRepository {
                        repository: "https://github.com/exoharness/exo.git".to_string(),
                        branch: Some("main".to_string()),
                        sha: None,
                        destination: repository_path.clone(),
                        secret_id: None,
                    },
                    SandboxRecipeStep::Command {
                        argv: vec![
                            "/bin/test".to_string(),
                            "-s".to_string(),
                            format!("{repository_path}/README.md"),
                        ],
                        cwd: None,
                    },
                ],
            },
        })
        .await
        .expect("recipe should create and initialize the sandbox");
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec!["/bin/cat".into(), format!("{repository_path}/README.md")],
            env: HashMap::new(),
        })
        .await
        .expect("marker should be readable");
    let mut parts = process.into_parts();
    let mut stdout = Vec::new();
    parts.stdout.read_to_end(&mut stdout).await.unwrap();
    assert_eq!(parts.wait.await.unwrap(), 0);
    assert!(
        !stdout.is_empty(),
        "the cloned repository README should not be empty"
    );
}

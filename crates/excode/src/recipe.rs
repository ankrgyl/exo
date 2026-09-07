use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use exoharness::{
    CreateSandboxRequest, RunInSandboxRequest, SandboxHandle, SandboxId, SecretId, SnapshotId,
};
use futures::io::AsyncReadExt;

#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve_key(&self, secret_id: &SecretId) -> Result<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSandboxFromRecipeRequest {
    pub sandbox: CreateSandboxRequest,
    pub recipe: SandboxRecipe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRecipe {
    pub snapshot_id: Option<SnapshotId>,
    pub steps: Vec<SandboxRecipeStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxRecipeStep {
    GithubRepository {
        repository: String,
        branch: Option<String>,
        sha: Option<String>,
        destination: String,
        secret_id: Option<SecretId>,
    },
    Command {
        argv: Vec<String>,
        cwd: Option<String>,
    },
}

pub struct RecipeService {
    sandbox: Arc<dyn SandboxHandle>,
    secrets: Arc<dyn SecretResolver>,
}

impl RecipeService {
    pub fn new(sandbox: Arc<dyn SandboxHandle>, secrets: Arc<dyn SecretResolver>) -> Self {
        Self { sandbox, secrets }
    }

    pub async fn create_sandbox(
        &self,
        request: CreateSandboxFromRecipeRequest,
    ) -> Result<SandboxId> {
        let CreateSandboxFromRecipeRequest { sandbox, recipe } = request;
        let sandbox_id = match recipe.snapshot_id {
            Some(snapshot_id) => {
                self.sandbox
                    .restore_sandbox(exoharness::RestoreSandboxRequest {
                        snapshot_id,
                        sandbox,
                    })
                    .await?
            }
            None => self.sandbox.create_sandbox(sandbox).await?,
        };

        let result = async {
            for step in recipe.steps {
                self.run_step(&sandbox_id, step).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            if let Err(cleanup_error) = self.sandbox.terminate_sandbox(sandbox_id.clone()).await {
                tracing::warn!(%cleanup_error, %sandbox_id, "failed cleaning up sandbox after recipe failure");
            }
            return Err(error);
        }
        Ok(sandbox_id)
    }

    async fn run_step(&self, sandbox_id: &SandboxId, step: SandboxRecipeStep) -> Result<()> {
        match step {
            SandboxRecipeStep::GithubRepository {
                repository,
                branch,
                sha,
                destination,
                secret_id,
            } => {
                validate_github_repository(&repository)?;
                let mut env = HashMap::new();
                if let Some(secret_id) = secret_id {
                    let token = self.secrets.resolve_key(&secret_id).await?;
                    let encoded = STANDARD.encode(format!("x-access-token:{token}"));
                    env.insert("GIT_CONFIG_COUNT".into(), "1".into());
                    env.insert(
                        "GIT_CONFIG_KEY_0".into(),
                        "http.https://github.com/.extraheader".into(),
                    );
                    env.insert(
                        "GIT_CONFIG_VALUE_0".into(),
                        format!("Authorization: Basic {encoded}"),
                    );
                }
                let mut command = vec!["git".into(), "clone".into(), "--single-branch".into()];
                if let Some(branch) = branch {
                    command.extend(["--branch".into(), branch]);
                }
                command.extend([repository, destination.clone()]);
                self.run_command(sandbox_id, command, env).await?;
                if let Some(sha) = sha {
                    validate_sha(&sha)?;
                    self.run_command(
                        sandbox_id,
                        vec![
                            "git".into(),
                            "-C".into(),
                            destination,
                            "checkout".into(),
                            "--detach".into(),
                            sha,
                        ],
                        HashMap::new(),
                    )
                    .await?;
                }
                Ok(())
            }
            SandboxRecipeStep::Command { argv, cwd } => {
                let command = if let Some(cwd) = cwd {
                    if argv.is_empty() {
                        bail!("recipe command must not be empty");
                    }
                    let command = shlex::try_join(argv.iter().map(String::as_str))
                        .context("recipe command contains an invalid argument")?;
                    vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        format!(
                            "cd -- {} && exec {command}",
                            shlex::try_quote(&cwd).map_err(|_| anyhow::anyhow!(
                                "recipe working directory contains an invalid argument"
                            ))?
                        ),
                    ]
                } else {
                    argv
                };
                self.run_command(sandbox_id, command, HashMap::new()).await
            }
        }
    }

    async fn run_command(
        &self,
        sandbox_id: &SandboxId,
        command: Vec<String>,
        env: HashMap<String, String>,
    ) -> Result<()> {
        if command.is_empty() || command[0].trim().is_empty() {
            bail!("recipe command must not be empty");
        }
        let process = self
            .sandbox
            .run_in_sandbox(RunInSandboxRequest {
                id: sandbox_id.clone(),
                command,
                env,
            })
            .await?;
        let parts = process.into_parts();
        let (mut stdout, mut stderr) = (parts.stdout, parts.stderr);
        let (stdout, stderr, exit_code) = tokio::try_join!(
            async {
                let mut output = Vec::new();
                stdout
                    .read_to_end(&mut output)
                    .await
                    .map(|_| output)
                    .map_err(anyhow::Error::from)
            },
            async {
                let mut output = Vec::new();
                stderr
                    .read_to_end(&mut output)
                    .await
                    .map(|_| output)
                    .map_err(anyhow::Error::from)
            },
            parts.wait,
        )?;
        drop(stdout);
        if exit_code != 0 {
            bail!(
                "recipe command failed: {}",
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        Ok(())
    }
}

fn validate_github_repository(repository: &str) -> Result<()> {
    let url = url::Url::parse(repository).context("GitHub repository must be an HTTPS URL")?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        bail!("GitHub repository must be an HTTPS github.com URL");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path().trim_matches('/').is_empty()
    {
        bail!("GitHub repository URL must not contain credentials");
    }
    Ok(())
}

fn validate_sha(sha: &str) -> Result<()> {
    if !(40..=64).contains(&sha.len()) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("GitHub SHA must be a 40- to 64-character hexadecimal object id");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_repository_must_not_contain_credentials() {
        assert!(validate_github_repository("https://github.com/org/repo").is_ok());
        assert!(validate_github_repository("https://user:token@github.com/org/repo").is_err());
        assert!(validate_github_repository("http://github.com/org/repo").is_err());
        assert!(validate_github_repository("https://example.com/org/repo").is_err());
    }

    #[test]
    fn github_sha_must_be_a_hex_object_id() {
        assert!(validate_sha(&"a".repeat(40)).is_ok());
        assert!(validate_sha(&"a".repeat(39)).is_err());
        assert!(validate_sha(&format!("{}g", "a".repeat(39))).is_err());
    }
}

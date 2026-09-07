use std::collections::HashMap;

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};

use crate::Result;
use crate::sandbox::{ManagedSandboxHandle, SandboxCommand};

fn validate_repository(repository: &str) -> Result<()> {
    let url = url::Url::parse(repository).context("GitHub repository must be an HTTPS URL")?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        bail!("GitHub repository must be an HTTPS github.com URL");
    }
    if url.username() != "" || url.password().is_some() || url.path().trim_matches('/').is_empty() {
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

async fn run_git(
    sandbox: &dyn ManagedSandboxHandle,
    argv: impl IntoIterator<Item = impl Into<String>>,
    env: HashMap<String, String>,
    error: &str,
) -> Result<()> {
    let output = sandbox
        .exec(&SandboxCommand {
            argv: argv.into_iter().map(Into::into).collect(),
            env,
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await?;
    if !output.ok {
        bail!("{error}: {}", output.stderr.trim());
    }
    Ok(())
}

pub(crate) async fn clone_branch(
    sandbox: &dyn ManagedSandboxHandle,
    repository: &str,
    branch: Option<&str>,
    destination: &str,
    token: Option<&str>,
) -> Result<()> {
    validate_repository(repository)?;
    // Keep the token scoped to `git clone`; it never lands in argv or `.git/config`.
    let env = token.map_or_else(HashMap::new, |token| {
        let encoded = STANDARD.encode(format!("x-access-token:{token}"));
        HashMap::from([
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            (
                "GIT_CONFIG_KEY_0".into(),
                "http.https://github.com/.extraheader".into(),
            ),
            (
                "GIT_CONFIG_VALUE_0".into(),
                format!("Authorization: Basic {encoded}"),
            ),
        ])
    });
    let mut argv = vec!["git", "clone", "--single-branch"];
    if let Some(branch) = branch {
        argv.extend(["--branch", branch]);
    }
    argv.extend(["--", repository, destination]);
    run_git(sandbox, argv, env, "GitHub clone failed").await
}

pub(crate) async fn checkout_sha(
    sandbox: &dyn ManagedSandboxHandle,
    destination: &str,
    sha: &str,
) -> Result<()> {
    validate_sha(sha)?;
    run_git(
        sandbox,
        ["git", "-C", destination, "checkout", "--detach", sha],
        HashMap::new(),
        "GitHub SHA checkout failed",
    )
    .await
}

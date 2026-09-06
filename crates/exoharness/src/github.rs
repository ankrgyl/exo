use std::collections::HashMap;

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};

use crate::Result;
use crate::sandbox::{ManagedSandboxHandle, SandboxCommand};

pub(crate) fn validate_repository_source(
    repository: &str,
    branch: Option<&str>,
    sha: Option<&str>,
    destination: &str,
) -> Result<()> {
    let url = url::Url::parse(repository).context("GitHub repository must be an HTTPS URL")?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        bail!("GitHub repository must be an HTTPS github.com URL");
    }
    if url.username() != "" || url.password().is_some() || url.path().trim_matches('/').is_empty() {
        bail!("GitHub repository URL must not contain credentials");
    }
    if let Some(branch) = branch
        && (branch.trim().is_empty() || branch.contains('\0'))
    {
        bail!("GitHub recipe branch must be a non-empty ref name");
    }
    if let Some(sha) = sha
        && (!(40..=64).contains(&sha.len()) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        bail!("GitHub recipe SHA must be a 40- to 64-character hexadecimal Git object id");
    }
    if destination.trim().is_empty() || destination.contains('\0') {
        bail!("GitHub recipe destination must be a non-empty path");
    }
    Ok(())
}

pub(crate) async fn clone_repository_branch(
    sandbox: &dyn ManagedSandboxHandle,
    repository: &str,
    branch: Option<&str>,
    destination: &str,
    token: Option<&str>,
) -> Result<()> {
    let mut env = HashMap::new();
    if let Some(token) = token {
        let encoded = STANDARD.encode(format!("x-access-token:{token}"));
        env.insert("GIT_CONFIG_COUNT".to_string(), "1".to_string());
        env.insert(
            "GIT_CONFIG_KEY_0".to_string(),
            "http.https://github.com/.extraheader".to_string(),
        );
        env.insert(
            "GIT_CONFIG_VALUE_0".to_string(),
            format!("Authorization: Basic {encoded}"),
        );
    }
    let mut argv = vec![
        "git".to_string(),
        "clone".to_string(),
        "--single-branch".to_string(),
    ];
    if let Some(branch) = branch {
        argv.extend(["--branch".to_string(), branch.to_string()]);
    }
    argv.extend([repository.to_string(), destination.to_string()]);
    let output = sandbox
        .exec(&SandboxCommand {
            argv,
            env,
            display_argv: Some(vec!["git clone GitHub repository".to_string()]),
            cwd: None,
            timeout: None,
        })
        .await?;
    if !output.ok {
        bail!("GitHub recipe clone failed: {}", output.stderr.trim());
    }
    Ok(())
}

pub(crate) async fn checkout_sha(
    sandbox: &dyn ManagedSandboxHandle,
    destination: &str,
    sha: &str,
) -> Result<()> {
    let output = sandbox
        .exec(&SandboxCommand {
            argv: vec![
                "git".to_string(),
                "-C".to_string(),
                destination.to_string(),
                "checkout".to_string(),
                "--detach".to_string(),
                sha.to_string(),
            ],
            env: HashMap::new(),
            display_argv: Some(vec!["git checkout requested SHA".to_string()]),
            cwd: None,
            timeout: None,
        })
        .await?;
    if !output.ok {
        bail!(
            "GitHub recipe SHA checkout failed: {}",
            output.stderr.trim()
        );
    }
    Ok(())
}

//! Vercel remote sandbox backend, speaking the Vercel Sandbox REST API.
//!
//! This backend supports named acquire/resume, one-shot command execution, and
//! stdin/stdout-backed processes through a small generic in-sandbox bridge.

const DEFAULT_VERCEL_IMAGE: &str = "node24";

pub fn default_vercel_image() -> String {
    DEFAULT_VERCEL_IMAGE.to_string()
}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::SandboxAttachment;
use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotFormat, SnapshotPayload,
    WARM_SANDBOX_KEY_LABEL, WARM_SANDBOX_SPEC_HASH_LABEL, sandbox_spec_hash,
};
use crate::sandbox_provider::{process_bridge, shell_quote};

pub const DEFAULT_VERCEL_API_URL: &str = "https://vercel.com/api";

#[derive(Debug, Clone)]
pub struct VercelConfig {
    pub api_token: String,
    pub api_url: String,
    pub team_id: String,
    pub project_id: String,
}

pub struct VercelSandboxBackend {
    client: reqwest::Client,
    api_url: String,
    team_id: String,
    project_id: String,
}

impl VercelSandboxBackend {
    pub fn new(config: VercelConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", config.api_token))
            .context("Vercel API token contains characters that aren't valid in an HTTP header")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("building Vercel HTTP client")?;
        Ok(Self {
            client,
            api_url: config.api_url.trim_end_matches('/').to_string(),
            team_id: config.team_id,
            project_id: config.project_id,
        })
    }

    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    async fn get_sandbox_session(
        &self,
        name: &str,
    ) -> Result<Option<VercelSandboxSessionResponse>> {
        let response = self
            .client
            .get(self.api_endpoint(&format!("/v2/sandboxes/{name}")))
            .query(&[
                ("teamId", self.team_id.as_str()),
                ("projectId", self.project_id.as_str()),
                ("resume", "true"),
            ])
            .send()
            .await
            .with_context(|| format!("fetching Vercel sandbox {name}"))?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("Vercel get-sandbox failed ({status}): {text}");
        }
        Ok(Some(response.json().await.with_context(|| {
            format!("decoding Vercel sandbox {name}")
        })?))
    }

    async fn create_sandbox(
        &self,
        request: &SandboxRequest,
        name: &str,
        spec_hash: &str,
    ) -> Result<VercelSandboxSessionResponse> {
        let mut tags = HashMap::new();
        tags.insert(WARM_SANDBOX_KEY_LABEL.to_string(), request.key.to_string());
        tags.insert(
            WARM_SANDBOX_SPEC_HASH_LABEL.to_string(),
            spec_hash.to_string(),
        );

        let runtime = match request.spec.image.trim() {
            "" => None,
            image => Some(image.to_string()),
        };
        let body = VercelCreateSandboxRequest {
            project_id: self.project_id.clone(),
            runtime,
            name: name.to_string(),
            persistent: true,
            timeout: request.lifecycle.idle_ttl.map(duration_to_millis),
            env: HashMap::new(),
            tags,
            network_policy: match request.spec.network {
                SandboxNetworkPolicy::Enabled => None,
                SandboxNetworkPolicy::Disabled => Some(VercelNetworkPolicy {
                    mode: "deny-all".to_string(),
                }),
            },
        };

        let response = self
            .client
            .post(self.api_endpoint("/v2/sandboxes"))
            .query(&[("teamId", self.team_id.as_str())])
            .json(&body)
            .send()
            .await
            .context("creating Vercel sandbox")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("Vercel create-sandbox failed ({status}): {text}");
        }
        response
            .json()
            .await
            .context("decoding Vercel create-sandbox response")
    }
}

#[async_trait]
impl ManagedSandboxBackend for VercelSandboxBackend {
    fn is_local(&self) -> bool {
        false
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &[]
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_unsupported_mounts(&request)?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        let sandbox_name = vercel_sandbox_name(&request, &spec_hash);
        let response = match self.get_sandbox_session(&sandbox_name).await? {
            Some(existing) => existing,
            None => {
                self.create_sandbox(&request, &sandbox_name, &spec_hash)
                    .await?
            }
        };

        Ok(Arc::new(VercelSandboxHandle {
            id: format!("vercel:{sandbox_name}"),
            sandbox_name,
            session_id: response.session.id,
            request,
            backend: self.handle_backend(),
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Vercel sandbox backend does not support external attachments")
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("restoring a Vercel sandbox from a snapshot is not implemented yet");
    }

    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        let spec_hash = sandbox_spec_hash(&request.spec);
        let sandbox_name = vercel_sandbox_name(&request, &spec_hash);
        let Some(session) = self.get_sandbox_session(&sandbox_name).await? else {
            return Ok(());
        };
        stop_session(&self.handle_backend(), &session.session.id)
            .await
            .with_context(|| format!("terminating Vercel sandbox {sandbox_name}"))
    }
}

impl VercelSandboxBackend {
    fn handle_backend(&self) -> VercelBackendHandle {
        VercelBackendHandle {
            client: self.client.clone(),
            api_url: self.api_url.clone(),
            team_id: self.team_id.clone(),
        }
    }
}

#[derive(Clone)]
struct VercelBackendHandle {
    client: reqwest::Client,
    api_url: String,
    team_id: String,
}

impl VercelBackendHandle {
    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }
}

struct VercelSandboxHandle {
    id: String,
    sandbox_name: String,
    session_id: String,
    request: SandboxRequest,
    backend: VercelBackendHandle,
}

#[async_trait]
impl ManagedSandboxHandle for VercelSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_in_sandbox(&self.backend, &self.session_id, &self.request.spec, command).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        start_process_in_sandbox(&self.backend, &self.session_id, &self.request.spec, command).await
    }

    async fn stop(&self) -> Result<()> {
        stop_session(&self.backend, &self.session_id)
            .await
            .with_context(|| format!("stopping Vercel sandbox {}", self.sandbox_name))
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("Vercel sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!("Vercel sandbox snapshots are not implemented yet");
    }
}

async fn start_process_in_sandbox(
    backend: &VercelBackendHandle,
    session_id: &str,
    spec: &SandboxSpec,
    command: &SandboxCommand,
) -> Result<crate::SandboxProcessParts> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }
    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());
    // Vercel exposes one-shot command execution, but not a native streaming
    // process handle. We emulate one with a single in-sandbox bridge per
    // sandbox session, so starting another long-running process would sever
    // the existing handle.
    if process_bridge_ping(backend, session_id, &cwd).await? {
        bail!(
            "Vercel sandbox backend supports only one active long-running process per sandbox session"
        );
    }
    install_process_bridge_script(backend, session_id, &cwd).await?;
    ensure_process_bridge_running(backend, session_id, &cwd, command).await?;
    let client = VercelProcessBridgeClient {
        backend: backend.clone(),
        session_id: session_id.to_string(),
        cwd,
    };
    Ok(process_bridge::process_parts(Arc::new(client)))
}

async fn exec_in_sandbox(
    backend: &VercelBackendHandle,
    session_id: &str,
    spec: &SandboxSpec,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());
    exec_command_in_sandbox(backend, session_id, cwd, command).await
}

async fn exec_command_in_sandbox(
    backend: &VercelBackendHandle,
    session_id: &str,
    cwd: String,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }
    if command.timeout.is_some() {
        bail!("Vercel sandbox exec does not support per-command timeout yet");
    }
    let body = VercelCommandRequest {
        command: command.argv[0].clone(),
        args: command.argv[1..].to_vec(),
        cwd: Some(cwd.clone()),
        env: command.env.clone(),
        sudo: false,
        wait: true,
    };
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/v2/sandboxes/sessions/{session_id}/cmd")))
        .query(&[("teamId", backend.team_id.as_str())])
        .json(&body)
        .send()
        .await
        .with_context(|| format!("running command in Vercel sandbox session {session_id}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Vercel run-command failed ({status}): {text}");
    }
    let text = response
        .text()
        .await
        .context("decoding Vercel command response stream")?;
    let finished = parse_command_response_stream(&text)?;
    let logs = collect_command_logs(backend, session_id, &finished.id).await?;
    Ok(SandboxCommandOutput {
        ok: finished.exit_code == 0,
        exit_code: Some(finished.exit_code),
        stdout: logs.stdout,
        stderr: logs.stderr,
        command: command
            .display_argv
            .clone()
            .unwrap_or_else(|| command.argv.clone()),
        cwd,
    })
}

async fn install_process_bridge_script(
    backend: &VercelBackendHandle,
    session_id: &str,
    cwd: &str,
) -> Result<()> {
    let output = exec_command_in_sandbox(
        backend,
        session_id,
        cwd.to_string(),
        &SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                process_bridge::install_script_shell_command(),
            ],
            env: HashMap::new(),
            display_argv: None,
            cwd: Some(cwd.to_string()),
            timeout: None,
        },
    )
    .await?;
    if output.ok {
        return Ok(());
    }
    bail!(
        "installing process bridge failed with exit code {:?}: {}{}",
        output.exit_code,
        output.stdout,
        output.stderr
    )
}

async fn ensure_process_bridge_running(
    backend: &VercelBackendHandle,
    session_id: &str,
    cwd: &str,
    command: &SandboxCommand,
) -> Result<()> {
    stop_existing_process_bridge(backend, session_id, cwd).await?;
    let argv_json = serde_json::to_string(&command.argv).context("encoding bridge argv")?;
    let env_json = serde_json::to_string(&command.env).context("encoding bridge env")?;
    let command = format!(
        "set -e; export EXO_PROCESS_BRIDGE_ARGV_JSON={}; export EXO_PROCESS_BRIDGE_ENV_JSON={}; export EXO_PROCESS_BRIDGE_CWD={}; nohup {} >/tmp/exo-process-bridge.out 2>&1 </dev/null &",
        shell_quote(&argv_json),
        shell_quote(&env_json),
        shell_quote(cwd),
        process_bridge::server_shell_command(),
    );
    let output = exec_command_in_sandbox(
        backend,
        session_id,
        cwd.to_string(),
        &SandboxCommand {
            argv: vec!["/bin/sh".to_string(), "-lc".to_string(), command],
            env: HashMap::new(),
            display_argv: None,
            cwd: Some(cwd.to_string()),
            timeout: None,
        },
    )
    .await?;
    if !output.ok {
        bail!(
            "starting process bridge failed with exit code {:?}: {}{}",
            output.exit_code,
            output.stdout,
            output.stderr
        );
    }
    for _ in 0..600 {
        if process_bridge_ping(backend, session_id, cwd).await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let logs = process_bridge_logs(backend, session_id, cwd)
        .await
        .unwrap_or_default();
    bail!("process bridge did not become ready in Vercel sandbox: {logs}");
}

async fn stop_existing_process_bridge(
    backend: &VercelBackendHandle,
    session_id: &str,
    cwd: &str,
) -> Result<()> {
    let output = exec_command_in_sandbox(
        backend,
        session_id,
        cwd.to_string(),
        &SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                process_bridge::stop_shell_command(),
            ],
            env: HashMap::new(),
            display_argv: None,
            cwd: Some(cwd.to_string()),
            timeout: None,
        },
    )
    .await?;
    if output.ok {
        return Ok(());
    }
    bail!(
        "stopping existing process bridge failed with exit code {:?}: {}{}",
        output.exit_code,
        output.stdout,
        output.stderr
    )
}

async fn process_bridge_ping(
    backend: &VercelBackendHandle,
    session_id: &str,
    cwd: &str,
) -> Result<bool> {
    let client = VercelProcessBridgeClient {
        backend: backend.clone(),
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
    };
    match process_bridge::Client::request(&client, process_bridge::Request::ping()).await {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

async fn process_bridge_logs(
    backend: &VercelBackendHandle,
    session_id: &str,
    cwd: &str,
) -> Result<String> {
    let output = exec_command_in_sandbox(
        backend,
        session_id,
        cwd.to_string(),
        &SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "cat /tmp/exo-process-bridge.out /tmp/exo-process-bridge.log 2>/dev/null || true"
                    .to_string(),
            ],
            env: HashMap::new(),
            display_argv: None,
            cwd: Some(cwd.to_string()),
            timeout: None,
        },
    )
    .await?;
    Ok(format!("{}{}", output.stdout, output.stderr))
}

struct VercelProcessBridgeClient {
    backend: VercelBackendHandle,
    session_id: String,
    cwd: String,
}

#[async_trait]
impl process_bridge::Client for VercelProcessBridgeClient {
    async fn request(&self, request: process_bridge::Request) -> Result<process_bridge::Response> {
        let request = serde_json::to_string(&request).context("encoding process bridge request")?;
        let output = exec_command_in_sandbox(
            &self.backend,
            &self.session_id,
            self.cwd.clone(),
            &SandboxCommand {
                argv: process_bridge::client_argv(request),
                env: HashMap::new(),
                display_argv: None,
                cwd: Some(self.cwd.clone()),
                timeout: None,
            },
        )
        .await?;
        if !output.ok {
            bail!(
                "process bridge request failed with exit code {:?}: {}{}",
                output.exit_code,
                output.stdout,
                output.stderr
            );
        }
        let decoded: process_bridge::Response = serde_json::from_str(output.stdout.trim())
            .context("decoding process bridge response")?;
        if !decoded.ok {
            bail!(
                "process bridge request failed: {}",
                decoded
                    .error
                    .as_deref()
                    .unwrap_or("unknown process bridge error")
            );
        }
        Ok(decoded)
    }
}

async fn collect_command_logs(
    backend: &VercelBackendHandle,
    session_id: &str,
    command_id: &str,
) -> Result<VercelCommandLogs> {
    let response = backend
        .client
        .get(backend.api_endpoint(&format!(
            "/v2/sandboxes/sessions/{session_id}/cmd/{command_id}/logs"
        )))
        .query(&[("teamId", backend.team_id.as_str())])
        .send()
        .await
        .with_context(|| {
            format!("fetching Vercel command logs for session {session_id} command {command_id}")
        })?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Vercel command logs failed ({status}): {text}");
    }
    let text = response
        .text()
        .await
        .context("decoding Vercel command log stream")?;
    parse_log_stream(&text)
}

async fn stop_session(backend: &VercelBackendHandle, session_id: &str) -> Result<()> {
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/v2/sandboxes/sessions/{session_id}/stop")))
        .query(&[("teamId", backend.team_id.as_str())])
        .send()
        .await
        .with_context(|| format!("stopping Vercel sandbox session {session_id}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Vercel stop-session failed ({status}): {text}");
    }
    Ok(())
}

fn parse_command_response_stream(text: &str) -> Result<VercelFinishedCommand> {
    let mut command_id = None;
    let mut exit_code = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let response: VercelCommandResponse =
            serde_json::from_str(line).context("decoding Vercel command response line")?;
        if command_id.is_none() {
            command_id = Some(response.command.id.clone());
        }
        if let Some(code) = response.command.exit_code {
            exit_code = Some(code);
        }
    }
    Ok(VercelFinishedCommand {
        id: command_id.context("Vercel command response did not include a command id")?,
        exit_code: exit_code.context("Vercel command response did not include an exit code")?,
    })
}

fn parse_log_stream(text: &str) -> Result<VercelCommandLogs> {
    let mut logs = VercelCommandLogs::default();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<VercelLogLine>(line).context("decoding Vercel log line")? {
            VercelLogLine::Stdout { data } => logs.stdout.push_str(&data),
            VercelLogLine::Stderr { data } => logs.stderr.push_str(&data),
            VercelLogLine::Error { data } => {
                bail!("Vercel command log error {}: {}", data.code, data.message)
            }
        }
    }
    Ok(logs)
}

fn vercel_sandbox_name(request: &SandboxRequest, spec_hash: &str) -> String {
    let key = format!("{}\n{spec_hash}", request.key);
    format!("exo-{}", stable_fnv1a_hex(&key))
}

fn stable_fnv1a_hex(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn reject_unsupported_mounts(request: &SandboxRequest) -> Result<()> {
    if !request.spec.mounts.is_empty() {
        bail!(
            "Vercel sandbox backend does not support host bind-mounts; \
         remove conversation mounts or use a local sandbox provider"
        );
    }
    if !request.spec.durable_file_systems.is_empty() {
        bail!("Vercel sandbox backend does not support durable file systems");
    }
    Ok(())
}

fn duration_to_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Serialize)]
struct VercelCreateSandboxRequest {
    #[serde(rename = "projectId")]
    project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
    name: String,
    persistent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
    env: HashMap<String, String>,
    tags: HashMap<String, String>,
    #[serde(rename = "networkPolicy", skip_serializing_if = "Option::is_none")]
    network_policy: Option<VercelNetworkPolicy>,
}

#[derive(Debug, Serialize)]
struct VercelNetworkPolicy {
    mode: String,
}

#[derive(Debug, Deserialize)]
struct VercelSandboxSessionResponse {
    session: VercelSession,
}

#[derive(Debug, Deserialize)]
struct VercelSession {
    id: String,
}

#[derive(Debug, Serialize)]
struct VercelCommandRequest {
    command: String,
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    env: HashMap<String, String>,
    sudo: bool,
    wait: bool,
}

#[derive(Debug, Deserialize)]
struct VercelCommandResponse {
    command: VercelCommand,
}

#[derive(Debug, Deserialize)]
struct VercelCommand {
    id: String,
    #[serde(default, rename = "exitCode", alias = "exit_code")]
    exit_code: Option<i32>,
}

#[derive(Debug)]
struct VercelFinishedCommand {
    id: String,
    exit_code: i32,
}

#[derive(Default)]
struct VercelCommandLogs {
    stdout: String,
    stderr: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "stream", rename_all = "lowercase")]
enum VercelLogLine {
    Stdout { data: String },
    Stderr { data: String },
    Error { data: VercelLogError },
}

#[derive(Debug, Deserialize)]
struct VercelLogError {
    code: String,
    message: String,
}

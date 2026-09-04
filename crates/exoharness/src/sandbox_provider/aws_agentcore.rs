use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_bedrockagentcore::Client;
use aws_sdk_bedrockagentcore::primitives::Blob;
use serde::{Deserialize, Serialize};

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxRequest, SandboxSpec, SnapshotFormat, SnapshotPayload, sandbox_spec_hash,
    validate_durable_file_systems,
};
use crate::sandbox_provider::process_bridge;
use crate::{
    EgressCapabilities, EgressPolicy, SandboxAttachment, validate_egress_policy_capabilities,
};

pub fn default_aws_agentcore_image() -> String {
    String::new()
}

#[derive(Debug, Clone)]
pub struct AwsAgentCoreConfig {
    pub runtime_arn: String,
    pub region: String,
    pub qualifier: Option<String>,
    pub endpoint_url: Option<String>,
    pub credentials: Option<AwsAgentCoreCredentials>,
    pub session_storage_mount_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AwsAgentCoreCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

pub struct AwsAgentCoreSandboxBackend {
    client: Client,
    runtime_arn: String,
    invoke_target: AwsAgentCoreInvokeTarget,
    qualifier: Option<String>,
    session_storage_mount_path: Option<String>,
}

impl AwsAgentCoreSandboxBackend {
    pub async fn new(config: AwsAgentCoreConfig) -> Result<Self> {
        if config.runtime_arn.trim().is_empty() {
            bail!("AgentCore runtime ARN must not be empty");
        }
        if config.region.trim().is_empty() {
            bail!("AgentCore region must not be empty");
        }
        let region = config.region;
        let mut sdk_config_loader =
            aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region.clone()));
        if let Some(credentials) = config.credentials {
            sdk_config_loader = sdk_config_loader.credentials_provider(Credentials::new(
                credentials.access_key_id,
                credentials.secret_access_key,
                credentials.session_token,
                None,
                "aws-agentcore",
            ));
        }
        let sdk_config = sdk_config_loader.load().await;
        let mut service_config_builder =
            aws_sdk_bedrockagentcore::config::Builder::from(&sdk_config);
        if let Some(endpoint_url) = config.endpoint_url {
            service_config_builder = service_config_builder.endpoint_url(endpoint_url);
        } else {
            service_config_builder.set_endpoint_url(None);
        }
        let service_config = service_config_builder.build();
        let invoke_target = agentcore_invoke_target(&config.runtime_arn)?;
        let session_storage_mount_path = config
            .session_storage_mount_path
            .as_deref()
            .map(normalize_agentcore_session_storage_mount_path)
            .transpose()?;
        Ok(Self {
            client: Client::from_conf(service_config),
            runtime_arn: config.runtime_arn,
            invoke_target,
            qualifier: config.qualifier,
            session_storage_mount_path,
        })
    }
}

#[async_trait]
impl ManagedSandboxBackend for AwsAgentCoreSandboxBackend {
    fn is_local(&self) -> bool {
        false
    }

    fn egress_capabilities(&self) -> EgressCapabilities {
        EgressCapabilities::default()
    }

    fn validate_egress_policy(&self, policy: &EgressPolicy) -> Result<()> {
        validate_egress_policy_capabilities(policy, self.egress_capabilities())
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &[]
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        self.validate_egress_policy(&request.spec.egress_policy)?;
        reject_unsupported_request(&request, self.session_storage_mount_path.as_deref())?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        let runtime_session_id = agentcore_runtime_session_id(&request, &spec_hash);
        Ok(Arc::new(AwsAgentCoreSandboxHandle {
            id: format!("aws-agentcore:{runtime_session_id}"),
            runtime_session_id,
            request,
            backend: AwsAgentCoreBackendHandle {
                client: self.client.clone(),
                runtime_arn: self.runtime_arn.clone(),
                invoke_target: self.invoke_target.clone(),
                qualifier: self.qualifier.clone(),
            },
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("AWS AgentCore sandbox backend does not support external attachments")
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("restoring an AgentCore sandbox from a snapshot is not implemented yet");
    }
}

#[derive(Clone)]
struct AwsAgentCoreBackendHandle {
    client: Client,
    runtime_arn: String,
    invoke_target: AwsAgentCoreInvokeTarget,
    qualifier: Option<String>,
}

#[derive(Clone)]
struct AwsAgentCoreInvokeTarget {
    runtime_identifier: String,
    account_id: String,
}

struct AwsAgentCoreSandboxHandle {
    id: String,
    runtime_session_id: String,
    request: SandboxRequest,
    backend: AwsAgentCoreBackendHandle,
}

#[async_trait]
impl ManagedSandboxHandle for AwsAgentCoreSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_in_agentcore(
            &self.backend,
            &self.runtime_session_id,
            &self.request.spec,
            command,
        )
        .await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        start_process_in_agentcore(
            &self.backend,
            &self.runtime_session_id,
            &self.request.spec,
            command,
        )
        .await
    }

    async fn stop(&self) -> Result<()> {
        let mut request = self
            .backend
            .client
            .stop_runtime_session()
            .agent_runtime_arn(self.backend.runtime_arn.clone())
            .runtime_session_id(self.runtime_session_id.clone());
        if let Some(qualifier) = &self.backend.qualifier {
            request = request.qualifier(qualifier.clone());
        }
        request.send().await.with_context(|| {
            format!(
                "stopping AgentCore runtime session {}",
                self.runtime_session_id
            )
        })?;
        Ok(())
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("AWS AgentCore sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!("AgentCore sandbox snapshots are not implemented yet");
    }
}

async fn exec_in_agentcore(
    backend: &AwsAgentCoreBackendHandle,
    runtime_session_id: &str,
    spec: &SandboxSpec,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }
    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());
    let body = AgentCoreExecRequest {
        request_type: "exec",
        argv: command.argv.clone(),
        env: command.env.clone(),
        cwd: cwd.clone(),
        timeout_ms: command.timeout.map(duration_to_millis),
    };
    let response: AgentCoreExecResponse =
        invoke_agentcore_json(backend, runtime_session_id, &body).await?;
    if let Some(error) = response.error {
        bail!("AgentCore exec failed: {error}");
    }
    let exit_code = response
        .exit_code
        .context("AgentCore exec response did not include exit_code")?;
    let ok = response.ok.unwrap_or(exit_code == 0);
    Ok(SandboxCommandOutput {
        ok,
        exit_code: Some(exit_code),
        stdout: response.stdout.unwrap_or_default(),
        stderr: response.stderr.unwrap_or_default(),
        command: command.argv.clone(),
        cwd: response.cwd.unwrap_or(cwd),
    })
}

async fn start_process_in_agentcore(
    backend: &AwsAgentCoreBackendHandle,
    runtime_session_id: &str,
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
    let response: AgentCoreStartProcessResponse = invoke_agentcore_json(
        backend,
        runtime_session_id,
        &AgentCoreStartProcessRequest {
            request_type: "start_process",
            argv: command.argv.clone(),
            env: command.env.clone(),
            cwd,
        },
    )
    .await?;
    let client = AwsAgentCoreProcessBridgeClient {
        backend: backend.clone(),
        runtime_session_id: runtime_session_id.to_string(),
        process_id: response.process_id,
    };
    Ok(process_bridge::process_parts(Arc::new(client)))
}

struct AwsAgentCoreProcessBridgeClient {
    backend: AwsAgentCoreBackendHandle,
    runtime_session_id: String,
    process_id: String,
}

#[async_trait]
impl process_bridge::Client for AwsAgentCoreProcessBridgeClient {
    async fn request(&self, request: process_bridge::Request) -> Result<process_bridge::Response> {
        invoke_agentcore_json(
            &self.backend,
            &self.runtime_session_id,
            &AgentCoreProcessBridgeRequest {
                request_type: "process_bridge",
                process_id: self.process_id.clone(),
                request,
            },
        )
        .await
    }
}

async fn invoke_agentcore_json<Request, Response>(
    backend: &AwsAgentCoreBackendHandle,
    runtime_session_id: &str,
    body: &Request,
) -> Result<Response>
where
    Request: Serialize,
    Response: for<'de> Deserialize<'de>,
{
    let payload = serde_json::to_vec(body).context("serializing AgentCore runtime request")?;
    let mut request = backend
        .client
        .invoke_agent_runtime()
        .agent_runtime_arn(backend.invoke_target.runtime_identifier.clone())
        .account_id(backend.invoke_target.account_id.clone())
        .runtime_session_id(runtime_session_id.to_string())
        .content_type("application/json")
        .accept("application/json")
        .payload(Blob::new(payload));
    if let Some(qualifier) = &backend.qualifier {
        request = request.qualifier(qualifier.clone());
    }
    let output = request.send().await.with_context(|| {
        format!(
            "invoking AgentCore runtime session {runtime_session_id} with runtime {} in account {}",
            backend.invoke_target.runtime_identifier, backend.invoke_target.account_id,
        )
    })?;
    let status_code = output.status_code.unwrap_or(200);
    let bytes = output
        .response
        .collect()
        .await
        .context("reading AgentCore runtime response body")?
        .into_bytes();
    if !(200..300).contains(&status_code) {
        let text = String::from_utf8_lossy(&bytes);
        bail!("AgentCore runtime returned status {status_code}: {text}");
    }
    serde_json::from_slice(&bytes).with_context(|| {
        let text = String::from_utf8_lossy(&bytes);
        format!("decoding AgentCore runtime JSON response: {text}")
    })
}

fn agentcore_invoke_target(runtime_arn: &str) -> Result<AwsAgentCoreInvokeTarget> {
    let mut parts = runtime_arn.splitn(6, ':');
    let arn = parts.next();
    let _partition = parts.next();
    let service = parts.next();
    let _region = parts.next();
    let account_id = parts.next();
    let resource = parts.next();
    let runtime_identifier = resource.and_then(|resource| resource.strip_prefix("runtime/"));
    match (arn, service, account_id, runtime_identifier) {
        (Some("arn"), Some("bedrock-agentcore"), Some(account_id), Some(runtime_identifier))
            if !account_id.is_empty() && !runtime_identifier.is_empty() =>
        {
            Ok(AwsAgentCoreInvokeTarget {
                runtime_identifier: runtime_identifier.to_string(),
                account_id: account_id.to_string(),
            })
        }
        _ => bail!(
            "AgentCore runtime ARN must have the form arn:...:bedrock-agentcore:...:<account-id>:runtime/<runtime-id>"
        ),
    }
}

fn reject_unsupported_request(
    request: &SandboxRequest,
    session_storage_mount_path: Option<&str>,
) -> Result<()> {
    if !request.spec.mounts.is_empty() {
        bail!(
            "AgentCore sandbox backend does not support host bind-mounts; remove conversation mounts or use a local sandbox provider"
        );
    }
    validate_durable_file_systems(&request.spec.durable_file_systems)?;
    match request.spec.durable_file_systems.as_slice() {
        [] => {}
        [file_system] => {
            if matches!(file_system.mode, crate::FileSystemMountMode::ReadOnly) {
                bail!("AgentCore sandbox backend does not support read-only durable file systems");
            }
            let requested_mount_path =
                normalize_agentcore_session_storage_mount_path(&file_system.mount_path)?;
            // AgentCore managed session storage is provisioned on the runtime by
            // create/update-agent-runtime, not on InvokeAgentRuntime. Exo stores
            // the mount path it expects here so a durable FS request fails before
            // we run work against a runtime that was built without matching
            // filesystemConfigurations.
            let Some(configured_mount_path) = session_storage_mount_path else {
                bail!(
                    "AgentCore sandbox backend was not configured with a managed session storage mount path; configure the runtime with filesystemConfigurations and set session_storage_mount_path"
                );
            };
            if requested_mount_path != configured_mount_path {
                bail!(
                    "AgentCore sandbox durable file system mount path {requested_mount_path:?} does not match configured managed session storage mount path {configured_mount_path:?}"
                );
            }
        }
        _ => {
            bail!("AgentCore sandbox backend supports at most one durable file system");
        }
    }
    if request.spec.egress_policy.default_deny {
        bail!("AgentCore sandbox backend cannot enforce disabled networking");
    }
    Ok(())
}

fn normalize_agentcore_session_storage_mount_path(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if !(6..=200).contains(&trimmed.len()) {
        bail!("AgentCore session storage mount path must be between 6 and 200 characters: {path}");
    }
    let Some(suffix) = trimmed.strip_prefix("/mnt/") else {
        bail!("AgentCore session storage mount path must be under /mnt/: {path}");
    };
    let suffix = suffix.trim_end_matches('/');
    if suffix.is_empty() || suffix.contains('/') {
        bail!(
            "AgentCore session storage mount path must have exactly one subdirectory under /mnt/: {path}"
        );
    }
    if !suffix
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!(
            "AgentCore session storage mount path may only contain letters, numbers, '.', '_', and '-' after /mnt/: {path}"
        );
    }
    Ok(format!("/mnt/{suffix}"))
}

fn agentcore_runtime_session_id(request: &SandboxRequest, spec_hash: &str) -> String {
    let input = format!("{}\n{spec_hash}", request.key);
    format!("exo-{}-session-0000000000", stable_fnv1a_hex(&input))
}

fn stable_fnv1a_hex(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn duration_to_millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Serialize)]
struct AgentCoreExecRequest {
    #[serde(rename = "type")]
    request_type: &'static str,
    argv: Vec<String>,
    env: std::collections::HashMap<String, String>,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct AgentCoreStartProcessRequest {
    #[serde(rename = "type")]
    request_type: &'static str,
    argv: Vec<String>,
    env: std::collections::HashMap<String, String>,
    cwd: String,
}

#[derive(Debug, Deserialize)]
struct AgentCoreStartProcessResponse {
    process_id: String,
}

#[derive(Debug, Serialize)]
struct AgentCoreProcessBridgeRequest {
    #[serde(rename = "type")]
    request_type: &'static str,
    process_id: String,
    request: process_bridge::Request,
}

#[derive(Debug, Deserialize)]
struct AgentCoreExecResponse {
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DurableFileSystem, FileSystemMountMode, SandboxKey, SandboxLifecycleConfig,
        SandboxNetworkPolicy, SandboxSpec,
    };

    #[test]
    fn durable_file_system_requires_configured_session_storage_mount_path() {
        let request = durable_request("/mnt/workspace", FileSystemMountMode::ReadWrite);
        let error = reject_unsupported_request(&request, None).expect_err("missing mount path");
        assert!(
            error
                .to_string()
                .contains("was not configured with a managed session storage mount path")
        );
    }

    #[test]
    fn durable_file_system_must_match_configured_session_storage_mount_path() {
        let request = durable_request("/mnt/workspace", FileSystemMountMode::ReadWrite);
        let error =
            reject_unsupported_request(&request, Some("/mnt/other")).expect_err("mount mismatch");
        assert!(
            error
                .to_string()
                .contains("does not match configured managed session storage mount path")
        );
    }

    #[test]
    fn durable_file_system_accepts_matching_session_storage_mount_path() {
        let request = durable_request("/mnt/workspace", FileSystemMountMode::ReadWrite);
        reject_unsupported_request(&request, Some("/mnt/workspace")).expect("matching mount path");
    }

    #[test]
    fn durable_file_system_rejects_non_agentcore_mount_path_shape() {
        let request = durable_request("/home/exo/workspace", FileSystemMountMode::ReadWrite);
        let error =
            reject_unsupported_request(&request, Some("/mnt/workspace")).expect_err("bad path");
        assert!(error.to_string().contains("must be under /mnt/"));
    }

    fn durable_request(mount_path: &str, mode: FileSystemMountMode) -> SandboxRequest {
        SandboxRequest {
            key: SandboxKey::ConversationSandbox {
                conversation_id: "conversation".to_string(),
                sandbox_id: "sandbox".to_string(),
            },
            spec: SandboxSpec {
                image: "agentcore".to_string(),
                mounts: Vec::new(),
                durable_file_systems: vec![DurableFileSystem {
                    name: "workspace".to_string(),
                    mount_path: mount_path.to_string(),
                    mode,
                }],
                egress_policy: SandboxNetworkPolicy::Enabled.into(),
                default_workdir: "/mnt/workspace".to_string(),
            },
            lifecycle: SandboxLifecycleConfig::default(),
            provider_state: None,
        }
    }
}

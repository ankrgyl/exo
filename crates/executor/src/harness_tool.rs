use std::path::PathBuf;

use crate::adapter::AdapterStore;
use crate::adapter::tools::{
    AdapterCreationOptions, execute_create_adapter_tool, execute_delete_adapter_tool,
    execute_disable_adapter_tool, execute_list_adapter_events_tool, execute_list_adapters_tool,
    execute_send_adapter_message_tool,
};
use crate::agent_sandbox::{current_agent_sandbox, ensure_agent_sandbox};
use crate::conversation_events::execute_list_conversation_events_tool;
use crate::conversation_sandbox::{
    agent_sandbox_spec, conversation_sandbox_spec, conversation_sandboxes,
    ensure_conversation_sandbox,
};
use crate::scheduler_store::SchedulerStore;
use crate::scheduler_types::{
    DEFAULT_MAX_OUTPUT_BYTES, NewScheduledTask, ScheduledTaskSandboxMode,
};
use crate::{AgentConfig, ConversationConfig, ToolRuntime};
use crate::{SandboxScope, effective_sandbox_scope};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, Artifact, ArtifactVersion, ConversationHandle, CreateSandboxRequest, EventData,
    EventKind, EventQuery, EventQueryDirection, FileSystemMount, FileSystemMountMode,
    ReadArtifactRequest, Result, RunInSandboxRequest, SandboxProcess, SandboxProvider, SnapshotId,
    StartSandboxRequest, ToolRequest, ToolResult, TurnHandle, WriteArtifactRequest,
};
use futures::io::AsyncReadExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SANDBOX_SNAPSHOT_REGISTRY_ARTIFACT_PATH: &str = "config/sandbox-snapshots.json";

#[derive(Debug, Clone, Default)]
pub struct BasicToolRuntime;

#[derive(Debug, Clone)]
pub struct ExoToolRuntime {
    scheduler_store: SchedulerStore,
    adapter_store: AdapterStore,
    adapter_creation_options: AdapterCreationOptions,
}

impl ExoToolRuntime {
    pub fn with_roots(
        scheduler_root: impl Into<PathBuf>,
        adapter_root: impl Into<PathBuf>,
        adapter_worker_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            scheduler_store: SchedulerStore::new(scheduler_root),
            adapter_store: AdapterStore::new(adapter_root),
            adapter_creation_options: AdapterCreationOptions::new(adapter_worker_root),
        }
    }
}

#[async_trait]
impl ToolRuntime for BasicToolRuntime {
    async fn prepare_conversation(
        &self,
        _agent: &dyn AgentHandle,
        _conversation: &dyn ConversationHandle,
        _agent_config: &AgentConfig,
        _config: &ConversationConfig,
    ) -> Result<()> {
        Ok(())
    }

    async fn execute(
        &self,
        _agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        _turn: Option<&dyn TurnHandle>,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
        request: &ToolRequest,
    ) -> Result<ToolResult> {
        match request.function_name.as_str() {
            "shell" => execute_shell_tool(conversation, agent_config, config, request).await,
            other => Err(anyhow::anyhow!(
                "tool execution is not configured for {other}"
            )),
        }
    }
}

#[async_trait]
impl ToolRuntime for ExoToolRuntime {
    async fn prepare_conversation(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
    ) -> Result<()> {
        match effective_sandbox_scope(agent_config, config) {
            SandboxScope::Agent => {
                ensure_agent_sandbox(agent, agent_config).await?;
            }
            SandboxScope::Conversation => {
                ensure_conversation_sandbox(conversation, agent_config, config).await?;
            }
        }
        Ok(())
    }

    async fn execute(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Option<&dyn TurnHandle>,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
        request: &ToolRequest,
    ) -> Result<ToolResult> {
        match request.function_name.as_str() {
            "shell" => {
                execute_exo_shell_tool(agent, conversation, agent_config, config, request).await
            }
            "schedule_sandbox_task" => {
                execute_schedule_task_tool(agent, conversation, &self.scheduler_store, request)
                    .await
            }
            "list_scheduled_tasks" => {
                execute_list_scheduled_tasks_tool(
                    agent,
                    conversation,
                    &self.scheduler_store,
                    request,
                )
                .await
            }
            "cancel_scheduled_task" => {
                execute_cancel_scheduled_task_tool(
                    agent,
                    conversation,
                    &self.scheduler_store,
                    request,
                )
                .await
            }
            "delete_scheduled_task" => {
                execute_delete_scheduled_task_tool(
                    agent,
                    conversation,
                    &self.scheduler_store,
                    request,
                )
                .await
            }
            "create_adapter" => {
                execute_create_adapter_tool(
                    agent,
                    conversation,
                    &self.adapter_store,
                    &self.adapter_creation_options,
                    request,
                )
                .await
            }
            "list_adapters" => {
                execute_list_adapters_tool(agent, conversation, &self.adapter_store, request).await
            }
            "list_adapter_events" => {
                execute_list_adapter_events_tool(agent, conversation, &self.adapter_store, request)
                    .await
            }
            "list_conversation_events" => {
                execute_list_conversation_events_tool(conversation, request).await
            }
            "disable_adapter" => {
                execute_disable_adapter_tool(conversation, agent, &self.adapter_store, request)
                    .await
            }
            "delete_adapter" => {
                execute_delete_adapter_tool(conversation, agent, &self.adapter_store, request).await
            }
            "send_adapter_message" => {
                execute_send_adapter_message_tool(
                    agent,
                    conversation,
                    agent_config,
                    config,
                    &self.adapter_store,
                    request,
                )
                .await
            }
            "list_sandbox_snapshots" => {
                execute_list_sandbox_snapshots_tool(
                    agent,
                    conversation,
                    agent_config,
                    config,
                    request,
                )
                .await
            }
            "snapshot_sandbox" => {
                let turn = turn.ok_or_else(|| {
                    anyhow::anyhow!("snapshot_sandbox must run inside an active turn")
                })?;
                execute_snapshot_sandbox_tool(
                    agent,
                    conversation,
                    turn,
                    agent_config,
                    config,
                    request,
                )
                .await
            }
            "rewind_sandbox" => {
                let turn = turn.ok_or_else(|| {
                    anyhow::anyhow!("rewind_sandbox must run inside an active turn")
                })?;
                execute_rewind_sandbox_tool(
                    agent,
                    conversation,
                    turn,
                    agent_config,
                    config,
                    request,
                )
                .await
            }
            other => Err(anyhow::anyhow!(
                "tool execution is not configured for {other}"
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ShellToolArguments {
    command: String,
}

#[derive(Debug, Serialize)]
struct ShellToolResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleTaskArguments {
    name: String,
    schedule: String,
    sandbox_mode: Option<ScheduledTaskSandboxMode>,
    setup_command: Option<Vec<String>>,
    command: Vec<String>,
    report_prompt: String,
    max_output_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversationScopedArguments {
    include_disabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScheduledTaskIdArguments {
    task_id: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SandboxControlScope {
    Agent,
    Conversation,
}

impl SandboxControlScope {
    // When the model does not name a scope, operate on whatever sandbox the
    // convo is configured to use.
    fn resolve(
        scope: Option<Self>,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
    ) -> Self {
        scope.unwrap_or_else(|| match effective_sandbox_scope(agent_config, config) {
            SandboxScope::Agent => Self::Agent,
            SandboxScope::Conversation => Self::Conversation,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Conversation => "conversation",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SandboxScopeArguments {
    scope: Option<SandboxControlScope>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RewindSandboxArguments {
    scope: Option<SandboxControlScope>,
    snapshot_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SandboxSnapshotInfo {
    snapshot_id: String,
    sandbox_id: String,
    owner_conversation_id: Option<String>,
    scope: SandboxControlScope,
    created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SandboxSnapshotRegistry {
    #[serde(default)]
    snapshots: Vec<SandboxSnapshotRecord>,
    #[serde(default)]
    current: Vec<SandboxSnapshotCurrent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SandboxSnapshotRecord {
    snapshot_id: String,
    sandbox_id: String,
    owner_conversation_id: String,
    scope: SandboxControlScope,
    created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SandboxSnapshotCurrent {
    scope: SandboxControlScope,
    owner_conversation_id: String,
    sandbox_id: String,
    snapshot_id: String,
}

async fn execute_schedule_task_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &SchedulerStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<ScheduleTaskArguments>(Value::Object(request.arguments.clone()))?;
    let task = store
        .create_task(NewScheduledTask {
            agent_id: agent.record().id.to_string(),
            conversation_id: conversation.record().id.to_string(),
            name: args.name,
            schedule: args.schedule,
            sandbox_mode: args.sandbox_mode,
            setup_command: args.setup_command,
            command: args.command,
            report_prompt: args.report_prompt,
            max_output_bytes: Some(args.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)),
        })
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "taskId": task.id,
        "name": task.name,
        "schedule": task.schedule,
        "nextRunAtMs": task.next_run_at_ms,
    }))
}

async fn execute_list_scheduled_tasks_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &SchedulerStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ConversationScopedArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let tasks = store
        .list_tasks_for_conversation(
            &agent.record().id.to_string(),
            &conversation.record().id.to_string(),
            args.include_disabled.unwrap_or(false),
        )
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "tasks": tasks,
    }))
}

async fn execute_cancel_scheduled_task_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &SchedulerStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ScheduledTaskIdArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let Some(task) = store.get_task(&args.task_id).await? else {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "scheduled task not found for this conversation",
        }));
    };
    if task.agent_id != agent.record().id.to_string()
        || task.conversation_id != conversation.record().id.to_string()
    {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "scheduled task not found for this conversation",
        }));
    }
    let task_sandbox_id = task.task_sandbox_id.clone();
    store.disable_task(&args.task_id).await?;
    if let Some(sandbox_id) = task_sandbox_id {
        conversation.stop_sandbox(sandbox_id).await?;
    }
    Ok(serde_json::json!({
        "ok": true,
        "taskId": args.task_id,
        "cancelled": true,
    }))
}

async fn execute_delete_scheduled_task_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &SchedulerStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ScheduledTaskIdArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let Some(task) = store.get_task(&args.task_id).await? else {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "scheduled task not found for this conversation",
        }));
    };
    if task.agent_id != agent.record().id.to_string()
        || task.conversation_id != conversation.record().id.to_string()
    {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "scheduled task not found for this conversation",
        }));
    }
    if let Some(sandbox_id) = task.task_sandbox_id.clone() {
        conversation.stop_sandbox(sandbox_id).await?;
    }
    store.delete_task(&args.task_id).await?;
    Ok(serde_json::json!({
        "ok": true,
        "taskId": args.task_id,
        "deleted": true,
        "runsDeleted": true,
    }))
}

async fn execute_list_sandbox_snapshots_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<SandboxScopeArguments>(Value::Object(request.arguments.clone()))?;
    let scope = SandboxControlScope::resolve(args.scope, agent_config, config);
    match scope {
        SandboxControlScope::Agent => {
            let Some(handle) = current_agent_sandbox(agent).await? else {
                return Ok(empty_sandbox_snapshot_result(scope));
            };
            sandbox_snapshot_result(agent, scope, None, handle.sandbox_id).await
        }
        SandboxControlScope::Conversation => {
            let spec = conversation_sandbox_spec(agent_config, config);
            let sandbox = conversation_sandboxes(conversation)
                .await?
                .into_iter()
                .find(|sandbox| sandbox.matches_spec(&spec));
            let Some(sandbox) = sandbox else {
                return Ok(empty_sandbox_snapshot_result(scope));
            };
            sandbox_snapshot_result(
                agent,
                scope,
                Some(conversation.record().id.to_string()),
                sandbox.id,
            )
            .await
        }
    }
}

async fn execute_snapshot_sandbox_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    turn: &dyn TurnHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<SandboxScopeArguments>(Value::Object(request.arguments.clone()))?;
    let scope = SandboxControlScope::resolve(args.scope, agent_config, config);
    match scope {
        SandboxControlScope::Agent => {
            let handle = ensure_agent_sandbox(agent, agent_config).await?;
            let snapshot_id = agent.snapshot_sandbox(handle.sandbox_id.clone()).await?;
            turn.add_events(vec![EventData::SandboxSnapshotted {
                sandbox_id: handle.sandbox_id.clone(),
                snapshot_id,
            }])
            .await?;
            record_sandbox_snapshot(
                agent,
                scope,
                String::new(),
                handle.sandbox_id.clone(),
                snapshot_id.to_string(),
            )
            .await?;
            Ok(serde_json::json!({
                "ok": true,
                "scope": scope.as_str(),
                "sandboxId": handle.sandbox_id,
                "ownerConversationId": null,
                "snapshotId": snapshot_id.to_string(),
            }))
        }
        SandboxControlScope::Conversation => {
            let sandbox_id =
                ensure_conversation_sandbox(conversation, agent_config, config).await?;
            let snapshot_id = turn.snapshot_sandbox(sandbox_id.clone()).await?;
            record_sandbox_snapshot(
                agent,
                scope,
                conversation.record().id.to_string(),
                sandbox_id.clone(),
                snapshot_id.to_string(),
            )
            .await?;
            Ok(serde_json::json!({
                "ok": true,
                "scope": scope.as_str(),
                "sandboxId": sandbox_id,
                "ownerConversationId": conversation.record().id.to_string(),
                "snapshotId": snapshot_id.to_string(),
            }))
        }
    }
}

async fn execute_rewind_sandbox_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    turn: &dyn TurnHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<RewindSandboxArguments>(Value::Object(request.arguments.clone()))?;
    let scope = SandboxControlScope::resolve(args.scope, agent_config, config);
    let snapshot_id = parse_snapshot_id(&args.snapshot_id)?;
    match scope {
        SandboxControlScope::Agent => {
            let spec = agent_sandbox_spec(agent_config);
            let handle = ensure_agent_sandbox(agent, agent_config).await?;
            agent
                .start_sandbox(StartSandboxRequest {
                    id: handle.sandbox_id.clone(),
                    snapshot_id,
                    idle_seconds: Some(spec.idle_seconds),
                    provider: None,
                })
                .await?;
            turn.add_events(vec![EventData::SandboxStarted {
                sandbox_id: handle.sandbox_id.clone(),
                snapshot_id: Some(snapshot_id),
            }])
            .await?;
            record_current_sandbox_snapshot(
                agent,
                scope,
                String::new(),
                handle.sandbox_id.clone(),
                args.snapshot_id.clone(),
            )
            .await?;
            Ok(serde_json::json!({
                "ok": true,
                "scope": scope.as_str(),
                "sandboxId": handle.sandbox_id,
                "ownerConversationId": null,
                "snapshotId": args.snapshot_id,
                "rewound": true,
            }))
        }
        SandboxControlScope::Conversation => {
            let spec = conversation_sandbox_spec(agent_config, config);
            let sandbox_id =
                ensure_conversation_sandbox(conversation, agent_config, config).await?;
            turn.start_sandbox(StartSandboxRequest {
                id: sandbox_id.clone(),
                snapshot_id,
                idle_seconds: Some(spec.idle_seconds),
                provider: None,
            })
            .await?;
            record_current_sandbox_snapshot(
                agent,
                scope,
                conversation.record().id.to_string(),
                sandbox_id.clone(),
                args.snapshot_id.clone(),
            )
            .await?;
            Ok(serde_json::json!({
                "ok": true,
                "scope": scope.as_str(),
                "sandboxId": sandbox_id,
                "ownerConversationId": conversation.record().id.to_string(),
                "snapshotId": args.snapshot_id,
                "rewound": true,
            }))
        }
    }
}

async fn sandbox_snapshot_result(
    agent: &dyn AgentHandle,
    scope: SandboxControlScope,
    owner_conversation_id: Option<String>,
    sandbox_id: String,
) -> Result<ToolResult> {
    let owner_conversation_id = owner_conversation_id.unwrap_or_default();
    let owner_conversation_id_result = match scope {
        SandboxControlScope::Agent => None,
        SandboxControlScope::Conversation => Some(owner_conversation_id.clone()),
    };
    let registry = load_sandbox_snapshot_registry(agent).await?;
    let snapshots = registry
        .snapshots
        .iter()
        .filter(|snapshot| {
            snapshot.scope == scope
                && snapshot.owner_conversation_id == owner_conversation_id
                && snapshot.sandbox_id == sandbox_id
        })
        .map(|snapshot| SandboxSnapshotInfo {
            snapshot_id: snapshot.snapshot_id.clone(),
            sandbox_id: snapshot.sandbox_id.clone(),
            owner_conversation_id: owner_conversation_id_result.clone(),
            scope: snapshot.scope,
            created_at_ms: snapshot.created_at_ms,
        })
        .collect::<Vec<_>>();
    let current_snapshot_id = registry
        .current
        .iter()
        .rev()
        .find(|current| {
            current.scope == scope
                && current.owner_conversation_id == owner_conversation_id
                && current.sandbox_id == sandbox_id
        })
        .map(|current| current.snapshot_id.clone());
    Ok(serde_json::json!({
        "ok": true,
        "scope": scope.as_str(),
        "sandboxId": sandbox_id,
        "ownerConversationId": owner_conversation_id_result,
        "currentSnapshotId": current_snapshot_id,
        "snapshots": snapshots,
    }))
}

fn empty_sandbox_snapshot_result(scope: SandboxControlScope) -> ToolResult {
    serde_json::json!({
        "ok": true,
        "scope": scope.as_str(),
        "sandboxId": null,
        "ownerConversationId": null,
        "currentSnapshotId": null,
        "snapshots": [],
    })
}

async fn record_sandbox_snapshot(
    agent: &dyn AgentHandle,
    scope: SandboxControlScope,
    owner_conversation_id: String,
    sandbox_id: String,
    snapshot_id: String,
) -> Result<()> {
    let mut registry = load_sandbox_snapshot_registry(agent).await?;
    if !registry.snapshots.iter().any(|snapshot| {
        snapshot.scope == scope
            && snapshot.owner_conversation_id == owner_conversation_id
            && snapshot.sandbox_id == sandbox_id
            && snapshot.snapshot_id == snapshot_id
    }) {
        registry.snapshots.push(SandboxSnapshotRecord {
            snapshot_id: snapshot_id.clone(),
            sandbox_id: sandbox_id.clone(),
            owner_conversation_id: owner_conversation_id.clone(),
            scope,
            created_at_ms: crate::scheduler_types::now_ms(),
        });
    }
    upsert_current_sandbox_snapshot(
        &mut registry,
        SandboxSnapshotCurrent {
            scope,
            owner_conversation_id,
            sandbox_id,
            snapshot_id,
        },
    );
    store_sandbox_snapshot_registry(agent, &registry).await
}

async fn record_current_sandbox_snapshot(
    agent: &dyn AgentHandle,
    scope: SandboxControlScope,
    owner_conversation_id: String,
    sandbox_id: String,
    snapshot_id: String,
) -> Result<()> {
    let mut registry = load_sandbox_snapshot_registry(agent).await?;
    upsert_current_sandbox_snapshot(
        &mut registry,
        SandboxSnapshotCurrent {
            scope,
            owner_conversation_id,
            sandbox_id,
            snapshot_id,
        },
    );
    store_sandbox_snapshot_registry(agent, &registry).await
}

fn upsert_current_sandbox_snapshot(
    registry: &mut SandboxSnapshotRegistry,
    current: SandboxSnapshotCurrent,
) {
    registry.current.retain(|existing| {
        !(existing.scope == current.scope
            && existing.owner_conversation_id == current.owner_conversation_id
            && existing.sandbox_id == current.sandbox_id)
    });
    registry.current.push(current);
}

async fn load_sandbox_snapshot_registry(
    agent: &dyn AgentHandle,
) -> Result<SandboxSnapshotRegistry> {
    let Some(artifact) =
        latest_agent_artifact(agent, SANDBOX_SNAPSHOT_REGISTRY_ARTIFACT_PATH).await?
    else {
        return Ok(SandboxSnapshotRegistry::default());
    };
    Ok(serde_json::from_slice(&artifact.contents)?)
}

async fn store_sandbox_snapshot_registry(
    agent: &dyn AgentHandle,
    registry: &SandboxSnapshotRegistry,
) -> Result<()> {
    agent
        .write_artifact(WriteArtifactRequest {
            path: SANDBOX_SNAPSHOT_REGISTRY_ARTIFACT_PATH.to_string(),
            contents: serde_json::to_vec_pretty(registry)?,
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

fn parse_snapshot_id(value: &str) -> Result<SnapshotId> {
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid snapshotId {value}: {error}"))
}

async fn execute_shell_tool(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<ShellToolArguments>(Value::Object(request.arguments.clone()))?;
    let program = config
        .shell_program
        .clone()
        .ok_or_else(|| anyhow::anyhow!("shell tool is not enabled for this conversation"))?;
    let sandbox_id = ensure_shell_sandbox(conversation, agent_config, config).await?;
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![program, "-lc".to_string(), args.command],
            env: Default::default(),
        })
        .await?;
    read_shell_process(process).await
}

async fn read_shell_process(process: Box<dyn SandboxProcess>) -> Result<ToolResult> {
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
    stdout_result?;
    stderr_result?;
    let exit_code = wait_result?;

    Ok(serde_json::to_value(ShellToolResult {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        exit_code,
    })?)
}

async fn execute_exo_shell_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    request: &ToolRequest,
) -> Result<ToolResult> {
    if effective_sandbox_scope(agent_config, config) == SandboxScope::Conversation {
        return execute_shell_tool(conversation, agent_config, config, request).await;
    }

    let args =
        serde_json::from_value::<ShellToolArguments>(Value::Object(request.arguments.clone()))?;
    let program = config
        .shell_program
        .clone()
        .ok_or_else(|| anyhow::anyhow!("shell tool is not enabled for this conversation"))?;
    let agent_sandbox = ensure_agent_sandbox(agent, agent_config).await?;
    let process = agent
        .run_in_sandbox(RunInSandboxRequest {
            id: agent_sandbox.sandbox_id,
            command: vec![program, "-lc".to_string(), args.command],
            env: Default::default(),
        })
        .await?;
    read_shell_process(process).await
}

pub(crate) async fn ensure_shell_sandbox(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> Result<String> {
    let desired_default_workdir = config
        .mounts
        .first()
        .map(|mount| mount.mount_path.clone())
        .or_else(|| {
            config
                .durable_file_systems
                .first()
                .map(|file_system| file_system.mount_path.clone())
        })
        .unwrap_or_else(|| "/".to_string());
    let desired_mounts = normalize_mounts(&config.mounts);
    let desired_durable_file_systems = config.durable_file_systems.clone();
    let desired_provider = config.effective_sandbox_provider(agent_config);
    // Empty means "unspecified"; the harness fills the provider's default.
    let requested_image = config.effective_sandbox_image(agent_config);
    let desired_image = requested_image.map(str::to_string).unwrap_or_default();
    let desired_enable_networking = agent_config.sandbox.enable_networking;

    if let Some(sandbox) = latest_shell_sandbox(conversation, desired_provider).await? {
        // When no image was requested, the stored sandbox holds the provider's
        // resolved default — don't treat that as a mismatch.
        let image_matches = requested_image.is_none_or(|img| sandbox.image == img);
        let config_matches = image_matches
            && sandbox.default_workdir == desired_default_workdir
            && sandbox.file_system_mounts == desired_mounts
            && sandbox.durable_file_systems == desired_durable_file_systems
            && sandbox.enable_networking == desired_enable_networking
            && sandbox.idle_seconds == 300;

        if config_matches {
            let Some(program) = &config.shell_program else {
                return Ok(sandbox.id);
            };

            let healthcheck = conversation
                .run_in_sandbox(RunInSandboxRequest {
                    id: sandbox.id.clone(),
                    command: vec![program.clone(), "-lc".to_string(), "true".to_string()],
                    env: Default::default(),
                })
                .await;
            if healthcheck.is_ok() {
                return Ok(sandbox.id);
            }
        }
    }

    conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: desired_provider,
            image: desired_image,
            default_workdir: Some(desired_default_workdir),
            file_system_mounts: Some(desired_mounts),
            durable_file_systems: Some(desired_durable_file_systems),
            enable_networking: Some(desired_enable_networking),
            idle_seconds: Some(300),
        })
        .await
}

fn normalize_mounts(mounts: &[FileSystemMount]) -> Vec<FileSystemMount> {
    mounts
        .iter()
        .map(|mount| FileSystemMount {
            host_path: mount.host_path.clone(),
            mount_path: mount.mount_path.clone(),
            mode: match mount.mode {
                FileSystemMountMode::ReadOnly => FileSystemMountMode::ReadOnly,
                FileSystemMountMode::ReadWrite => FileSystemMountMode::ReadWrite,
            },
            internal: Some(mount.internal.unwrap_or(false)),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellSandboxInfo {
    id: String,
    image: String,
    default_workdir: String,
    file_system_mounts: Vec<FileSystemMount>,
    durable_file_systems: Vec<exoharness::DurableFileSystem>,
    enable_networking: bool,
    idle_seconds: u64,
}

async fn latest_shell_sandbox(
    conversation: &dyn ConversationHandle,
    desired_provider: SandboxProvider,
) -> Result<Option<ShellSandboxInfo>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(50),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await?
        .events;

    let Some(event) = events.into_iter().next() else {
        return Ok(None);
    };
    match event.data {
        EventData::SandboxCreated {
            sandbox_id,
            provider,
            image,
            default_workdir,
            file_system_mounts,
            durable_file_systems,
            enable_networking,
            idle_seconds,
            ..
        } => {
            if provider != desired_provider {
                return Ok(None);
            }
            Ok(Some(ShellSandboxInfo {
                id: sandbox_id,
                image,
                default_workdir,
                file_system_mounts,
                durable_file_systems,
                enable_networking,
                idle_seconds,
            }))
        }
        other => Err(anyhow::anyhow!(
            "type-filtered query for {} returned unexpected variant {}",
            EventKind::SANDBOX_CREATED.as_str(),
            other.kind().as_str(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use exoharness::{BasicExoHarness, ExoHarness, NewAgentRequest, NewConversationRequest};
    use tempfile::TempDir;

    use super::*;
    use crate::test_support::local_test_config;

    #[tokio::test]
    async fn schedule_task_tool_uses_current_conversation_scope() {
        let tempdir = TempDir::new().unwrap();
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        let agent = exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap();
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap();
        let store = SchedulerStore::new(tempdir.path().join("scheduled-tasks"));

        let schedule_result = execute_schedule_task_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "schedule_sandbox_task",
                serde_json::json!({
                    "agentId": "spoofed-agent",
                    "conversationId": "spoofed-conversation",
                    "name": "check",
                    "schedule": "@every 1m",
                    "sandboxMode": null,
                    "setupCommand": null,
                    "command": ["true"],
                    "reportPrompt": "Report.",
                    "maxOutputBytes": 1024
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(schedule_result["ok"], true);

        let task_id = schedule_result["taskId"].as_str().unwrap();
        let task = store.get_task(task_id).await.unwrap().unwrap();
        assert_eq!(task.agent_id, agent.record().id.to_string());
        assert_eq!(task.conversation_id, conversation.record().id.to_string());

        let list_result = execute_list_scheduled_tasks_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "list_scheduled_tasks",
                serde_json::json!({
                    "agentId": "spoofed-agent",
                    "conversationId": "spoofed-conversation",
                    "includeDisabled": false
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(list_result["tasks"].as_array().unwrap().len(), 1);
    }

    fn tool_request(function_name: &str, arguments: serde_json::Value) -> ToolRequest {
        ToolRequest {
            function_name: function_name.to_string(),
            arguments: arguments.as_object().unwrap().clone(),
        }
    }
}

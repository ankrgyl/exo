use std::sync::Arc;

use anyhow::{Result, anyhow};
use exoharness::{RunInSandboxRequest, SandboxProcess, WriteArtifactRequest};
use futures::io::{AsyncRead, AsyncReadExt};
use serde::Serialize;

use crate::agent_sandbox::ensure_agent_sandbox;
use crate::conversation_sandbox::{create_conversation_sandbox, ensure_conversation_sandbox};
use crate::conversation_wakeup::send_conversation_wakeup;
use crate::scheduler_store::SchedulerStore;
use crate::scheduler_types::{
    DEFAULT_COMMAND_TIMEOUT_MS, DEFAULT_TASK_LEASE_MS, ScheduledTaskRecord, ScheduledTaskRunRecord,
    ScheduledTaskSandboxMode, now_ms,
};
use crate::{Harness, Uuid7};

#[derive(Debug, Clone, Copy)]
pub struct SchedulerRunOptions {
    pub limit: usize,
}

impl Default for SchedulerRunOptions {
    fn default() -> Self {
        Self { limit: 10 }
    }
}

#[derive(Debug, Serialize)]
struct ScheduledTaskArtifact {
    task_id: String,
    task_name: String,
    run_id: String,
    sandbox_id: Option<String>,
    setup_command: Option<Vec<String>>,
    command: Vec<String>,
    exit_code: Option<i32>,
    setup_stdout: Option<String>,
    setup_stderr: Option<String>,
    stdout: String,
    stderr: String,
    truncated: bool,
    error: Option<String>,
}

pub async fn run_due_tasks(
    harness: Arc<dyn Harness>,
    store: &SchedulerStore,
    options: SchedulerRunOptions,
) -> Result<Vec<ScheduledTaskRunRecord>> {
    let due = store
        .claim_due_tasks(now_ms(), options.limit, DEFAULT_TASK_LEASE_MS)
        .await?;
    futures::future::try_join_all(
        due.into_iter()
            .map(|task| run_task(Arc::clone(&harness), store, task)),
    )
    .await
}

pub async fn run_task(
    harness: Arc<dyn Harness>,
    store: &SchedulerStore,
    mut task: ScheduledTaskRecord,
) -> Result<ScheduledTaskRunRecord> {
    let started_at_ms = now_ms();
    let run_id = Uuid7::now().to_string();
    let run_result = run_task_inner(Arc::clone(&harness), &mut task, &run_id).await;
    let finished_at_ms = now_ms();

    let (mut run, result_artifact_id) = match run_result {
        Ok(output) => {
            let stdout_bytes = output.stdout.len() as u64;
            let stderr_bytes = output.stderr.len() as u64;
            (
                ScheduledTaskRunRecord {
                    id: run_id,
                    task_id: task.id.clone(),
                    started_at_ms,
                    finished_at_ms,
                    exit_code: output.exit_code,
                    stdout_bytes,
                    stderr_bytes,
                    truncated: output.truncated,
                    result_artifact_id: output.result_artifact_id.clone(),
                    error: output.error.clone(),
                },
                output.result_artifact_id,
            )
        }
        Err(error) => (
            ScheduledTaskRunRecord {
                id: run_id,
                task_id: task.id.clone(),
                started_at_ms,
                finished_at_ms,
                exit_code: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
                truncated: false,
                result_artifact_id: None,
                error: Some(error.to_string()),
            },
            None,
        ),
    };

    task.mark_completed(&run, result_artifact_id, finished_at_ms)?;
    if run
        .error
        .as_deref()
        .is_some_and(is_missing_task_owner_error)
    {
        task.enabled = false;
        task.updated_at_ms = finished_at_ms;
    }
    run.task_id = task.id.clone();
    store.put_run(&run).await?;
    store.put_task(&task).await?;
    Ok(run)
}

fn is_missing_task_owner_error(error: &str) -> bool {
    error.starts_with("scheduled task agent does not exist:")
        || error.starts_with("scheduled task conversation does not exist:")
}

struct TaskOutput {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    result_artifact_id: Option<String>,
    error: Option<String>,
}

async fn run_task_inner(
    harness: Arc<dyn Harness>,
    task: &mut ScheduledTaskRecord,
    run_id: &str,
) -> Result<TaskOutput> {
    let agent = harness
        .get_agent(&task.agent_id)
        .await?
        .ok_or_else(|| anyhow!("scheduled task agent does not exist: {}", task.agent_id))?;
    let conversation = agent
        .get_conversation(&task.conversation_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "scheduled task conversation does not exist: {}",
                task.conversation_id
            )
        })?;
    let agent_config = agent.config().await?;
    let conversation_config = conversation.config().await?;
    let conversation_handle = conversation.exoharness_handle();
    let agent_handle = agent.exoharness_handle();
    let sandbox = resolve_task_sandbox(
        task,
        agent_handle.as_ref(),
        std::sync::Arc::clone(&conversation_handle),
        &agent_config,
        &conversation_config,
    )
    .await?;
    let command_result: Result<CommandOutput> = async {
        let process = sandbox
            .run_in_sandbox(
                agent_handle.as_ref(),
                task.setup_command
                    .clone()
                    .unwrap_or_else(|| task.command.clone()),
            )
            .await?;
        let setup_output =
            read_process_output(process, task.max_output_bytes, DEFAULT_COMMAND_TIMEOUT_MS).await?;
        if task.setup_command.is_none() {
            return Ok(CommandOutput {
                sandbox_id: sandbox.sandbox_id().to_string(),
                setup: None,
                main: setup_output,
                error: None,
            });
        }
        if setup_output.exit_code != Some(0) {
            return Ok(CommandOutput {
                sandbox_id: sandbox.sandbox_id().to_string(),
                setup: Some(setup_output),
                main: ProcessOutput::empty(),
                error: Some("setup command exited non-zero".to_string()),
            });
        }
        let process = sandbox
            .run_in_sandbox(agent_handle.as_ref(), task.command.clone())
            .await?;
        let main_output =
            read_process_output(process, task.max_output_bytes, DEFAULT_COMMAND_TIMEOUT_MS).await?;
        Ok(CommandOutput {
            sandbox_id: sandbox.sandbox_id().to_string(),
            setup: Some(setup_output),
            main: main_output,
            error: None,
        })
    }
    .await;

    let (exit_code, stdout, stderr, truncated, error, setup, sandbox_id) = match command_result {
        Ok(output) => {
            let truncated =
                output.main.truncated || output.setup.as_ref().is_some_and(|setup| setup.truncated);
            (
                output.main.exit_code,
                output.main.stdout,
                output.main.stderr,
                truncated,
                output.error,
                output.setup,
                Some(output.sandbox_id),
            )
        }
        Err(error) => (
            None,
            Vec::new(),
            Vec::new(),
            false,
            Some(error.to_string()),
            None,
            None,
        ),
    };

    let artifact = ScheduledTaskArtifact {
        task_id: task.id.clone(),
        task_name: task.name.clone(),
        run_id: run_id.to_string(),
        sandbox_id,
        setup_command: task.setup_command.clone(),
        command: task.command.clone(),
        exit_code,
        setup_stdout: setup
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned()),
        setup_stderr: setup
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stderr).into_owned()),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated,
        error: error.clone(),
    };
    let artifact_version = conversation_handle
        .write_artifact(WriteArtifactRequest {
            path: format!("scheduled-tasks/{}/{run_id}.json", task.name),
            contents: serde_json::to_vec_pretty(&artifact)?,
        })
        .await?;
    let artifact_id = artifact_version.artifact_id.to_string();
    let prompt = if let Some(error) = &error {
        error_wakeup_prompt(task, run_id, error, &artifact_version)
    } else {
        wakeup_prompt(
            task,
            run_id,
            exit_code.expect("completed scheduled command has exit code"),
            truncated,
            &artifact_version,
            &stdout,
            &stderr,
        )
    };
    send_conversation_wakeup(conversation.as_ref(), prompt).await?;

    Ok(TaskOutput {
        exit_code,
        stdout,
        stderr,
        truncated,
        result_artifact_id: Some(artifact_id),
        error,
    })
}

async fn resolve_task_sandbox(
    task: &mut ScheduledTaskRecord,
    agent: &dyn exoharness::AgentHandle,
    conversation: std::sync::Arc<dyn exoharness::ConversationHandle>,
    agent_config: &crate::AgentConfig,
    conversation_config: &crate::ConversationConfig,
) -> Result<ResolvedTaskSandbox> {
    match task.sandbox_mode {
        ScheduledTaskSandboxMode::Agent => {
            let sandbox = ensure_agent_sandbox(agent, agent_config).await?;
            Ok(ResolvedTaskSandbox::Agent {
                sandbox_id: sandbox.sandbox_id,
            })
        }
        ScheduledTaskSandboxMode::Conversation => Ok(ResolvedTaskSandbox::Conversation {
            sandbox_id: ensure_conversation_sandbox(
                conversation.as_ref(),
                agent_config,
                conversation_config,
            )
            .await?,
            conversation,
        }),
        ScheduledTaskSandboxMode::TaskFresh => {
            if let Some(sandbox_id) = &task.task_sandbox_id {
                return Ok(ResolvedTaskSandbox::Conversation {
                    conversation,
                    sandbox_id: sandbox_id.clone(),
                });
            }
            let sandbox_id = create_conversation_sandbox(
                conversation.as_ref(),
                agent_config,
                conversation_config,
            )
            .await?;
            task.task_sandbox_id = Some(sandbox_id.clone());
            Ok(ResolvedTaskSandbox::Conversation {
                conversation,
                sandbox_id,
            })
        }
    }
}

enum ResolvedTaskSandbox {
    Agent {
        sandbox_id: String,
    },
    Conversation {
        conversation: std::sync::Arc<dyn exoharness::ConversationHandle>,
        sandbox_id: String,
    },
}

impl ResolvedTaskSandbox {
    fn sandbox_id(&self) -> &str {
        match self {
            Self::Agent { sandbox_id } | Self::Conversation { sandbox_id, .. } => sandbox_id,
        }
    }

    async fn run_in_sandbox(
        &self,
        agent: &dyn exoharness::AgentHandle,
        command: Vec<String>,
    ) -> Result<Box<dyn SandboxProcess>> {
        match self {
            Self::Agent { sandbox_id } => {
                agent
                    .run_in_sandbox(RunInSandboxRequest {
                        id: sandbox_id.clone(),
                        command,
                        env: Default::default(),
                    })
                    .await
            }
            Self::Conversation {
                conversation,
                sandbox_id,
            } => {
                conversation
                    .run_in_sandbox(RunInSandboxRequest {
                        id: sandbox_id.clone(),
                        command,
                        env: Default::default(),
                    })
                    .await
            }
        }
    }
}

async fn read_process_output(
    process: Box<dyn SandboxProcess>,
    max_stream_bytes: u64,
    timeout_ms: u64,
) -> Result<ProcessOutput> {
    let parts = process.into_parts();
    drop(parts.stdin);

    let read_output = async {
        let (stdout_result, stderr_result, exit_result) = tokio::join!(
            read_limited(parts.stdout, max_stream_bytes),
            read_limited(parts.stderr, max_stream_bytes),
            parts.wait,
        );
        let (stdout, stdout_truncated) = stdout_result?;
        let (stderr, stderr_truncated) = stderr_result?;
        let exit_code = exit_result?;
        Result::<_>::Ok((
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
            exit_code,
        ))
    };
    let (stdout, stdout_truncated, stderr, stderr_truncated, exit_code) =
        tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), read_output)
            .await
            .map_err(|_| anyhow!("scheduled task command timed out after {timeout_ms}ms"))??;
    let truncated = stdout_truncated || stderr_truncated;
    Ok(ProcessOutput {
        exit_code: Some(exit_code),
        stdout,
        stderr,
        truncated,
    })
}

struct CommandOutput {
    sandbox_id: String,
    setup: Option<ProcessOutput>,
    main: ProcessOutput,
    error: Option<String>,
}

#[derive(Debug)]
struct ProcessOutput {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

impl ProcessOutput {
    fn empty() -> Self {
        Self {
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
        }
    }
}

async fn read_limited<R>(mut reader: R, max_bytes: u64) -> Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len() as u64) as usize;
        if read > remaining {
            output.extend_from_slice(&buffer[..remaining]);
            truncated = true;
            continue;
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok((output, truncated))
}

fn wakeup_prompt(
    task: &ScheduledTaskRecord,
    run_id: &str,
    exit_code: i32,
    truncated: bool,
    artifact: &exoharness::ArtifactVersion,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    format!(
        "Scheduled task `{}` completed.\n\nRun id: `{}`\nExit code: {}\nResult artifact: `{}` version {} at `{}`\nOutput truncated: {}\n\nReport instructions:\n{}\n\nstdout preview:\n{}\n\nstderr preview:\n{}",
        task.name,
        run_id,
        exit_code,
        artifact.artifact_id,
        artifact.version,
        artifact.path,
        truncated,
        task.report_prompt,
        preview(stdout),
        preview(stderr),
    )
}

fn error_wakeup_prompt(
    task: &ScheduledTaskRecord,
    run_id: &str,
    error: &str,
    artifact: &exoharness::ArtifactVersion,
) -> String {
    format!(
        "Scheduled task `{}` failed.\n\nRun id: `{}`\nResult artifact: `{}` version {} at `{}`\n\nReport instructions:\n{}\n\nError:\n{}",
        task.name,
        run_id,
        artifact.artifact_id,
        artifact.version,
        artifact.path,
        task.report_prompt,
        error,
    )
}

fn preview(bytes: &[u8]) -> String {
    const PREVIEW_BYTES: usize = 4_000;
    let end = bytes.len().min(PREVIEW_BYTES);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{future::FutureExt, io::Cursor};

    struct HangingSandboxProcess;

    impl SandboxProcess for HangingSandboxProcess {
        fn into_parts(self: Box<Self>) -> exoharness::SandboxProcessParts {
            exoharness::SandboxProcessParts {
                stdout: Box::pin(Cursor::new(Vec::new())),
                stderr: Box::pin(Cursor::new(Vec::new())),
                stdin: Box::pin(Cursor::new(Vec::new())),
                wait: async {
                    futures::future::pending::<()>().await;
                    Ok(0)
                }
                .boxed(),
            }
        }
    }

    #[tokio::test]
    async fn read_process_output_times_out() {
        let error = read_process_output(Box::new(HangingSandboxProcess), 1024, 1)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }
}

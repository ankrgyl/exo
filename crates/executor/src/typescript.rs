use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context as AnyhowContext, anyhow, bail};
use async_trait::async_trait;
use exoharness::{
    AddEventsRequest, AgentHandle, AgentId, BasicExoHarness, BasicExoHarnessConfig,
    CancelSandboxProcessRequest, CloseSandboxProcessInputRequest, ConversationHandle,
    ConversationId, EventData, EventKind, EventQuery, EventQueryDirection, ExoHarness,
    GetSandboxProcessEventsResult, Result, SandboxId, SandboxProcessEvent,
    SandboxProcessEventQuery, SandboxProcessId, SandboxProcessLifecycle, SandboxProcessMode,
    SandboxProcessStatus, SandboxProcessStdin, StartSandboxProcessRequest, ToolArguments,
    ToolRequest, ToolResult, TurnHandle, WriteSandboxProcessInputRequest,
    protocol::{
        ConversationHandleInfo, Request as ExoRequest, Response as ExoResponse, TurnHandleInfo,
    },
    server::ExoHarnessServer,
};
use lingua::UniversalStreamChunk;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter, Lines};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{Mutex, MutexGuard, mpsc};
use tokio::task::JoinHandle;

use crate::execution_tracing::TurnExecutionTrace;
use crate::harness_executor::{ExecutorHarnessRuntime, ExecutorStreamMode, HarnessExecutor};
use crate::harness_facade::{SharedHarness, SharedHarnessBacked};
use crate::harness_tool::{BasicToolRuntime, ExoToolRuntime, ensure_shell_sandbox};
use crate::shared::try_send_stream_event;
use crate::{
    AgentConfig, BraintrustRuntimeConfig, ConversationConfig, ExecutionStreamEvent, SendRequest,
    ToolRuntime,
};

pub struct TypeScriptExecutor<T> {
    root: Arc<dyn ExoHarness>,
    workspace_root: PathBuf,
    env: Arc<HashMap<String, String>>,
    tools: Arc<T>,
    runners: Arc<StdMutex<HashMap<String, Arc<TypeScriptRunner>>>>,
}

impl<T> TypeScriptExecutor<T> {
    pub fn new(
        root: Arc<dyn ExoHarness>,
        workspace_root: PathBuf,
        env: HashMap<String, String>,
        tools: Arc<T>,
    ) -> Self {
        Self {
            root,
            workspace_root,
            env: Arc::new(env),
            tools,
            runners: Arc::new(StdMutex::new(HashMap::new())),
        }
    }
}

impl<T> Clone for TypeScriptExecutor<T> {
    fn clone(&self) -> Self {
        Self {
            root: Arc::clone(&self.root),
            workspace_root: self.workspace_root.clone(),
            env: Arc::clone(&self.env),
            tools: Arc::clone(&self.tools),
            runners: Arc::clone(&self.runners),
        }
    }
}

#[async_trait]
impl<T> HarnessExecutor for TypeScriptExecutor<T>
where
    T: ToolRuntime + 'static,
{
    type Prepared = SendRequest;

    async fn prepare_conversation(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
    ) -> Result<()> {
        self.tools
            .prepare_conversation(agent, conversation, agent_config, conversation_config)
            .await
    }

    fn prepare_request(&self, request: &SendRequest) -> Result<Self::Prepared> {
        Ok(request.clone())
    }

    async fn execute_turn(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        prepared: &Self::Prepared,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        let module_path = agent_config
            .typescript
            .as_ref()
            .map(|config| config.module_path.clone())
            .ok_or_else(|| anyhow!("typescript harness requires agent.typescript.module_path"))?;
        if !Path::new(&module_path).is_file() {
            bail!("typescript harness module does not exist: {module_path}");
        }

        loop {
            let runner = self.runner(&module_path)?;
            let process = runner.process.lock().await;
            if !runner.valid.load(Ordering::Acquire) {
                drop(process);
                continue;
            }
            let mut run = TypeScriptRunnerRun::new(
                process,
                runner.as_ref(),
                self.runners.as_ref(),
                &module_path,
            );
            let result = run
                .execute_turn(
                    self,
                    TypeScriptTurn {
                        agent,
                        conversation,
                        turn,
                        agent_config,
                        conversation_config,
                        prepared,
                        stream_mode,
                        turn_trace,
                    },
                )
                .await;
            run.reusable = result.is_ok();
            drop(run);

            return result;
        }
    }
}

impl<T> TypeScriptExecutor<T>
where
    T: ToolRuntime + 'static,
{
    fn runner(&self, module_path: &str) -> Result<Arc<TypeScriptRunner>> {
        let mut runners = self
            .runners
            .lock()
            .expect("typescript runner cache poisoned");
        if let Some(runner) = runners
            .get(module_path)
            .filter(|runner| runner.valid.load(Ordering::Acquire))
        {
            return Ok(Arc::clone(runner));
        }

        let runner = Arc::new(TypeScriptRunner {
            process: Mutex::new(TypeScriptRunnerProcess::start(
                &self.workspace_root,
                self.env.as_ref(),
                module_path,
            )?),
            valid: AtomicBool::new(true),
        });
        runners.insert(module_path.to_string(), Arc::clone(&runner));
        Ok(runner)
    }

    async fn execute_runtime_request(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        turn: Arc<dyn TurnHandle>,
        request: RuntimeRequest,
    ) -> Result<RuntimeResponsePayload> {
        match request {
            RuntimeRequest::ExecuteTool { request } => Ok(RuntimeResponsePayload::ToolResult {
                result: self
                    .tools
                    .execute(
                        agent,
                        conversation,
                        Some(turn.as_ref()),
                        agent_config,
                        conversation_config,
                        &request,
                    )
                    .await?,
            }),
            RuntimeRequest::StartSandboxProcess { .. }
            | RuntimeRequest::WriteSandboxProcessStdin { .. }
            | RuntimeRequest::CloseSandboxProcessStdin { .. }
            | RuntimeRequest::CloseSandboxProcess { .. } => {
                bail!("sandbox process requests are handled by the TypeScript runner")
            }
        }
    }
}

const RUNNER_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct TypeScriptRunner {
    process: Mutex<TypeScriptRunnerProcess>,
    valid: AtomicBool,
}

struct TypeScriptRunnerRun<'a> {
    process: MutexGuard<'a, TypeScriptRunnerProcess>,
    runner: &'a TypeScriptRunner,
    runners: &'a StdMutex<HashMap<String, Arc<TypeScriptRunner>>>,
    module_path: &'a str,
    reusable: bool,
}

impl<'a> TypeScriptRunnerRun<'a> {
    fn new(
        process: MutexGuard<'a, TypeScriptRunnerProcess>,
        runner: &'a TypeScriptRunner,
        runners: &'a StdMutex<HashMap<String, Arc<TypeScriptRunner>>>,
        module_path: &'a str,
    ) -> Self {
        Self {
            process,
            runner,
            runners,
            module_path,
            reusable: false,
        }
    }
}

impl std::ops::Deref for TypeScriptRunnerRun<'_> {
    type Target = TypeScriptRunnerProcess;

    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

impl std::ops::DerefMut for TypeScriptRunnerRun<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}

impl Drop for TypeScriptRunnerRun<'_> {
    fn drop(&mut self) {
        if !self.reusable {
            self.runner.valid.store(false, Ordering::Release);
            if let Err(error) = self.process.child.start_kill() {
                tracing::warn!(%error, "failed to terminate unusable TypeScript harness runner");
            }
            match self.runners.lock() {
                Ok(mut runners) => {
                    if let Some(current) = runners.get(self.module_path)
                        && std::ptr::eq(current.as_ref(), self.runner)
                    {
                        runners.remove(self.module_path);
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "failed to remove unusable TypeScript harness runner");
                }
            }
        }
    }
}

struct TypeScriptRunnerProcess {
    child: Child,
    host_tx: mpsc::UnboundedSender<HostToGuestMessage>,
    lines: Lines<BufReader<ChildStdout>>,
    stderr_task: Option<JoinHandle<std::io::Result<String>>>,
    _writer_task: JoinHandle<anyhow::Result<()>>,
    next_sandbox_process_id: u64,
    sandbox_processes: HashMap<u64, RunningSandboxProcess>,
}

struct RunningSandboxProcess {
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
    event_task: JoinHandle<()>,
}

const TYPESCRIPT_SANDBOX_PROCESS_REUSE_EVENT: &str = "typescript_sandbox_process_reuse";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TypeScriptSandboxProcessReuseEvent {
    reuse_key: String,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
}

struct TypeScriptTurn<'a> {
    agent: &'a dyn AgentHandle,
    conversation: &'a dyn ConversationHandle,
    turn: Arc<dyn TurnHandle>,
    agent_config: &'a AgentConfig,
    conversation_config: &'a ConversationConfig,
    prepared: &'a SendRequest,
    stream_mode: ExecutorStreamMode<'a>,
    turn_trace: Option<&'a dyn TurnExecutionTrace>,
}

impl TypeScriptRunnerProcess {
    fn start(
        workspace_root: &Path,
        env: &HashMap<String, String>,
        module_path: &str,
    ) -> Result<Self> {
        let runner_path = workspace_root
            .join("exoharness")
            .join("typescript")
            .join("harness")
            .join("runner.ts");
        if !runner_path.is_file() {
            bail!(
                "typescript harness runner does not exist: {}",
                runner_path.display()
            );
        }

        let mut child = Command::new("node")
            .arg("--import")
            .arg("tsx")
            .arg(&runner_path)
            .arg(module_path)
            .current_dir(workspace_root)
            .envs(env.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("failed to start TypeScript harness runner for {module_path}")
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("typescript harness runner did not expose stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("typescript harness runner did not expose stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("typescript harness runner did not expose stderr"))?;

        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        });

        let (host_tx, mut host_rx) = mpsc::unbounded_channel::<HostToGuestMessage>();
        let writer_task = tokio::spawn(async move {
            let mut stdin = BufWriter::new(stdin);
            while let Some(message) = host_rx.recv().await {
                write_protocol_message(&mut stdin, &message).await?;
            }
            Ok(())
        });

        Ok(Self {
            child,
            host_tx,
            lines: BufReader::new(stdout).lines(),
            stderr_task: Some(stderr_task),
            _writer_task: writer_task,
            next_sandbox_process_id: 1,
            sandbox_processes: HashMap::new(),
        })
    }

    async fn execute_turn<T>(
        &mut self,
        executor: &TypeScriptExecutor<T>,
        turn: TypeScriptTurn<'_>,
    ) -> Result<()>
    where
        T: ToolRuntime + 'static,
    {
        let TypeScriptTurn {
            agent,
            conversation,
            turn,
            agent_config,
            conversation_config,
            prepared,
            stream_mode,
            turn_trace,
        } = turn;
        let exoharness_server = ExoHarnessServer::new(Arc::clone(&executor.root));
        let conversation_info = ConversationHandleInfo {
            agent_id: agent.record().id,
            record: conversation.record().clone(),
        };
        let turn_info = TurnHandleInfo {
            conversation: conversation_info.clone(),
            record: turn.record().clone(),
        };
        send_host_message(
            &self.host_tx,
            HostToGuestMessage::Init {
                payload: Box::new(TypeScriptInitPayload {
                    agent: agent.record().clone(),
                    conversation: conversation_info,
                    turn: turn_info,
                    agent_config: agent_config.clone(),
                    conversation_config: conversation_config.clone(),
                    request: prepared.clone(),
                    streaming: matches!(stream_mode, ExecutorStreamMode::Enabled(_)),
                    braintrust_parent: turn_trace.and_then(TurnExecutionTrace::export_parent),
                }),
            },
        )?;

        loop {
            tokio::select! {
                biased;

                line = self.lines.next_line() => {
                    let Some(line) = line? else {
                        let status = self.child.wait().await?;
                        return Err(self.exited_error(status).await);
                    };
                    let message = match serde_json::from_str::<GuestToHostMessage>(&line) {
                        Ok(message) => message,
                        Err(error) => {
                            // malformed guest message (e.g. a tool that passes bad field)
                            // Reject explicitly so failure is clean, then keep serving the loop.
                            self.reject_undecodable_message(&line, &error)?;
                            continue;
                        }
                    };
                    match message {
                        GuestToHostMessage::RuntimeRequest { id, request } => {
                            let request_kind = request.kind();
                            let response = self
                                .execute_runtime_request(
                                    executor,
                                    agent,
                                    conversation,
                                    agent_config,
                                    conversation_config,
                                    Arc::clone(&turn),
                                    request,
                                )
                                .await;
                            let response = match response {
                                Ok(payload) => HostToGuestMessage::RuntimeResponse {
                                    id,
                                    ok: true,
                                    payload: Some(payload),
                                    error: None,
                                },
                                Err(error) => HostToGuestMessage::RuntimeResponse {
                                    id,
                                    ok: false,
                                    payload: None,
                                    error: Some(format_error_chain(
                                        &error,
                                        format_args!("typescript runtime request `{request_kind}` failed"),
                                    )),
                                },
                            };
                            send_host_message(&self.host_tx, response)?;
                        }
                        GuestToHostMessage::ExoRequest { id, request } => {
                            let request_kind = request.kind();
                            let response = match exoharness_server.handle_request(request).await {
                                Ok(response) => HostToGuestMessage::ExoResponse {
                                    id,
                                    ok: true,
                                    response: Box::new(Some(response)),
                                    error: None,
                                },
                                Err(error) => HostToGuestMessage::ExoResponse {
                                    id,
                                    ok: false,
                                    response: Box::new(None),
                                    error: Some(format_error_chain(
                                        &error,
                                        format_args!(
                                            "typescript exoharness request `{request_kind}` failed"
                                        ),
                                    )),
                                },
                            };
                            send_host_message(&self.host_tx, response)?;
                        }
                        GuestToHostMessage::StreamEvent { event } => {
                            if let ExecutorStreamMode::Enabled(event_tx) = stream_mode {
                                try_send_stream_event(event_tx, to_execution_stream_event(event));
                            }
                        }
                        GuestToHostMessage::Done => return Ok(()),
                        GuestToHostMessage::Error { message, stack } => {
                            let stack_suffix = stack
                                .as_deref()
                                .map(|stack| format!("\n{stack}"))
                                .unwrap_or_default();
                            bail!("typescript harness failed: {message}{stack_suffix}");
                        }
                    }
                }
                () = tokio::time::sleep(RUNNER_EXIT_POLL_INTERVAL) => {
                    if let Some(status) = self.child.try_wait()? {
                        return Err(self.exited_error(status).await);
                    }
                }
            }
        }
    }

    async fn execute_runtime_request<T>(
        &mut self,
        executor: &TypeScriptExecutor<T>,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        turn: Arc<dyn TurnHandle>,
        request: RuntimeRequest,
    ) -> Result<RuntimeResponsePayload>
    where
        T: ToolRuntime + 'static,
    {
        match request {
            RuntimeRequest::ExecuteTool { request } => {
                executor
                    .execute_runtime_request(
                        agent,
                        conversation,
                        agent_config,
                        conversation_config,
                        Arc::clone(&turn),
                        RuntimeRequest::ExecuteTool { request },
                    )
                    .await
            }
            RuntimeRequest::StartSandboxProcess {
                command,
                env,
                reuse_key,
            } => {
                self.start_sandbox_process(
                    executor,
                    agent,
                    conversation,
                    agent_config,
                    conversation_config,
                    command,
                    env,
                    reuse_key,
                )
                .await
            }
            RuntimeRequest::WriteSandboxProcessStdin { process_id, data } => {
                let process = self
                    .sandbox_processes
                    .get(&process_id)
                    .ok_or_else(|| anyhow!("sandbox process is not active: {process_id}"))?;
                conversation
                    .write_sandbox_process_input(WriteSandboxProcessInputRequest {
                        sandbox_id: process.sandbox_id.clone(),
                        process_id: process.process_id.clone(),
                        data: data.into_bytes(),
                    })
                    .await?;
                Ok(RuntimeResponsePayload::Unit)
            }
            RuntimeRequest::CloseSandboxProcessStdin { process_id } => {
                let process = self
                    .sandbox_processes
                    .get(&process_id)
                    .ok_or_else(|| anyhow!("sandbox process is not active: {process_id}"))?;
                conversation
                    .close_sandbox_process_input(CloseSandboxProcessInputRequest {
                        sandbox_id: process.sandbox_id.clone(),
                        process_id: process.process_id.clone(),
                    })
                    .await?;
                Ok(RuntimeResponsePayload::Unit)
            }
            RuntimeRequest::CloseSandboxProcess { process_id } => {
                if let Some(process) = self.sandbox_processes.remove(&process_id) {
                    process.event_task.abort();
                    conversation
                        .cancel_sandbox_process(CancelSandboxProcessRequest {
                            sandbox_id: process.sandbox_id,
                            process_id: process.process_id,
                            signal: None,
                        })
                        .await?;
                    send_host_message(
                        &self.host_tx,
                        HostToGuestMessage::RuntimeEvent {
                            event: RuntimeEvent::Exit {
                                process_id,
                                exit_code: None,
                            },
                        },
                    )?;
                }
                Ok(RuntimeResponsePayload::Unit)
            }
        }
    }

    async fn start_sandbox_process<T>(
        &mut self,
        executor: &TypeScriptExecutor<T>,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        command: Vec<String>,
        env: HashMap<String, String>,
        reuse_key: Option<String>,
    ) -> Result<RuntimeResponsePayload>
    where
        T: ToolRuntime + 'static,
    {
        let sandbox_id =
            ensure_shell_sandbox(conversation, agent_config, conversation_config).await?;
        let reusable_process = match reuse_key.as_deref() {
            Some(reuse_key) => {
                reusable_sandbox_process(conversation, reuse_key, &sandbox_id).await?
            }
            None => None,
        };
        let (sandbox_process_id, reused, cursor) = match reusable_process {
            Some(process) => process,
            None => {
                let process = conversation
                    .start_sandbox_process(StartSandboxProcessRequest {
                        sandbox_id: sandbox_id.clone(),
                        name: None,
                        command,
                        env,
                        cwd: None,
                        mode: SandboxProcessMode::Exec,
                        stdin: SandboxProcessStdin::Open,
                        output: Default::default(),
                        lifecycle: SandboxProcessLifecycle::Attached,
                    })
                    .await?;
                if let Some(reuse_key) = reuse_key {
                    conversation
                        .add_events(AddEventsRequest {
                            session_id: None,
                            turn_id: None,
                            data: vec![EventData::Custom {
                                event_type: TYPESCRIPT_SANDBOX_PROCESS_REUSE_EVENT.to_string(),
                                payload: serde_json::to_value(
                                    TypeScriptSandboxProcessReuseEvent {
                                        reuse_key,
                                        sandbox_id: process.sandbox_id.clone(),
                                        process_id: process.id.clone(),
                                    },
                                )?,
                            }],
                        })
                        .await?;
                }
                (process.id, false, None)
            }
        };
        let process_id = self.next_sandbox_process_id;
        self.next_sandbox_process_id += 1;
        let process_conversation = executor
            .conversation_handle(agent.record().id, conversation.record().id)
            .await?;
        let event_task = spawn_sandbox_process_event_task(
            self.host_tx.clone(),
            process_conversation,
            process_id,
            sandbox_id.clone(),
            sandbox_process_id.clone(),
            cursor,
        );

        self.sandbox_processes.insert(
            process_id,
            RunningSandboxProcess {
                sandbox_id: sandbox_id.clone(),
                process_id: sandbox_process_id.clone(),
                event_task,
            },
        );

        Ok(RuntimeResponsePayload::SandboxProcessStarted {
            process_id,
            sandbox_id,
            sandbox_process_id,
            reused,
        })
    }

    /// Handles a guest line that failed to decode into a `GuestToHostMessage`.
    ///
    /// If the line is recognizably a request with an `id`, we reply with an
    /// error response so the guest's pending call rejects and the turn keeps
    /// running. Otherwise there is nothing to reject, so we log and drop it.
    fn reject_undecodable_message(&self, line: &str, error: &serde_json::Error) -> Result<()> {
        let detail =
            format!("invalid TypeScript harness protocol message: {line}\ncaused by: {error}");
        match undecodable_message_response(line, detail.clone()) {
            Some(response) => {
                tracing::warn!("{detail}");
                send_host_message(&self.host_tx, response)
            }
            None => {
                // No id (or an unrecognized kind): the guest has no pending call
                // keyed to this line, so drop it instead of aborting the turn.
                tracing::warn!("ignoring undecodable guest message: {detail}");
                Ok(())
            }
        }
    }

    async fn exited_error(&mut self, status: ExitStatus) -> anyhow::Error {
        let stderr_output = self.take_stderr_output().await;
        let stderr_suffix = if stderr_output.trim().is_empty() {
            String::new()
        } else {
            format!("\nstderr:\n{}", stderr_output.trim())
        };
        anyhow!(
            "typescript harness runner exited with status {}{}",
            status,
            stderr_suffix
        )
    }

    async fn take_stderr_output(&mut self) -> String {
        let Some(stderr_task) = self.stderr_task.take() else {
            return String::new();
        };
        match stderr_task.await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => format!("failed to read TypeScript harness stderr: {error}"),
            Err(error) => format!("TypeScript harness stderr task panicked: {error}"),
        }
    }
}

async fn reusable_sandbox_process(
    conversation: &dyn ConversationHandle,
    reuse_key: &str,
    desired_sandbox_id: &SandboxId,
) -> Result<Option<(SandboxProcessId, bool, Option<u64>)>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(100),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::custom(
                TYPESCRIPT_SANDBOX_PROCESS_REUSE_EVENT,
            )]),
        }))
        .await?
        .events;

    for event in events {
        let EventData::Custom {
            event_type,
            payload,
        } = event.data
        else {
            continue;
        };
        if event_type != TYPESCRIPT_SANDBOX_PROCESS_REUSE_EVENT {
            continue;
        }
        let candidate: TypeScriptSandboxProcessReuseEvent = serde_json::from_value(payload)?;
        if candidate.reuse_key != reuse_key || candidate.sandbox_id != *desired_sandbox_id {
            continue;
        }
        let status = latest_sandbox_process_event_cursor(
            conversation,
            candidate.sandbox_id.clone(),
            candidate.process_id.clone(),
        )
        .await;
        let Ok(status) = status else {
            continue;
        };
        if status.status.is_running() {
            return Ok(Some((candidate.process_id, true, status.cursor)));
        }
    }

    Ok(None)
}

async fn latest_sandbox_process_event_cursor(
    conversation: &dyn ConversationHandle,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
) -> Result<GetLatestSandboxProcessCursorResult> {
    latest_sandbox_process_event_cursor_from_fetch(|after| {
        let sandbox_id = sandbox_id.clone();
        let process_id = process_id.clone();
        async move {
            conversation
                .get_sandbox_process_events(SandboxProcessEventQuery {
                    sandbox_id,
                    process_id,
                    after,
                    limit: Some(1000),
                    follow: Some(false),
                })
                .await
        }
    })
    .await
}

async fn latest_sandbox_process_event_cursor_from_fetch<F, Fut>(
    mut fetch_page: F,
) -> Result<GetLatestSandboxProcessCursorResult>
where
    F: FnMut(Option<u64>) -> Fut,
    Fut: Future<Output = Result<GetSandboxProcessEventsResult>>,
{
    let mut after = None;
    loop {
        let previous_after = after;
        let page = fetch_page(after).await?;
        let event_count = page.events.len();
        after = page.cursor.or(after);
        if !page.status.is_running() || event_count < 1000 {
            return Ok(GetLatestSandboxProcessCursorResult {
                cursor: after,
                status: page.status,
            });
        }
        if after == previous_after {
            bail!("sandbox process event pagination did not advance");
        }
    }
}

struct GetLatestSandboxProcessCursorResult {
    cursor: Option<u64>,
    status: SandboxProcessStatus,
}

impl<T> TypeScriptExecutor<T>
where
    T: ToolRuntime + 'static,
{
    async fn conversation_handle(
        &self,
        agent_id: AgentId,
        conversation_id: ConversationId,
    ) -> Result<Arc<dyn ConversationHandle>> {
        let agent = self
            .root
            .get_agent(&agent_id)
            .await?
            .ok_or_else(|| anyhow!("agent disappeared while running TypeScript harness"))?;
        agent
            .get_conversation(&conversation_id)
            .await?
            .ok_or_else(|| anyhow!("conversation disappeared while running TypeScript harness"))
    }
}

pub struct TypeScriptHarness<T> {
    inner: SharedHarness<ExecutorHarnessRuntime<TypeScriptExecutor<T>>>,
}

impl<T> TypeScriptHarness<T> {
    pub fn new(exoharness: Arc<dyn ExoHarness>, workspace_root: PathBuf, tools: Arc<T>) -> Self
    where
        T: ToolRuntime + 'static,
    {
        let runtime = ExecutorHarnessRuntime::new(
            TypeScriptExecutor::new(
                Arc::clone(&exoharness),
                workspace_root,
                HashMap::new(),
                tools,
            ),
            None,
        );
        Self {
            inner: SharedHarness::new(exoharness, runtime),
        }
    }
}

impl TypeScriptHarness<BasicToolRuntime> {
    pub fn from_exoharness(
        exoharness: Arc<dyn ExoHarness>,
        runtime_config: Option<BraintrustRuntimeConfig>,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        let workspace_root = std::env::current_dir()
            .context("failed to resolve current directory for TypeScript harness")?;
        let tools = Arc::new(BasicToolRuntime);
        let runtime = ExecutorHarnessRuntime::new(
            TypeScriptExecutor::new(Arc::clone(&exoharness), workspace_root, env, tools),
            runtime_config,
        );
        Ok(Self {
            inner: SharedHarness::new(exoharness, runtime),
        })
    }

    pub async fn from_config(
        exo_config: BasicExoHarnessConfig,
        runtime_config: Option<BraintrustRuntimeConfig>,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        Self::from_exoharness(
            Arc::new(BasicExoHarness::new(exo_config).await?),
            runtime_config,
            env,
        )
    }
}

impl TypeScriptHarness<ExoToolRuntime> {
    pub async fn exo_from_root(
        root: impl AsRef<Path>,
        exo_config: BasicExoHarnessConfig,
        runtime_config: Option<BraintrustRuntimeConfig>,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        let workspace_root = std::env::current_dir()
            .context("failed to resolve current directory for Exo harness")?;
        let root = root.as_ref();
        let exoharness: Arc<dyn ExoHarness> = Arc::new(BasicExoHarness::new(exo_config).await?);
        let adapter_worker_root = workspace_root.join("exo/adapters");
        let tools = Arc::new(ExoToolRuntime::with_roots(
            root.join("scheduled-tasks"),
            root.join("adapters"),
            adapter_worker_root,
        ));
        let runtime = ExecutorHarnessRuntime::new(
            TypeScriptExecutor::new(Arc::clone(&exoharness), workspace_root, env, tools),
            runtime_config,
        );
        Ok(Self {
            inner: SharedHarness::new(exoharness, runtime),
        })
    }
}

impl<T> SharedHarnessBacked for TypeScriptHarness<T>
where
    T: ToolRuntime + 'static,
{
    type Runtime = ExecutorHarnessRuntime<TypeScriptExecutor<T>>;

    fn shared_harness(&self) -> &SharedHarness<Self::Runtime> {
        &self.inner
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HostToGuestMessage {
    Init {
        payload: Box<TypeScriptInitPayload>,
    },
    Shutdown,
    RuntimeResponse {
        id: u64,
        ok: bool,
        payload: Option<RuntimeResponsePayload>,
        error: Option<String>,
    },
    ExoResponse {
        id: u64,
        ok: bool,
        response: Box<Option<ExoResponse>>,
        error: Option<String>,
    },
    RuntimeEvent {
        event: RuntimeEvent,
    },
}

/// A lenient view of a guest line used only to recover the `kind`/`id` when the
/// full `GuestToHostMessage` fails to decode, so we can reject the right pending
/// request instead of tearing down the turn.
#[derive(Debug, Deserialize)]
struct GuestMessagePreview {
    kind: GuestMessageKind,
    #[serde(default)]
    id: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GuestMessageKind {
    RuntimeRequest,
    ExoRequest,
    #[serde(other)]
    Other,
}

/// Decides how to reply to a guest line that failed to decode into a
/// `GuestToHostMessage`. Returns an error response keyed to the pending request
/// when the line's `kind`/`id` can be recovered, or `None` when the line can't
/// be tied to a pending call (so the caller drops it rather than aborting).
fn undecodable_message_response(line: &str, detail: String) -> Option<HostToGuestMessage> {
    match serde_json::from_str::<GuestMessagePreview>(line).ok()? {
        GuestMessagePreview {
            kind: GuestMessageKind::RuntimeRequest,
            id: Some(id),
        } => Some(HostToGuestMessage::RuntimeResponse {
            id,
            ok: false,
            payload: None,
            error: Some(detail),
        }),
        GuestMessagePreview {
            kind: GuestMessageKind::ExoRequest,
            id: Some(id),
        } => Some(HostToGuestMessage::ExoResponse {
            id,
            ok: false,
            response: Box::new(None),
            error: Some(detail),
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GuestToHostMessage {
    RuntimeRequest {
        id: u64,
        request: RuntimeRequest,
    },
    ExoRequest {
        id: u64,
        request: ExoRequest,
    },
    StreamEvent {
        event: TypeScriptStreamEvent,
    },
    Done,
    Error {
        message: String,
        stack: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TypeScriptInitPayload {
    agent: exoharness::AgentRecord,
    conversation: ConversationHandleInfo,
    turn: TurnHandleInfo,
    agent_config: AgentConfig,
    conversation_config: ConversationConfig,
    request: SendRequest,
    streaming: bool,
    braintrust_parent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RuntimeRequest {
    ExecuteTool {
        request: ToolRequest,
    },
    StartSandboxProcess {
        command: Vec<String>,
        env: HashMap<String, String>,
        reuse_key: Option<String>,
    },
    WriteSandboxProcessStdin {
        process_id: u64,
        data: String,
    },
    CloseSandboxProcessStdin {
        process_id: u64,
    },
    CloseSandboxProcess {
        process_id: u64,
    },
}

impl RuntimeRequest {
    fn kind(&self) -> &'static str {
        match self {
            Self::ExecuteTool { .. } => "execute_tool",
            Self::StartSandboxProcess { .. } => "start_sandbox_process",
            Self::WriteSandboxProcessStdin { .. } => "write_sandbox_process_stdin",
            Self::CloseSandboxProcessStdin { .. } => "close_sandbox_process_stdin",
            Self::CloseSandboxProcess { .. } => "close_sandbox_process",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RuntimeResponsePayload {
    ToolResult {
        result: ToolResult,
    },
    SandboxProcessStarted {
        process_id: u64,
        sandbox_id: SandboxId,
        sandbox_process_id: SandboxProcessId,
        reused: bool,
    },
    Unit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SandboxProcessStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum RuntimeEvent {
    #[serde(rename = "sandbox_process_output")]
    Output {
        process_id: u64,
        stream: SandboxProcessStream,
        data: String,
    },
    #[serde(rename = "sandbox_process_exit")]
    Exit {
        process_id: u64,
        exit_code: Option<i32>,
    },
    #[serde(rename = "sandbox_process_error")]
    Error { process_id: u64, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TypeScriptStreamEvent {
    FirstChunk {
        ttft_ms: u64,
    },
    TextDelta {
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        tool_name: String,
        arguments: ToolArguments,
    },
    ToolResult {
        tool_call_id: String,
        result: ToolResult,
    },
}

fn to_execution_stream_event(event: TypeScriptStreamEvent) -> ExecutionStreamEvent {
    match event {
        TypeScriptStreamEvent::FirstChunk { ttft_ms } => ExecutionStreamEvent::FirstChunk {
            ttft: Duration::from_millis(ttft_ms),
        },
        TypeScriptStreamEvent::TextDelta { text } => {
            ExecutionStreamEvent::Chunk(UniversalStreamChunk::text_delta(0, &text))
        }
        TypeScriptStreamEvent::ToolCall {
            tool_call_id,
            tool_name,
            arguments,
        } => ExecutionStreamEvent::ToolCall {
            tool_call_id,
            tool_name,
            arguments,
        },
        TypeScriptStreamEvent::ToolResult {
            tool_call_id,
            result,
        } => ExecutionStreamEvent::ToolResult {
            tool_call_id,
            result,
        },
    }
}

fn spawn_sandbox_process_event_task(
    sender: mpsc::UnboundedSender<HostToGuestMessage>,
    conversation: Arc<dyn ConversationHandle>,
    process_id: u64,
    sandbox_id: SandboxId,
    sandbox_process_id: SandboxProcessId,
    mut cursor: Option<u64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = conversation
                .get_sandbox_process_events(SandboxProcessEventQuery {
                    sandbox_id: sandbox_id.clone(),
                    process_id: sandbox_process_id.clone(),
                    after: cursor,
                    limit: Some(100),
                    follow: Some(true),
                })
                .await;

            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    if send_host_message(
                        &sender,
                        HostToGuestMessage::RuntimeEvent {
                            event: RuntimeEvent::Error {
                                process_id,
                                message: error.to_string(),
                            },
                        },
                    )
                    .is_err()
                    {
                        return;
                    }
                    return;
                }
            };

            let mut emitted_terminal = false;
            for event in result.events {
                cursor = Some(event.cursor());
                let runtime_event = sandbox_process_event_to_runtime_event(process_id, event);
                emitted_terminal |= matches!(
                    runtime_event,
                    RuntimeEvent::Exit { .. } | RuntimeEvent::Error { .. }
                );
                if send_host_message(
                    &sender,
                    HostToGuestMessage::RuntimeEvent {
                        event: runtime_event,
                    },
                )
                .is_err()
                {
                    return;
                }
            }
            if emitted_terminal {
                return;
            }

            if !result.status.is_running() {
                let runtime_event = match result.status {
                    exoharness::SandboxProcessStatus::Running => continue,
                    exoharness::SandboxProcessStatus::Exited { exit_code } => RuntimeEvent::Exit {
                        process_id,
                        exit_code: Some(exit_code),
                    },
                    exoharness::SandboxProcessStatus::Failed { message } => RuntimeEvent::Error {
                        process_id,
                        message,
                    },
                    exoharness::SandboxProcessStatus::Cancelled => RuntimeEvent::Exit {
                        process_id,
                        exit_code: None,
                    },
                };
                if send_host_message(
                    &sender,
                    HostToGuestMessage::RuntimeEvent {
                        event: runtime_event,
                    },
                )
                .is_err()
                {
                    return;
                }
                return;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

fn sandbox_process_event_to_runtime_event(
    process_id: u64,
    event: SandboxProcessEvent,
) -> RuntimeEvent {
    match event {
        SandboxProcessEvent::Stdout { data, .. } => RuntimeEvent::Output {
            process_id,
            stream: SandboxProcessStream::Stdout,
            data: String::from_utf8_lossy(&data).into_owned(),
        },
        SandboxProcessEvent::Stderr { data, .. } => RuntimeEvent::Output {
            process_id,
            stream: SandboxProcessStream::Stderr,
            data: String::from_utf8_lossy(&data).into_owned(),
        },
        SandboxProcessEvent::Exit { exit_code, .. } => RuntimeEvent::Exit {
            process_id,
            exit_code: Some(exit_code),
        },
        SandboxProcessEvent::Error { message, .. } => RuntimeEvent::Error {
            process_id,
            message,
        },
        SandboxProcessEvent::Cancelled { .. } => RuntimeEvent::Exit {
            process_id,
            exit_code: None,
        },
    }
}

fn send_host_message(
    sender: &mpsc::UnboundedSender<HostToGuestMessage>,
    message: HostToGuestMessage,
) -> Result<()> {
    sender
        .send(message)
        .map_err(|_| anyhow!("typescript harness runner stdin closed"))
}

async fn write_protocol_message(
    writer: &mut BufWriter<tokio::process::ChildStdin>,
    message: &HostToGuestMessage,
) -> Result<()> {
    let encoded = serde_json::to_vec(message)?;
    writer.write_all(&encoded).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

fn format_error_chain(error: &anyhow::Error, context: std::fmt::Arguments<'_>) -> String {
    let mut message = context.to_string();
    for (index, cause) in error.chain().enumerate() {
        if index == 0 {
            message.push_str(": ");
        } else {
            message.push_str("\ncaused by: ");
        }
        message.push_str(&cause.to_string());
    }
    message
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex as StdMutex;

    #[cfg(unix)]
    use lingua::Message;
    #[cfg(unix)]
    use lingua::universal::UserContent;
    #[cfg(unix)]
    use tempfile::TempDir;
    #[cfg(unix)]
    use tokio_stream::StreamExt;

    use super::*;
    #[cfg(unix)]
    use crate::test_support::local_test_config;
    #[cfg(unix)]
    use crate::{
        AgentHarnessKind, CreateAgentRequest, CreateConversationRequest, ExecutionStreamHandle,
        Harness, SandboxProvider, SandboxScope, SendRequest, TypeScriptHarnessConfig,
    };

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn queued_turns_replace_cancelled_and_failed_runners() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let bin_dir = tempdir.path().join("bin");
        fs::create_dir(&bin_dir).expect("fake node bin should exist");
        let node_path = bin_dir.join("node");
        fs::write(
            &node_path,
            r#"#!/bin/sh
pending=0
while IFS= read -r line; do
  if [ "$pending" -eq 1 ]; then
    printf '%s\n' '{"kind":"done"}'
    pending=0
  fi
  case "$line" in
    *pending*)
      printf '%s\n' '{"kind":"stream_event","event":{"type":"first_chunk","ttft_ms":1}}'
      pending=1
      ;;
    *fail*)
      printf '%s\n' '{"kind":"stream_event","event":{"type":"first_chunk","ttft_ms":1}}'
      while [ ! -f "$FAIL_SIGNAL" ]; do sleep 0.01; done
      printf '%s\n' '{"kind":"error","message":"expected failure","stack":"private stack"}'
      exit 1
      ;;
    *fresh*)
      printf '%s\n' '{"kind":"stream_event","event":{"type":"text_delta","text":"fresh"}}'
      printf '%s\n' '{"kind":"done"}'
      ;;
  esac
done
"#,
        )
        .expect("fake node should be written");
        let mut permissions = fs::metadata(&node_path)
            .expect("fake node metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&node_path, permissions).expect("fake node should be executable");
        let module_path = tempdir.path().join("harness.ts");
        fs::write(&module_path, "export default {};").expect("module should be written");
        let module_path = module_path.to_string_lossy().into_owned();
        let fail_signal = tempdir.path().join("fail");

        let exoharness = Arc::new(
            BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
                .await
                .expect("basic exoharness should initialize"),
        ) as Arc<dyn ExoHarness>;
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = std::env::var("PATH").unwrap_or_default();
        let executor = TypeScriptExecutor::new(
            Arc::clone(&exoharness),
            workspace_root,
            HashMap::from([
                ("PATH".to_string(), format!("{}:{path}", bin_dir.display())),
                (
                    "FAIL_SIGNAL".to_string(),
                    fail_signal.to_string_lossy().into_owned(),
                ),
            ]),
            Arc::new(BasicToolRuntime),
        );
        let executor_state = executor.clone();
        let harness = TypeScriptHarness {
            inner: SharedHarness::new(exoharness, ExecutorHarnessRuntime::new(executor, None)),
        };
        let agent = harness
            .create_agent(CreateAgentRequest {
                slug: "typescript".to_string(),
                name: None,
                harness: AgentHarnessKind::TypeScript,
                typescript: Some(TypeScriptHarnessConfig {
                    module_path: module_path.clone(),
                    tool_module_paths: Vec::new(),
                }),
                enable_agent_tool_creation: false,
                sandbox_image: None,
                sandbox_provider: SandboxProvider::LocalProcess,
                sandbox_scope: Some(SandboxScope::Conversation),
                enable_networking: false,
                model: "test".to_string(),
                max_output_tokens: None,
                max_tool_round_trips: None,
                braintrust: None,
            })
            .await
            .expect("agent should be created");
        let first_conversation = agent
            .create_conversation(CreateConversationRequest::default())
            .await
            .expect("first conversation should be created");
        let second_conversation = agent
            .create_conversation(CreateConversationRequest::default())
            .await
            .expect("second conversation should be created");
        let third_conversation = agent
            .create_conversation(CreateConversationRequest::default())
            .await
            .expect("third conversation should be created");
        let fourth_conversation = agent
            .create_conversation(CreateConversationRequest::default())
            .await
            .expect("fourth conversation should be created");

        let mut first = first_conversation
            .send_stream(SendRequest {
                input: vec![Message::User {
                    content: UserContent::String("pending".to_string()),
                }],
                session_id: None,
            })
            .await
            .expect("first stream should start");
        tokio::time::timeout(Duration::from_secs(1), first.next())
            .await
            .expect("first runner should start")
            .expect("first runner should emit an event")
            .expect("first runner event should succeed");
        let mut second = second_conversation
            .send_stream(SendRequest {
                input: vec![Message::User {
                    content: UserContent::String("fresh".to_string()),
                }],
                session_id: None,
            })
            .await
            .expect("second stream should start");
        wait_for_queued_turn(&executor_state, &module_path).await;
        tokio::time::timeout(Duration::from_secs(1), first.cancel())
            .await
            .expect("first stream should cancel promptly")
            .expect("first stream should support cancellation");

        assert_eq!(stream_text(&mut second).await, "fresh");
        second.join().await.expect("second stream task should join");

        let mut failed = third_conversation
            .send_stream(SendRequest {
                input: vec![Message::User {
                    content: UserContent::String("fail".to_string()),
                }],
                session_id: None,
            })
            .await
            .expect("failing stream should start");
        tokio::time::timeout(Duration::from_secs(1), failed.next())
            .await
            .expect("failing runner should start")
            .expect("failing runner should emit an event")
            .expect("failing runner event should succeed");
        let mut after_failure = fourth_conversation
            .send_stream(SendRequest {
                input: vec![Message::User {
                    content: UserContent::String("fresh".to_string()),
                }],
                session_id: None,
            })
            .await
            .expect("queued stream should start");
        wait_for_queued_turn(&executor_state, &module_path).await;
        fs::write(&fail_signal, "").expect("failure signal should be written");
        tokio::time::timeout(Duration::from_secs(1), failed.next())
            .await
            .expect("first queued turn should fail promptly")
            .expect("first queued turn should emit its failure")
            .expect_err("first queued turn should fail");
        failed.join().await.expect("failed stream task should join");

        assert_eq!(stream_text(&mut after_failure).await, "fresh");
        after_failure
            .join()
            .await
            .expect("replacement stream task should join");
    }

    #[cfg(unix)]
    async fn wait_for_queued_turn(
        executor: &TypeScriptExecutor<BasicToolRuntime>,
        module_path: &str,
    ) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let queued = executor
                    .runners
                    .lock()
                    .expect("typescript runner cache poisoned")
                    .get(module_path)
                    .is_some_and(|runner| Arc::strong_count(runner) >= 3);
                if queued {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("turn should queue on the active runner");
    }

    #[cfg(unix)]
    async fn stream_text(stream: &mut ExecutionStreamHandle) -> String {
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                match event.expect("stream event should succeed") {
                    ExecutionStreamEvent::Chunk(chunk) => {
                        for choice in chunk.choices {
                            if let Some(delta) = choice.delta_view()
                                && let Some(content) = delta.content
                            {
                                text.push_str(&content);
                            }
                        }
                    }
                    ExecutionStreamEvent::Completed(_) => break,
                    _ => {}
                }
            }
            text
        })
        .await
        .expect("stream should complete promptly")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn latest_sandbox_process_event_cursor_pages_to_latest_cursor() {
        let pages = Arc::new(StdMutex::new(VecDeque::from([
            GetSandboxProcessEventsResult {
                events: (1..=1000)
                    .map(|cursor| SandboxProcessEvent::Stdout {
                        cursor,
                        data: Vec::new(),
                    })
                    .collect(),
                cursor: Some(1000),
                status: SandboxProcessStatus::Running,
            },
            GetSandboxProcessEventsResult {
                events: vec![SandboxProcessEvent::Stdout {
                    cursor: 1001,
                    data: Vec::new(),
                }],
                cursor: Some(1001),
                status: SandboxProcessStatus::Running,
            },
        ])));
        let requested_after = Arc::new(StdMutex::new(Vec::new()));

        let result = latest_sandbox_process_event_cursor_from_fetch({
            let pages = Arc::clone(&pages);
            let requested_after = Arc::clone(&requested_after);
            move |after| {
                requested_after
                    .lock()
                    .expect("requested_after lock")
                    .push(after);
                let page = pages
                    .lock()
                    .expect("pages lock")
                    .pop_front()
                    .expect("expected a page");
                async move { Ok(page) }
            }
        })
        .await
        .expect("cursor lookup should succeed");

        assert_eq!(result.cursor, Some(1001));
        assert_eq!(result.status, SandboxProcessStatus::Running);
        assert_eq!(
            *requested_after.lock().expect("requested_after lock"),
            vec![None, Some(1000)]
        );
        assert!(pages.lock().expect("pages lock").is_empty());
    }

    // Regression: a tool that passes a bad field (here a secret name where a
    // UUID is required) makes the full envelope fail to decode. That must not
    // tear down the turn — we recover the `id` and reject just that request.
    #[test]
    fn undecodable_exo_request_is_rejected_by_id() {
        let line = r#"{"kind":"exo_request","id":12,"request":{"type":"conversation_get_secret","agent_id":"019f4823-3441-7450-b14e-0d8951193dc8","conversation_id":"019f4823-34d5-76e1-b2ee-8da7ad6b0dc9","secret_id":"youtube-client-secret"}}"#;

        // The full message really does fail to decode, so the fallback matters.
        let decode_error = serde_json::from_str::<GuestToHostMessage>(line)
            .expect_err("a non-UUID secret_id must fail to decode");

        let detail = format!("bad message: {decode_error}");
        let response = undecodable_message_response(line, detail.clone())
            .expect("a request with an id should produce an error response");

        match response {
            HostToGuestMessage::ExoResponse {
                id,
                ok,
                response,
                error,
            } => {
                assert_eq!(id, 12);
                assert!(!ok);
                assert!(response.is_none());
                assert_eq!(error.as_deref(), Some(detail.as_str()));
            }
            other => panic!("expected an ExoResponse, got {other:?}"),
        }
    }

    #[test]
    fn undecodable_runtime_request_is_rejected_by_id() {
        let line = r#"{"kind":"runtime_request","id":7,"request":{"type":"totally_unknown"}}"#;
        let response = undecodable_message_response(line, "bad message".to_string())
            .expect("a runtime request with an id should produce an error response");

        match response {
            HostToGuestMessage::RuntimeResponse { id, ok, .. } => {
                assert_eq!(id, 7);
                assert!(!ok);
            }
            other => panic!("expected a RuntimeResponse, got {other:?}"),
        }
    }

    // Without an id (or with an unrecognized kind) there is no pending guest
    // call to reject, so the line is dropped instead of aborting the turn.
    #[test]
    fn undecodable_message_without_id_is_dropped() {
        for line in [
            r#"{"kind":"exo_request","request":{"type":"conversation_get_secret"}}"#,
            r#"{"kind":"stream_event","event":{"garbage":true}}"#,
            r#"{"not":"even close"}"#,
            "not json at all",
        ] {
            assert!(
                undecodable_message_response(line, "bad message".to_string()).is_none(),
                "expected {line} to be dropped",
            );
        }
    }
}

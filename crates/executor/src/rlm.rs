use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::{
    AgentConfig, BraintrustRuntimeConfig, ConversationConfig, ExecutionStreamEvent, ModelClient,
    ModelRequest, ModelResponse, ToolDefinition,
};
use anyhow::{Context as AnyhowContext, anyhow, bail};
use exoharness::{
    AgentHandle, BasicExoHarness, BasicExoHarnessConfig, ConversationHandle, EventData, EventId,
    ExoHarness, FileSystemMountMode, Result, ToolCallId, ToolRequest, ToolResult, TurnHandle,
};
use lingua::Message;
use lingua::universal::{ToolContentPart, ToolResultContentPart};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::execution_tracing::{LlmExecutionTrace, TurnExecutionTrace};
use crate::harness_executor::{ExecutorHarnessRuntime, ExecutorStreamMode, HarnessExecutor};
use crate::harness_facade::{SharedHarness, SharedHarnessBacked};
use crate::harness_helpers::{
    ResolvedModelBinding, assistant_message, assistant_messages_text,
    materialize_conversation_messages, messages_to_history_messages, messages_to_transcript,
    resolve_model_binding, system_message, to_lingua_value, user_message,
};
use crate::harness_js_repl::JsReplState;
use crate::harness_runtime::RouterModelClient;
use crate::shared::try_send_stream_event;

const RLM_STDOUT_PREVIEW_CHARS: usize = 12_000;
const RLM_RESULT_PREVIEW_CHARS: usize = 12_000;
const RLM_CONTEXT_PREVIEW_CHARS: usize = 400;

pub struct RlmExecutor<M> {
    model: Arc<M>,
}

impl<M> Clone for RlmExecutor<M> {
    fn clone(&self) -> Self {
        Self {
            model: Arc::clone(&self.model),
        }
    }
}

impl<M> RlmExecutor<M> {
    pub fn new(model: Arc<M>) -> Self {
        Self { model }
    }
}

impl<M> RlmExecutor<M>
where
    M: ModelClient + 'static,
{
    async fn run_turn_loop(
        &self,
        conversation: &dyn ConversationHandle,
        turn: &dyn TurnHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        query_text: &str,
        turn_trace: Option<&dyn TurnExecutionTrace>,
        event_tx: Option<&mpsc::UnboundedSender<Result<ExecutionStreamEvent>>>,
    ) -> Result<()> {
        let context_messages = materialize_conversation_messages(conversation)
            .await
            .context("failed to materialize conversation messages for RLM context")?;
        let model_binding = resolve_model_binding(conversation, &agent_config.model).await?;
        let context_text = messages_to_transcript(&context_messages);
        let history_messages = messages_to_history_messages(&context_messages);
        let mut js_state = JsReplState::new(&context_text, &history_messages)?;
        append_custom_event(
            turn,
            "rlm_context_initialized",
            &RlmContextInitializedEvent {
                engine: "boa_js".to_string(),
                context_chars: context_text.chars().count(),
                query_chars: query_text.chars().count(),
            },
        )
        .await?;

        let mut history = agent_config.instructions.clone();
        history.push(system_message(&build_rlm_system_prompt(
            conversation_config,
        )));
        history.push(user_message(&build_rlm_root_prompt(
            query_text,
            &context_text,
            conversation_config,
        )));
        let model_session_id = conversation.record().id.to_string();

        let mut round = 0u32;
        loop {
            if agent_config
                .max_tool_round_trips
                .is_some_and(|limit| round > limit)
            {
                bail!("RLM turn exceeded the configured round budget");
            }

            let request = ModelRequest {
                model: model_binding.model.clone(),
                provider: model_binding.provider.clone(),
                auth: model_binding.auth.clone(),
                base_url: model_binding.base_url.clone(),
                messages: history.clone(),
                tools: build_rlm_tool_definitions(),
                max_output_tokens: agent_config.max_output_tokens,
                session_id: Some(model_session_id.clone()),
            };
            let llm_trace = match turn_trace {
                Some(turn_trace) => turn_trace.start_llm_round(&request, round as usize).await,
                None => None,
            };
            let response = if let Some(event_tx) = event_tx {
                self.complete_streaming(request, event_tx, llm_trace)
                    .await?
            } else {
                match self.model.complete(request).await {
                    Ok(response) => {
                        if let Some(llm_trace) = llm_trace {
                            llm_trace.finish_success(&response, None).await;
                        }
                        response
                    }
                    Err(error) => {
                        if let Some(llm_trace) = llm_trace {
                            llm_trace.finish_error(&error).await;
                        }
                        return Err(error);
                    }
                }
            };

            append_custom_event(
                turn,
                "rlm_model_response",
                &RlmModelResponseEvent {
                    round,
                    response_id: response.response_id,
                    messages: response.messages.clone(),
                    tool_call_count: response.tool_calls.len(),
                },
            )
            .await?;

            history.extend(response.messages.clone());

            if response.tool_calls.is_empty() {
                let final_answer = resolve_final_answer(&js_state, &response.messages)?;
                append_final_answer(turn, response.response_id, round, &final_answer).await?;
                return Ok(());
            }

            let mut tool_messages = Vec::with_capacity(response.tool_calls.len());
            for tool_call in response.tool_calls {
                if let Some(event_tx) = event_tx {
                    try_send_stream_event(
                        event_tx,
                        ExecutionStreamEvent::ToolCall {
                            tool_call_id: tool_call.tool_call_id.clone(),
                            tool_name: tool_call.request.function_name.clone(),
                            arguments: tool_call.request.arguments.clone(),
                        },
                    );
                }
                let tool_trace = match turn_trace {
                    Some(turn_trace) => {
                        turn_trace
                            .start_tool_call(&tool_call.request, round as usize)
                            .await
                    }
                    None => None,
                };
                append_custom_event(
                    turn,
                    "rlm_tool_call",
                    &RlmToolCallEvent {
                        round,
                        tool_call_id: tool_call.tool_call_id.clone(),
                        request: tool_call.request.clone(),
                    },
                )
                .await?;

                let result = match self
                    .execute_tool_call(
                        &mut js_state,
                        agent_config,
                        &model_binding,
                        &model_session_id,
                        &tool_call.request,
                    )
                    .await
                {
                    Ok(result) => {
                        if let Some(tool_trace) = tool_trace {
                            tool_trace.finish_success(&result).await;
                        }
                        result
                    }
                    Err(error) => {
                        if let Some(tool_trace) = tool_trace {
                            tool_trace.finish_error(&error).await;
                        }
                        return Err(error);
                    }
                };
                append_custom_event(
                    turn,
                    "rlm_tool_result",
                    &RlmToolResultEvent {
                        round,
                        tool_call_id: tool_call.tool_call_id.clone(),
                        result: result.clone(),
                    },
                )
                .await?;
                if let Some(event_tx) = event_tx {
                    try_send_stream_event(
                        event_tx,
                        ExecutionStreamEvent::ToolResult {
                            tool_call_id: tool_call.tool_call_id.clone(),
                            result: result.clone(),
                        },
                    );
                }
                if let Some(final_answer) = js_state.final_value()? {
                    append_final_answer(turn, response.response_id, round, &final_answer).await?;
                    return Ok(());
                }
                tool_messages.push(Message::Tool {
                    content: vec![ToolContentPart::ToolResult(ToolResultContentPart {
                        tool_call_id: tool_call.tool_call_id,
                        tool_name: tool_call.request.function_name,
                        output: to_lingua_value(result),
                        provider_options: None,
                    })],
                });
            }
            history.extend(tool_messages);
            round += 1;
        }
    }

    async fn complete_streaming(
        &self,
        request: ModelRequest,
        event_tx: &mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
        llm_trace: Option<Box<dyn LlmExecutionTrace>>,
    ) -> Result<ModelResponse> {
        let started_at = Instant::now();
        let mut stream = match self.model.complete_stream(request).await {
            Ok(stream) => stream,
            Err(error) => {
                if let Some(llm_trace) = llm_trace {
                    llm_trace.finish_error(&error).await;
                }
                return Err(error);
            }
        };
        let mut ttft = None;
        loop {
            let chunk = match stream.next_chunk().await {
                Ok(chunk) => chunk,
                Err(error) => {
                    if let Some(llm_trace) = llm_trace {
                        llm_trace.finish_error(&error).await;
                    }
                    return Err(error);
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            if chunk.is_keep_alive() {
                continue;
            }
            if ttft.is_none() {
                let first_chunk = started_at.elapsed();
                ttft = Some(first_chunk);
                try_send_stream_event(
                    event_tx,
                    ExecutionStreamEvent::FirstChunk { ttft: first_chunk },
                );
            }
            // RLM root-model text is executor control traffic rather than user-facing output.
            // The final persisted assistant message is emitted after the loop finishes, so
            // streaming these raw chunks would leak control syntax like FINAL(...).
        }
        let response = match stream.finish().await {
            Ok(response) => response,
            Err(error) => {
                if let Some(llm_trace) = llm_trace {
                    llm_trace.finish_error(&error).await;
                }
                return Err(error);
            }
        };
        if let Some(llm_trace) = llm_trace {
            llm_trace.finish_success(&response, ttft).await;
        }
        Ok(response)
    }

    async fn execute_tool_call(
        &self,
        js_state: &mut JsReplState,
        agent_config: &AgentConfig,
        model_binding: &ResolvedModelBinding,
        session_id: &str,
        request: &ToolRequest,
    ) -> Result<ToolResult> {
        match request.function_name.as_str() {
            "repl_execute" => {
                let args: ReplExecuteArguments = parse_tool_arguments(request)?;
                Ok(serde_json::to_value(execute_repl_code(
                    js_state, &args.code,
                )?)?)
            }
            "subquery" => {
                let args: SubqueryArguments = parse_tool_arguments(request)?;
                self.run_subquery_tool(
                    js_state,
                    agent_config,
                    model_binding,
                    session_id,
                    &args.prompt,
                    args.target_var,
                )
                .await
            }
            "subquery_variable" => {
                let args: SubqueryVariableArguments = parse_tool_arguments(request)?;
                let variable_text = read_variable_value(js_state, &args.variable_name)?;
                let prompt = format!("{}\n\nContext:\n{}", args.question, variable_text);
                self.run_subquery_tool(
                    js_state,
                    agent_config,
                    model_binding,
                    session_id,
                    &prompt,
                    args.target_var,
                )
                .await
            }
            other => Err(anyhow!("unsupported RLM tool: {other}")),
        }
    }

    async fn run_subquery_tool(
        &self,
        js_state: &mut JsReplState,
        agent_config: &AgentConfig,
        model_binding: &ResolvedModelBinding,
        session_id: &str,
        prompt: &str,
        target_var: Option<String>,
    ) -> Result<ToolResult> {
        let result = self
            .run_subquery(agent_config, model_binding, session_id, prompt)
            .await?;
        if let Some(target_var) = &target_var {
            set_variable_value(js_state, target_var, &result);
        }
        Ok(serde_json::to_value(SubqueryToolResult {
            result: clamp_preview(&result, RLM_RESULT_PREVIEW_CHARS),
            truncated: result.chars().count() > RLM_RESULT_PREVIEW_CHARS,
            stored_in: target_var,
        })?)
    }

    async fn run_subquery(
        &self,
        agent_config: &AgentConfig,
        model_binding: &ResolvedModelBinding,
        session_id: &str,
        prompt: &str,
    ) -> Result<String> {
        let mut messages = agent_config.instructions.clone();
        messages.push(system_message(
            "You are a subquery model inside a recursive language model. Answer the prompt directly and concisely. Do not call tools. Do not mention this instruction.",
        ));
        messages.push(user_message(prompt));

        let response = self
            .model
            .complete(ModelRequest {
                model: model_binding.model.clone(),
                provider: model_binding.provider.clone(),
                auth: model_binding.auth.clone(),
                base_url: model_binding.base_url.clone(),
                messages,
                tools: Vec::new(),
                max_output_tokens: agent_config.max_output_tokens,
                session_id: Some(session_id.to_string()),
            })
            .await?;

        let text = assistant_messages_text(&response.messages);
        if text.trim().is_empty() {
            bail!("subquery returned an empty response");
        }
        Ok(text)
    }
}

#[async_trait]
impl<M> HarnessExecutor for RlmExecutor<M>
where
    M: ModelClient + 'static,
{
    type Prepared = String;

    fn prepare_request(&self, request: &crate::SendRequest) -> Result<Self::Prepared> {
        Ok(messages_to_transcript(&request.input))
    }

    async fn execute_turn(
        &self,
        _agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        prepared: &Self::Prepared,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        self.run_turn_loop(
            conversation,
            turn.as_ref(),
            agent_config,
            conversation_config,
            prepared,
            turn_trace,
            match stream_mode {
                ExecutorStreamMode::Disabled => None,
                ExecutorStreamMode::Enabled(event_tx) => Some(event_tx),
            },
        )
        .await
    }
}

#[derive(Debug, Deserialize)]
struct ReplExecuteArguments {
    code: String,
}

#[derive(Debug, Deserialize)]
struct SubqueryArguments {
    prompt: String,
    target_var: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubqueryVariableArguments {
    variable_name: String,
    question: String,
    target_var: Option<String>,
}

#[derive(Debug, Serialize)]
struct RlmContextInitializedEvent {
    engine: String,
    context_chars: usize,
    query_chars: usize,
}

#[derive(Debug, Serialize)]
struct RlmModelResponseEvent {
    round: u32,
    response_id: Option<exoharness::ResponseId>,
    messages: Vec<Message>,
    tool_call_count: usize,
}

#[derive(Debug, Serialize)]
struct RlmToolCallEvent {
    round: u32,
    tool_call_id: ToolCallId,
    request: ToolRequest,
}

#[derive(Debug, Serialize)]
struct RlmToolResultEvent {
    round: u32,
    tool_call_id: ToolCallId,
    result: ToolResult,
}

#[derive(Debug, Serialize)]
struct RlmFinalAnswerEvent {
    round: u32,
    chars: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct PreparedReplExecutionResult {
    stdout: String,
    variable_names: Vec<String>,
    error: Option<String>,
    final_preview: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SubqueryToolResult {
    result: String,
    truncated: bool,
    stored_in: Option<String>,
}

#[derive(Debug)]
enum FinalDirective {
    Direct(String),
    Variable(String),
}

async fn append_custom_event<T: Serialize>(
    turn: &dyn TurnHandle,
    event_type: &str,
    payload: &T,
) -> Result<EventId> {
    Ok(turn
        .add_events(vec![EventData::Custom {
            event_type: event_type.to_string(),
            payload: serde_json::to_value(payload)?,
        }])
        .await?
        .latest_event_id)
}

async fn append_final_answer(
    turn: &dyn TurnHandle,
    response_id: Option<exoharness::ResponseId>,
    round: u32,
    final_answer: &str,
) -> Result<()> {
    turn.add_events(vec![EventData::Messages {
        messages: vec![assistant_message(final_answer)],
        response_id,
        usage: None,
    }])
    .await?;
    append_custom_event(
        turn,
        "rlm_final_answer",
        &RlmFinalAnswerEvent {
            round,
            chars: final_answer.chars().count(),
        },
    )
    .await?;
    Ok(())
}

fn parse_tool_arguments<T: DeserializeOwned>(request: &ToolRequest) -> Result<T> {
    Ok(serde_json::from_value(Value::Object(
        request.arguments.clone(),
    ))?)
}

fn execute_repl_code(
    js_state: &mut JsReplState,
    code: &str,
) -> Result<PreparedReplExecutionResult> {
    let result = js_state.execute(code)?;
    Ok(PreparedReplExecutionResult {
        stdout: clamp_preview(&result.stdout, RLM_STDOUT_PREVIEW_CHARS),
        variable_names: result.variable_names,
        error: result.error,
        final_preview: result.final_preview,
    })
}

fn read_variable_value(js_state: &JsReplState, variable_name: &str) -> Result<String> {
    js_state.read_variable(variable_name)
}

fn set_variable_value(js_state: &mut JsReplState, variable_name: &str, value: &str) {
    js_state.set_variable(variable_name, value);
}

fn resolve_final_answer(js_state: &JsReplState, messages: &[Message]) -> Result<String> {
    match parse_final_directive(messages) {
        Some(FinalDirective::Direct(answer)) => Ok(answer),
        Some(FinalDirective::Variable(name)) => read_variable_value(js_state, &name),
        None => {
            let text = assistant_messages_text(messages);
            if text.trim().is_empty() {
                bail!("RLM response did not contain a final answer")
            }
            Ok(text)
        }
    }
}

fn parse_final_directive(messages: &[Message]) -> Option<FinalDirective> {
    let text = assistant_messages_text(messages);
    let trimmed = text.trim();
    if let Some(name) = trimmed
        .strip_prefix("FINAL_VAR(")
        .and_then(|value| value.strip_suffix(')'))
    {
        return Some(FinalDirective::Variable(name.trim().to_string()));
    }
    if let Some(answer) = trimmed
        .strip_prefix("FINAL(")
        .and_then(|value| value.strip_suffix(')'))
    {
        return Some(FinalDirective::Direct(answer.trim().to_string()));
    }
    None
}

fn build_rlm_system_prompt(config: &ConversationConfig) -> String {
    let mut prompt = String::from(
        "You are tasked with answering a query with associated context. You can access, transform, and analyze this context interactively in a persistent JavaScript REPL environment that can recursively query sub-LLMs, which you are strongly encouraged to use as much as possible. You will be queried iteratively until you provide a final answer.\n\n\
The REPL is secure and intentionally limited:\n\
- no filesystem access\n\
- no network access\n\
- only JSON-compatible values persist across calls\n\
- persistent values should be stored on `globalThis`\n\n\
The REPL is initialized with:\n\
1. A `context` variable that contains the full prompt as a string. This variable contains extremely important information. You should inspect it explicitly before answering.\n\
2. A `getMessages(role = null)` JavaScript helper function backed by a turn-start snapshot of exoharness conversation history. It returns an array of `{{ index, role, content }}` objects, optionally filtered by role.\n\
3. A `repl_execute` tool that runs JavaScript in the persistent REPL namespace.\n\
4. `subquery` and `subquery_variable` tools that let you recursively query the underlying LLM over prompt strings or stored JavaScript variables.\n\
5. A `print(...)` function in the REPL that lets you inspect short outputs between iterations.\n\n\
Use the tools this way:\n\
- `getMessages(...)` is the easiest way to retrieve prior messages without regexing the transcript manually, and you can compose its array results with normal JavaScript filtering, slicing, mapping, and searching however you like.\n\
- `repl_execute` runs JavaScript in the persistent REPL with `context` already loaded.\n\
- `subquery` asks a direct sub-LLM question over a prompt string. It always takes a `target_var` field; pass `null` if you do not want to store the result.\n\
- `subquery_variable` asks a direct sub-LLM question using the string value of a JavaScript variable as external context. It always takes a `target_var` field; pass `null` if you do not want to store the result.\n\n\
Only truncated REPL output is surfaced back to you each iteration, so you should use variables on `globalThis` to store intermediate state and use recursive subqueries to understand long strings.\n\n\
Make sure to explicitly look through the entire context in the REPL before answering your query. A viable strategy is to inspect its structure, chunk it into smart segments, recursively query sub-LLMs over those segments, accumulate buffers in variables, and then synthesize the final answer.\n\n\
When you are done, prefer setting `globalThis.Final` in the REPL to the final answer. You may also reply with `FINAL(<answer>)` or `FINAL_VAR(<javascript_variable_name>)` if needed.\n\
Think step by step carefully, plan, and execute immediately. Do not just say what you will do. Prefer code, variables, and recursive subqueries over long prose.\n",
    );
    if !config.mounts.is_empty() {
        prompt.push_str(
            "\nConversation filesystem mounts do not apply inside this JS REPL. If you need shell or filesystem access, this harness cannot provide it.\n",
        );
    }
    prompt
}

fn build_rlm_root_prompt(
    query_text: &str,
    context_text: &str,
    config: &ConversationConfig,
) -> String {
    let mounts = if config.mounts.is_empty() {
        "No external filesystem mounts are configured for the conversation.".to_string()
    } else {
        config
            .mounts
            .iter()
            .map(|mount| {
                format!(
                    "- {} -> {} ({})",
                    mount.host_path,
                    mount.mount_path,
                    match mount.mode {
                        FileSystemMountMode::ReadOnly => "ro",
                        FileSystemMountMode::ReadWrite => "rw",
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "Latest user request:\n{query}\n\n\
Prompt metadata:\n\
- total characters: {context_chars}\n\
- preview: {preview:?}\n\
- js repl: persistent `context` plus JSON-compatible globals on `globalThis`\n\
- history api: `getMessages(role = null)` returning `{{ index, role, content }}[]`\n\
\nFilesystem mounts:\n{mounts}\n\n\
The prompt string in `context` is the external environment. It is formatted as a flattened transcript with blocks like `USER:\\n...`, `ASSISTANT:\\n...`, and `TOOL:\\n...`, separated by blank lines. Solve the latest request by inspecting and manipulating `context` directly. If you need precise message-level access, use `getMessages(...)` and then slice/filter/search in plain JavaScript. If you need intermediate state, create variables on `globalThis` and reuse them across `repl_execute` calls.",
        query = query_text,
        context_chars = context_text.chars().count(),
        preview = clamp_preview(context_text, RLM_CONTEXT_PREVIEW_CHARS),
        mounts = mounts,
    )
}

fn build_rlm_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "repl_execute".to_string(),
            description: "Execute JavaScript in the persistent REPL namespace. The variable `context` is always available and persistent values should live on `globalThis`.".to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "JavaScript code to execute in the persistent REPL namespace."
                    }
                },
                "required": ["code"]
            }),
        },
        ToolDefinition {
            name: "subquery".to_string(),
            description: "Ask a direct sub-LLM question over a prompt string and optionally store the result in a JavaScript variable.".to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Prompt to send to the sub-LLM."
                    },
                    "target_var": {
                        "type": ["string", "null"],
                        "description": "JavaScript variable name to store the result, or null to avoid storing it."
                    }
                },
                "required": ["prompt", "target_var"]
            }),
        },
        ToolDefinition {
            name: "subquery_variable".to_string(),
            description: "Ask a direct sub-LLM question using the string value of a JavaScript variable as external context, and optionally store the answer in another variable.".to_string(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "variable_name": {
                        "type": "string",
                        "description": "JavaScript variable whose string value will be used as subquery context."
                    },
                    "question": {
                        "type": "string",
                        "description": "Question to ask about the variable value."
                    },
                    "target_var": {
                        "type": ["string", "null"],
                        "description": "JavaScript variable name to store the result, or null to avoid storing it."
                    }
                },
                "required": ["variable_name", "question", "target_var"]
            }),
        },
    ]
}

fn clamp_preview(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

pub struct RlmHarness<M> {
    inner: SharedHarness<ExecutorHarnessRuntime<RlmExecutor<M>>>,
}

impl<M> RlmHarness<M> {
    pub fn new(exoharness: Arc<dyn ExoHarness>, model: Arc<M>) -> Self
    where
        M: ModelClient + 'static,
    {
        let runtime = ExecutorHarnessRuntime::new(RlmExecutor::new(model), None);
        Self {
            inner: SharedHarness::new(exoharness, runtime),
        }
    }
}

impl RlmHarness<RouterModelClient> {
    pub fn from_exoharness(
        exoharness: Arc<dyn ExoHarness>,
        runtime_config: Option<BraintrustRuntimeConfig>,
        env: HashMap<String, String>,
    ) -> Self {
        let model = Arc::new(RouterModelClient::new(env));
        let runtime = ExecutorHarnessRuntime::new(RlmExecutor::new(model), runtime_config);

        Self {
            inner: SharedHarness::new(exoharness, runtime),
        }
    }

    pub async fn from_config(
        exo_config: BasicExoHarnessConfig,
        runtime_config: Option<BraintrustRuntimeConfig>,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        Ok(Self::from_exoharness(
            Arc::new(BasicExoHarness::new(exo_config).await?),
            runtime_config,
            env,
        ))
    }
}

impl<M> SharedHarnessBacked for RlmHarness<M>
where
    M: ModelClient + 'static,
{
    type Runtime = ExecutorHarnessRuntime<RlmExecutor<M>>;

    fn shared_harness(&self) -> &SharedHarness<Self::Runtime> {
        &self.inner
    }
}

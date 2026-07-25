use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use async_trait::async_trait;
use cost::{PricingTable, TokenCounts};
use exoharness::{
    AgentHandle, ConversationHandle, ConversationId, EventData, EventId, EventKind, EventQuery,
    EventQueryDirection, Result, ToolCallId, ToolRequest, TurnHandle, UsageRecord,
};
use lingua::Message;
use lingua::universal::{ToolContentPart, ToolResultContentPart};
use serde_json::json;

use crate::execution_tracing::TurnExecutionTrace;
use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor};
use crate::harness_helpers::{resolve_model_binding, to_lingua_value};
use crate::shared::{HISTORY_CACHE_NAME, try_send_stream_event};
use crate::{
    AgentConfig, ConversationConfig, ExecutionStreamEvent, ModelClient, ModelRequest,
    ModelResponse, SendRequest, ToolDefinition, ToolRuntime,
};

pub struct BasicExecutor<M, T> {
    model: Arc<M>,
    tools: Arc<T>,
    history_cache: Arc<RwLock<HashMap<ConversationId, HistoryCacheEntry>>>,
    pricing: Arc<PricingTable>,
}

impl<M, T> BasicExecutor<M, T> {
    pub fn new(model: Arc<M>, tools: Arc<T>) -> Self {
        Self::with_pricing(model, tools, Arc::new(PricingTable::empty()))
    }

    /// Cost is filled from `pricing`; an empty table leaves `cost_usd` unset.
    pub fn with_pricing(model: Arc<M>, tools: Arc<T>, pricing: Arc<PricingTable>) -> Self {
        Self {
            model,
            tools,
            history_cache: Arc::new(RwLock::new(HashMap::new())),
            pricing,
        }
    }
}

impl<M, T> Clone for BasicExecutor<M, T> {
    fn clone(&self) -> Self {
        Self {
            model: Arc::clone(&self.model),
            tools: Arc::clone(&self.tools),
            history_cache: Arc::clone(&self.history_cache),
            pricing: Arc::clone(&self.pricing),
        }
    }
}

struct ToolRoundContext<'a> {
    agent: &'a dyn AgentHandle,
    conversation: &'a dyn ConversationHandle,
    turn: Arc<dyn TurnHandle>,
    agent_config: &'a AgentConfig,
    conversation_config: &'a ConversationConfig,
    round: usize,
    stream_mode: ExecutorStreamMode<'a>,
    turn_trace: Option<&'a dyn TurnExecutionTrace>,
}

impl<M, T> BasicExecutor<M, T>
where
    M: ModelClient + 'static,
    T: ToolRuntime + 'static,
{
    async fn materialize_prompt_history(
        &self,
        conversation: &dyn ConversationHandle,
        instructions: &[Message],
    ) -> Result<Vec<Message>> {
        let conversation_id = conversation.record().id;
        let cached_entry = {
            let cache = self.history_cache.read().expect(HISTORY_CACHE_NAME);
            cache.get(&conversation_id).cloned()
        };

        let result = conversation
            .get_events(Some(EventQuery {
                cursor: cached_entry.as_ref().and_then(|entry| entry.cursor),
                direction: Some(EventQueryDirection::Asc),
                limit: None,
                session_id: None,
                turn_id: None,
                types: Some(vec![
                    EventKind::MESSAGES,
                    EventKind::TOOL_REQUESTED,
                    EventKind::TOOL_RESULT,
                ]),
            }))
            .await?;

        let mut event_messages = cached_entry
            .as_ref()
            .map_or_else(Vec::new, |entry| entry.messages.clone());
        let mut tool_call_names = cached_entry
            .as_ref()
            .map_or_else(HashMap::new, |entry| entry.tool_call_names.clone());
        extend_message_history(&mut event_messages, &mut tool_call_names, &result.events);
        let cursor = result
            .cursor
            .or_else(|| cached_entry.and_then(|entry| entry.cursor));

        self.history_cache
            .write()
            .expect(HISTORY_CACHE_NAME)
            .insert(
                conversation_id,
                HistoryCacheEntry {
                    cursor,
                    messages: event_messages.clone(),
                    tool_call_names,
                },
            );

        let mut messages = instructions.to_vec();
        messages.extend(event_messages);
        Ok(messages)
    }

    async fn run_turn_loop(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        for round in 0u32.. {
            if agent_config
                .max_tool_round_trips
                .is_some_and(|limit| round > limit)
            {
                return Ok(());
            }

            let messages = self
                .materialize_prompt_history(conversation, &agent_config.instructions)
                .await?;
            let request =
                build_model_request(conversation, agent_config, conversation_config, messages)
                    .await?;
            let response = self
                .complete_model_round(request, round as usize, stream_mode, turn_trace)
                .await?;

            let events = interpret_model_response(response, &self.pricing);
            turn.add_events(events.clone()).await?;

            let tool_requests = collect_tool_requests(&events);
            if tool_requests.is_empty() {
                return Ok(());
            }

            let tool_results = self
                .execute_tool_round(
                    ToolRoundContext {
                        agent,
                        conversation,
                        turn: Arc::clone(&turn),
                        agent_config,
                        conversation_config,
                        round: round as usize,
                        stream_mode,
                        turn_trace,
                    },
                    tool_requests,
                )
                .await?;
            turn.add_events(tool_results).await?;
        }

        Ok(())
    }

    async fn complete_model_round(
        &self,
        request: ModelRequest,
        round: usize,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<ModelResponse> {
        let llm_trace = match turn_trace {
            Some(turn_trace) => turn_trace.start_llm_round(&request, round).await,
            None => None,
        };
        let requested_model = request.model.clone();

        match stream_mode {
            ExecutorStreamMode::Disabled => {
                let started_at = Instant::now();
                let response = match self.model.complete(request).await {
                    Ok(response) => response,
                    Err(error) => {
                        if let Some(llm_trace) = llm_trace {
                            llm_trace.finish_error(&error).await;
                        }
                        return Err(error);
                    }
                };
                let duration = started_at.elapsed();
                let mut response = response;
                if response.model.is_none() {
                    response.model = Some(requested_model);
                }
                if response.duration.is_none() {
                    response.duration = Some(duration);
                }
                if let Some(llm_trace) = llm_trace {
                    llm_trace.finish_success(&response, None).await;
                }
                Ok(response)
            }
            ExecutorStreamMode::Enabled(event_tx) => {
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
                        let measured_ttft = started_at.elapsed();
                        ttft = Some(measured_ttft);
                        try_send_stream_event(
                            event_tx,
                            ExecutionStreamEvent::FirstChunk {
                                ttft: measured_ttft,
                            },
                        );
                    }
                    try_send_stream_event(event_tx, ExecutionStreamEvent::Chunk(chunk));
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
                let duration = started_at.elapsed();
                let mut response = response;
                if response.model.is_none() {
                    response.model = Some(requested_model);
                }
                if response.ttft.is_none() {
                    response.ttft = ttft;
                }
                if response.duration.is_none() {
                    response.duration = Some(duration);
                }
                if let Some(llm_trace) = llm_trace {
                    llm_trace.finish_success(&response, ttft).await;
                }
                Ok(response)
            }
        }
    }

    async fn execute_tool_round(
        &self,
        context: ToolRoundContext<'_>,
        tool_requests: Vec<ExecutableToolRequest>,
    ) -> Result<Vec<EventData>> {
        let mut tool_results = Vec::with_capacity(tool_requests.len());

        for tool_request in tool_requests {
            if let ExecutorStreamMode::Enabled(event_tx) = context.stream_mode {
                try_send_stream_event(
                    event_tx,
                    ExecutionStreamEvent::ToolCall {
                        tool_call_id: tool_request.tool_call_id.clone(),
                        tool_name: tool_request.request.function_name.clone(),
                        arguments: tool_request.request.arguments.clone(),
                    },
                );
            }

            let mut tool_trace = match context.turn_trace {
                Some(turn_trace) => {
                    turn_trace
                        .start_tool_call(&tool_request.request, context.round)
                        .await
                }
                None => None,
            };
            let tool_future = self.tools.execute(
                context.agent,
                context.conversation,
                Some(context.turn.as_ref()),
                context.agent_config,
                context.conversation_config,
                &tool_request.request,
            );
            let (result, tool_succeeded) = match tool_future.await {
                Ok(response) => (response, true),
                Err(error) => {
                    if let Some(tool_trace) = tool_trace.take() {
                        tool_trace.finish_error(&error).await;
                    }
                    (
                        json!({
                            "ok": false,
                            "error": error.to_string(),
                        }),
                        false,
                    )
                }
            };
            if tool_succeeded && let Some(tool_trace) = tool_trace.take() {
                tool_trace.finish_success(&result).await;
            }
            if let ExecutorStreamMode::Enabled(event_tx) = context.stream_mode {
                try_send_stream_event(
                    event_tx,
                    ExecutionStreamEvent::ToolResult {
                        tool_call_id: tool_request.tool_call_id.clone(),
                        result: result.clone(),
                    },
                );
            }
            tool_results.push(EventData::ToolResult {
                tool_call_id: tool_request.tool_call_id,
                result,
            });
        }

        Ok(tool_results)
    }
}

#[async_trait]
impl<M, T> HarnessExecutor for BasicExecutor<M, T>
where
    M: ModelClient + 'static,
    T: ToolRuntime + 'static,
{
    type Prepared = ();

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

    fn prepare_request(&self, _request: &SendRequest) -> Result<Self::Prepared> {
        Ok(())
    }

    async fn execute_turn(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        _prepared: &Self::Prepared,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        self.run_turn_loop(
            agent,
            conversation,
            turn,
            agent_config,
            conversation_config,
            stream_mode,
            turn_trace,
        )
        .await
    }
}

fn extend_message_history(
    history: &mut Vec<Message>,
    tool_call_names: &mut HashMap<ToolCallId, String>,
    events: &[exoharness::Event],
) {
    let mut pending_tool_call_ids = Vec::new();

    for event in events {
        match &event.data {
            EventData::Messages { messages, .. } => {
                flush_dangling_tool_results(history, tool_call_names, &mut pending_tool_call_ids);
                history.extend(messages.clone());
            }
            EventData::ToolRequested {
                tool_call_id,
                request,
                ..
            } => {
                tool_call_names.insert(tool_call_id.clone(), request.function_name.clone());
                pending_tool_call_ids.push(tool_call_id.clone());
            }
            EventData::ToolResult {
                tool_call_id,
                result,
            } => {
                let Some(tool_name) = tool_call_names.get(tool_call_id) else {
                    continue;
                };
                remove_pending_tool_call(&mut pending_tool_call_ids, tool_call_id);
                history.push(Message::Tool {
                    content: vec![ToolContentPart::ToolResult(ToolResultContentPart {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        output: to_lingua_value(result.clone()),
                        provider_options: None,
                    })],
                });
            }
            _ => {}
        }
    }
}

fn flush_dangling_tool_results(
    history: &mut Vec<Message>,
    tool_call_names: &HashMap<ToolCallId, String>,
    pending_tool_call_ids: &mut Vec<ToolCallId>,
) {
    for tool_call_id in std::mem::take(pending_tool_call_ids) {
        let Some(tool_name) = tool_call_names.get(&tool_call_id) else {
            continue;
        };
        history.push(Message::Tool {
            content: vec![ToolContentPart::ToolResult(ToolResultContentPart {
                tool_call_id,
                tool_name: tool_name.clone(),
                output: to_lingua_value(json!({
                    "ok": false,
                    "error": "tool execution did not complete before the previous turn ended",
                })),
                provider_options: None,
            })],
        });
    }
}

fn remove_pending_tool_call(pending_tool_call_ids: &mut Vec<ToolCallId>, tool_call_id: &str) {
    if let Some(index) = pending_tool_call_ids
        .iter()
        .position(|pending| pending == tool_call_id)
    {
        pending_tool_call_ids.remove(index);
    }
}

fn interpret_model_response(response: ModelResponse, pricing: &PricingTable) -> Vec<EventData> {
    let mut events = Vec::new();

    if !response.messages.is_empty() {
        let usage = build_usage_record(&response, pricing);
        events.push(EventData::Messages {
            messages: response.messages,
            response_id: response.response_id,
            usage,
        });
    }

    for tool_call in response.tool_calls {
        events.push(EventData::ToolRequested {
            tool_call_id: tool_call.tool_call_id,
            response_id: response.response_id,
            request: tool_call.request,
        });
    }

    events
}

fn build_usage_record(
    response: &ModelResponse,
    pricing: &PricingTable,
) -> Option<Box<UsageRecord>> {
    // Only emit a record when we have *something* worth recording — token usage
    // or timing. Skipping when both are absent keeps event JSON clean for
    // tests/fakes that don't populate metadata.
    let has_usage = response.usage.is_some();
    let has_timing = response.ttft.is_some() || response.duration.is_some();
    if !has_usage && !has_timing {
        return None;
    }

    let model = response.model.clone().unwrap_or_default();
    let (
        prompt_tokens,
        completion_tokens,
        prompt_cached_tokens,
        prompt_cache_creation_tokens,
        completion_reasoning_tokens,
    ) = match &response.usage {
        Some(u) => (
            u.prompt_tokens,
            u.completion_tokens,
            u.prompt_cached_tokens,
            u.prompt_cache_creation_tokens,
            u.completion_reasoning_tokens,
        ),
        None => (None, None, None, None, None),
    };

    // Prefer the provider-reported cost (e.g. OpenRouter's `usage.cost`); fall
    // back to the local price-table estimate when the provider doesn't send one.
    let cost_usd = response.provider_cost_usd.or_else(|| {
        if has_usage && !model.is_empty() {
            pricing.compute_cost_usd(
                &model,
                TokenCounts {
                    prompt: prompt_tokens,
                    completion: completion_tokens,
                    prompt_cached: prompt_cached_tokens,
                    prompt_cache_creation: prompt_cache_creation_tokens,
                },
            )
        } else {
            None
        }
    });

    Some(Box::new(UsageRecord {
        model,
        prompt_tokens,
        completion_tokens,
        prompt_cached_tokens,
        prompt_cache_creation_tokens,
        completion_reasoning_tokens,
        cost_usd,
        ttft_ms: response.ttft.map(|d| d.as_millis() as u64),
        duration_ms: response.duration.map(|d| d.as_millis() as u64),
    }))
}

#[derive(Debug, Clone)]
struct ExecutableToolRequest {
    tool_call_id: String,
    request: ToolRequest,
}

fn collect_tool_requests(events: &[EventData]) -> Vec<ExecutableToolRequest> {
    events
        .iter()
        .filter_map(|event| match event {
            EventData::ToolRequested {
                tool_call_id,
                request,
                ..
            } => Some(ExecutableToolRequest {
                tool_call_id: tool_call_id.clone(),
                request: request.clone(),
            }),
            _ => None,
        })
        .collect()
}

async fn build_model_request(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
    messages: Vec<Message>,
) -> Result<ModelRequest> {
    let model_binding = resolve_model_binding(conversation, &agent_config.model).await?;
    Ok(ModelRequest {
        model: model_binding.model,
        provider: model_binding.provider,
        auth: model_binding.auth,
        base_url: model_binding.base_url,
        messages,
        tools: build_tool_definitions(conversation_config),
        max_output_tokens: agent_config.max_output_tokens,
        session_id: Some(conversation.record().id.to_string()),
    })
}

fn build_tool_definitions(config: &ConversationConfig) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    if let Some(program) = &config.shell_program {
        tools.push(ToolDefinition {
            name: "shell".to_string(),
            description: format!("Run a shell command using {program}."),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute."
                    }
                },
                "required": ["command"]
            }),
        });
    }

    tools
}

#[derive(Debug, Clone, Default)]
struct HistoryCacheEntry {
    cursor: Option<EventId>,
    messages: Vec<Message>,
    tool_call_names: HashMap<ToolCallId, String>,
}

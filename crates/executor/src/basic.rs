use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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

use crate::compaction::{
    CompactionLatch, CompactionOutcome, PromptSize, SummarizeInput, previous_summary_message,
    prompt_size, read_active_checkpoint, read_latest_turn_ended, read_summary_or_fall_back,
    record_summarizer_usage, resolve_summarizer_model, run_compaction, should_compact,
    summarizer_instruction, summarizer_max_output_tokens, summary_message, tool_definition_size,
};
use crate::execution_tracing::TurnExecutionTrace;
use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor};
use crate::harness_helpers::{
    assistant_messages_text, resolve_model_binding, system_message, to_lingua_value,
};
use crate::shared::{HISTORY_CACHE_NAME, try_send_stream_event};
use crate::{
    AgentConfig, ConversationConfig, ExecutionStreamEvent, ModelClient, ModelRequest,
    ModelResponse, SendRequest, ToolDefinition, ToolRuntime,
};

pub struct BasicExecutor<M, T> {
    model: Arc<M>,
    tools: Arc<T>,
    history_cache: Arc<RwLock<HashMap<ConversationId, HistoryCacheEntry>>>,
    /// Bumped on every cache invalidation. A materialization that started before
    /// an invalidation must not write its now-stale snapshot back afterwards.
    cache_generation: Arc<AtomicU64>,
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
            cache_generation: Arc::new(AtomicU64::new(0)),
            pricing,
        }
    }
}

impl<M, T> BasicExecutor<M, T> {
    /// Drop a conversation's cached history so the next materialization rebuilds
    /// from the log. Compaction replaces exactly the prefix this cache holds, so
    /// skipping this would keep serving pre-compaction history from memory: the
    /// prompt would never shrink and nothing would error.
    pub(crate) fn invalidate_history_cache(&self, conversation_id: ConversationId) {
        let mut cache = self.history_cache.write().expect(HISTORY_CACHE_NAME);
        cache.remove(&conversation_id);
        // Bump while still holding the write lock, so no reader can observe the
        // removal without also observing the new generation.
        self.cache_generation.fetch_add(1, Ordering::AcqRel);
    }
}

impl<M, T> Clone for BasicExecutor<M, T> {
    fn clone(&self) -> Self {
        Self {
            model: Arc::clone(&self.model),
            tools: Arc::clone(&self.tools),
            history_cache: Arc::clone(&self.history_cache),
            cache_generation: Arc::clone(&self.cache_generation),
            pricing: Arc::clone(&self.pricing),
        }
    }
}

/// Everything the compaction trigger weighs, gathered at the call site.
pub(crate) struct CompactionTrigger<'a> {
    /// Already-resolved provider model id, for the price-table lookup.
    pub(crate) model: &'a str,
    pub(crate) max_input_tokens: Option<i64>,
    /// Provider-reported input occupancy, when a response has come back.
    pub(crate) prompt_tokens: Option<u64>,
    /// Serialized size of the request, messages and tool schemas together.
    pub(crate) prompt_size: PromptSize,
    /// Round index and trace sink for the summarizer's own model call. It is a
    /// real, billable request whose output shapes every later prompt, so it
    /// belongs in the same trace as the round that triggered it.
    pub(crate) round: usize,
    pub(crate) turn_trace: Option<&'a dyn TurnExecutionTrace>,
}

/// Where a summarizer request is sent, and where it is recorded.
struct SummarizerCall<'a> {
    /// Model binding name, for credentials and base URL.
    binding: &'a str,
    /// Already-resolved model id, which may be the summary model or the agent's.
    model: &'a str,
    round: usize,
    turn_trace: Option<&'a dyn TurnExecutionTrace>,
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
    pub(crate) async fn materialize_prompt_history(
        &self,
        conversation: &dyn ConversationHandle,
        instructions: &[Message],
    ) -> Result<Vec<Message>> {
        let conversation_id = conversation.record().id;
        // Sampled with the read, and re-checked before the write below. Turns on
        // one conversation are not serialized, so without this a turn that read
        // a pre-checkpoint entry, then blocked in `get_events` while another turn
        // compacted and invalidated, would write its stale full-history snapshot
        // back over the invalidation — and every later prompt would keep
        // replaying the compacted prefix, silently and indefinitely.
        let generation = self.cache_generation.load(Ordering::Acquire);
        let cached_entry = {
            let cache = self.history_cache.read().expect(HISTORY_CACHE_NAME);
            cache.get(&conversation_id).cloned()
        };

        // Re-read the active checkpoint every time, warm cache or not.
        //
        // `cache_generation` only counts *this* executor instance's own
        // compactions. A checkpoint written by another instance, or by the
        // TypeScript runtime over the same conversation, bumps nothing here —
        // and the incremental query below filters custom events out, so a warm
        // entry would never see it. The cache would then replay the compacted
        // prefix from this instance forever. One bounded `desc limit 1` query
        // against a scan the round is doing anyway; the same check
        // `PromptHistoryCache` makes on the TypeScript side.
        let active = read_active_checkpoint(conversation).await?;
        let active_checkpoint_id = active.as_ref().map(|(event_id, _)| *event_id);

        // An entry built against a different checkpoint — or against none — is
        // describing a prompt that no longer exists. Rebuild rather than extend.
        let cached_entry = match &cached_entry {
            Some(entry) if entry.checkpoint_event_id == active_checkpoint_id => cached_entry,
            _ => None,
        };

        let summary = match &cached_entry {
            Some(entry) => entry.summary.clone(),
            // A checkpoint whose artifact has vanished is worse than none: it
            // would cut history out with nothing standing in for it. Fall back
            // to the full replay instead.
            //
            // A read that *errors* is treated identically, not propagated. The
            // raw log is intact and materializing from it is always possible, so
            // failing the turn over an unreadable summary would take a working
            // conversation down over a recoverable artifact-store problem — and
            // every later turn would consult the same checkpoint and fail the
            // same way. This is what `readCheckpointSummary` already does in the
            // TypeScript harness.
            None => match active {
                Some((_, checkpoint)) => read_summary_or_fall_back(conversation, &checkpoint)
                    .await
                    .map(|summary| CachedSummary {
                        text: summary,
                        up_to_event_id: checkpoint.up_to_event_id,
                    }),
                None => None,
            },
        };

        let cursor = match &cached_entry {
            Some(entry) => entry.cursor,
            None => summary.as_ref().map(|summary| summary.up_to_event_id),
        };

        let result = conversation
            .get_events(Some(EventQuery {
                cursor,
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
        let cursor = result.cursor.or(cursor);

        {
            let mut cache = self.history_cache.write().expect(HISTORY_CACHE_NAME);
            // Only publish if nothing invalidated the cache while this read was
            // in flight. Dropping the entry costs one rebuild; keeping a stale
            // one costs correctness.
            if self.cache_generation.load(Ordering::Acquire) == generation {
                cache.insert(
                    conversation_id,
                    HistoryCacheEntry {
                        cursor,
                        messages: event_messages.clone(),
                        tool_call_names,
                        summary: summary.clone(),
                        // Tracked even when the summary was unreadable and we
                        // fell back to the full log: re-priming every round
                        // would defeat the cache entirely.
                        checkpoint_event_id: active_checkpoint_id,
                    },
                );
            }
        }

        if let Some(summary) = summary {
            event_messages.insert(0, summary_message(&summary.text));
        }

        let mut messages = instructions.to_vec();
        messages.extend(event_messages);
        Ok(messages)
    }

    /// Compact if the prompt has grown past the configured share of the model's
    /// input limit. Deliberately infallible: compaction is housekeeping, and a
    /// summarizer outage should leave an oversized prompt rather than kill the
    /// turn. `run_compaction` records its own failure events.
    pub(crate) async fn maybe_compact(
        &self,
        conversation: &dyn ConversationHandle,
        turn: &dyn TurnHandle,
        agent_config: &AgentConfig,
        trigger: CompactionTrigger<'_>,
        latch: &mut CompactionLatch,
    ) -> bool {
        let CompactionTrigger {
            model,
            max_input_tokens,
            prompt_tokens,
            prompt_size,
            round,
            turn_trace,
        } = trigger;
        let config = agent_config.compaction.clone().unwrap_or_default();
        if !should_compact(
            &config,
            prompt_tokens,
            max_input_tokens,
            prompt_size.bytes(),
        ) {
            return false;
        }

        // Re-attempting within a turn costs a log scan and possibly a
        // summarizer call, so it needs a reason. The reason is a turn boundary
        // that was not there last time: cuts land only on `TurnEnded`, so while
        // the newest one is unchanged a re-scan reaches the same answer. Turns
        // are not serialized, so other turns do finish while this one loops —
        // which is exactly the case a plain once-per-turn flag got wrong,
        // suppressing every later check after one early "not enough completed
        // turns to cut".
        let latest_turn_ended = match read_latest_turn_ended(conversation).await {
            Ok(latest) => latest,
            Err(error) => {
                tracing::warn!(%error, "compaction: could not read the latest turn boundary");
                return false;
            }
        };
        if latch.is_settled(latest_turn_ended) {
            return false;
        }
        latch.mark_attempted(latest_turn_ended);

        // `summary_model` overrides the model id within the agent's existing
        // binding, so a cheaper model from the same provider costs no extra
        // configuration. `model` here is the already-resolved provider id.
        let summary_model = config
            .summary_model
            .clone()
            .unwrap_or_else(|| model.to_string());
        // A configured summary model can have a smaller input window than the
        // agent's; when the prompt does not fit it, summarize with the agent's
        // model rather than losing the compaction to a rejected request.
        let summary_model_input_limit = self.pricing.max_input_tokens(&summary_model);
        let summary_model = resolve_summarizer_model(
            summary_model,
            model,
            summary_model_input_limit,
            max_input_tokens,
            // Provider counts when a response has come back; the pessimistic
            // char estimate otherwise, the same input the trigger just used.
            prompt_tokens.unwrap_or_else(|| prompt_size.estimated_tokens()),
        );
        // Collected during the summarizer call and written below. Safe to write
        // right here, mid-round: it goes on a custom event, which prompt
        // assembly ignores outright. See `COMPACTION_USAGE_EVENT`.
        let summarizer_usage: std::sync::Mutex<Option<Box<UsageRecord>>> =
            std::sync::Mutex::new(None);
        let outcome = run_compaction(
            conversation,
            turn,
            &config,
            &summary_model,
            prompt_tokens,
            &|input| {
                Box::pin(self.summarize(
                    input,
                    conversation,
                    &summarizer_usage,
                    SummarizerCall {
                        binding: &agent_config.model,
                        model: &summary_model,
                        round,
                        turn_trace,
                    },
                ))
            },
        )
        .await;
        let usage = summarizer_usage
            .lock()
            .expect("summarizer usage poisoned")
            .take();
        record_summarizer_usage(turn, usage).await;

        let conversation_id = conversation.record().id;
        match outcome {
            CompactionOutcome::Compacted { checkpoint } => {
                tracing::info!(
                    %conversation_id,
                    compacted_events = checkpoint.compacted_event_count,
                    summary_chars = checkpoint.summary_chars,
                    "compacted conversation history"
                );
                // The cache holds exactly the prefix that was just replaced.
                self.invalidate_history_cache(conversation_id);
                true
            }
            // The prompt crossed the threshold but nothing was compacted, so it
            // will cross again next round. Both cases are worth seeing in logs:
            // they are why a conversation's context stops shrinking.
            CompactionOutcome::Skipped { reason } => {
                tracing::debug!(%conversation_id, %reason, "compaction skipped");
                false
            }
            CompactionOutcome::Failed { error } => {
                tracing::warn!(%conversation_id, %error, "compaction failed");
                false
            }
        }
    }

    /// Summarize a compacted span with a model call carrying no tools.
    ///
    /// Credentials come from the agent's resolved model binding; `model`
    /// overrides only the model id within it. Building this request by hand
    /// would drop the API key and base URL and fail auth against every real
    /// provider — and because compaction failures are deliberately non-fatal,
    /// the only symptom would be compaction silently never working.
    async fn summarize(
        &self,
        input: SummarizeInput,
        conversation: &dyn ConversationHandle,
        usage_sink: &std::sync::Mutex<Option<Box<UsageRecord>>>,
        call: SummarizerCall<'_>,
    ) -> Result<String> {
        let SummarizerCall {
            binding,
            model,
            round,
            turn_trace,
        } = call;
        let model_binding = resolve_model_binding(conversation, binding).await?;
        let instruction = summarizer_instruction(&input);
        let SummarizeInput {
            messages: span,
            previous_summary,
            max_chars,
        } = input;
        let mut messages = vec![system_message(&instruction)];
        // Ahead of the span, delimited, at user priority — deliberately not
        // spliced into the instruction. See `previous_summary_message`.
        if let Some(previous) = previous_summary {
            messages.push(previous_summary_message(&previous));
        }
        messages.extend(span);

        // Through `complete_model_round` rather than `ModelClient::complete`
        // directly. That is the only path that opens an LLM trace span, and the
        // only one that fills `response.model` from the request when a provider
        // does not echo it back — without which `build_usage_record` has no
        // model to look up in the price table and files this call's cost under
        // an empty string. Streaming is off: nobody is watching a summary being
        // written, and the turn's own stream should not carry it.
        let response = self
            .complete_model_round(
                ModelRequest {
                    model: model.to_string(),
                    api_key: model_binding.api_key,
                    base_url: model_binding.base_url,
                    messages,
                    // No tools: the summarizer reads, it does not act.
                    tools: Vec::new(),
                    // Bound the response at request time. `cap_summary`
                    // truncates only after generation, so without this a
                    // runaway summary is paid for in full before being thrown
                    // away.
                    max_output_tokens: Some(summarizer_max_output_tokens(max_chars)),
                },
                round,
                ExecutorStreamMode::Disabled,
                turn_trace,
            )
            .await?;
        let text = assistant_messages_text(&response.messages);
        *usage_sink.lock().expect("summarizer usage poisoned") =
            build_usage_record(&response, &self.pricing);
        Ok(text)
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
        let mut compaction_latch = CompactionLatch::default();
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
            let mut request =
                build_model_request(conversation, agent_config, conversation_config, messages)
                    .await?;
            let model = request.model.clone();
            let max_input_tokens = self.pricing.max_input_tokens(&model);
            // Tools count too: they ride in the same request and consume the
            // same window, and a harness can register a lot of them.
            let prompt_size = prompt_size(&request.messages) + tool_definition_size(&request.tools);

            // Compact *before* sending when the prompt already looks too large.
            //
            // The post-response trigger below is more accurate — it uses the
            // provider's own counts — but it only ever runs after a successful
            // call. A prompt that is already past the model's hard limit is
            // rejected outright, and that error propagates out of the turn
            // before anything can shrink the history responsible for it. Every
            // later turn then replays the same oversized log and fails the same
            // way, with no path back. This check is what makes that state
            // recoverable; the character estimate is deliberately pessimistic
            // because failing to fire here is far more costly than firing early.
            if self
                .maybe_compact(
                    conversation,
                    turn.as_ref(),
                    agent_config,
                    CompactionTrigger {
                        model: &model,
                        max_input_tokens,
                        prompt_tokens: Some(prompt_size.estimated_tokens()),
                        prompt_size,
                        round: round as usize,
                        turn_trace,
                    },
                    &mut compaction_latch,
                )
                .await
            {
                // The checkpoint just written replaces the prefix this prompt
                // was built from, so rebuild it before sending.
                let messages = self
                    .materialize_prompt_history(conversation, &agent_config.instructions)
                    .await?;
                request =
                    build_model_request(conversation, agent_config, conversation_config, messages)
                        .await?;
            }

            let response = self
                .complete_model_round(request, round as usize, stream_mode, turn_trace)
                .await?;
            // Occupancy, not `prompt_tokens`: on Anthropic-family providers the
            // latter counts only the fresh slice, so a heavily cached prompt
            // that fills the window reports a tiny number and would never trip
            // the threshold.
            let prompt_tokens = response.usage.as_ref().and_then(|usage| {
                self.pricing.input_occupancy(
                    &model,
                    TokenCounts {
                        prompt: usage.prompt_tokens,
                        completion: usage.completion_tokens,
                        prompt_cached: usage.prompt_cached_tokens,
                        prompt_cache_creation: usage.prompt_cache_creation_tokens,
                    },
                )
            });

            let events = interpret_model_response(response, &self.pricing);
            turn.add_events(events.clone()).await?;

            // Compact between rounds using the token count the provider just
            // reported, so a single runaway turn can bring its own prompt back
            // under the limit rather than failing every turn from here on.
            // Whether it fired does not matter here: the next round rebuilds the
            // prompt from scratch and will pick up any checkpoint written.
            self.maybe_compact(
                conversation,
                turn.as_ref(),
                agent_config,
                CompactionTrigger {
                    model: &model,
                    max_input_tokens,
                    prompt_tokens,
                    prompt_size,
                    round: round as usize,
                    turn_trace,
                },
                &mut compaction_latch,
            )
            .await;

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

pub(crate) fn extend_message_history(
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

pub(crate) fn build_usage_record(
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
        api_key: model_binding.api_key,
        base_url: model_binding.base_url,
        messages,
        tools: build_tool_definitions(conversation_config),
        max_output_tokens: agent_config.max_output_tokens,
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
    /// Summary standing in for the compacted prefix, if the conversation has
    /// been compacted. Cached alongside the messages so a warm cache does not
    /// re-read the checkpoint artifact on every round.
    summary: Option<CachedSummary>,
    /// Which checkpoint this entry was built against, so a checkpoint written
    /// by anyone else — another executor instance, the TypeScript runtime —
    /// invalidates it. `None` means "built with no checkpoint active".
    checkpoint_event_id: Option<EventId>,
}

#[derive(Debug, Clone)]
struct CachedSummary {
    text: String,
    up_to_event_id: EventId,
}

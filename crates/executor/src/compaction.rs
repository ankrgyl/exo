//! Conversation compaction policy for the Rust executors.
//!
//! A conversation's durable event log grows without bound, but a prompt cannot.
//! Compaction bridges the two by writing a *checkpoint*: a custom event
//! recording that everything up to some event id is now represented by a summary
//! artifact. Prompt assembly then reads `instructions + summary + events after
//! the checkpoint` instead of replaying the whole log. The log itself is never
//! mutated, so history stays queryable and forking/time travel keep working.
//!
//! This module is the pure half: cut-point selection, the trigger predicate, and
//! the checkpoint payload. It mirrors `typescript/harness/compaction.ts` so both
//! executors agree on the on-disk format and can read each other's checkpoints.

use std::collections::HashMap;

use anyhow::anyhow;
use exoharness::{
    ConversationHandle, Event, EventData, EventId, EventKind, EventQuery, EventQueryDirection,
    ReadArtifactRequest, Result, TurnHandle, WriteArtifactRequest,
};
use futures::future::BoxFuture;
use lingua::Message;
use serde::{Deserialize, Serialize};

use crate::basic::extend_message_history;

pub(crate) const COMPACTION_CHECKPOINT_EVENT: &str = "exo.compaction.v1";
pub(crate) const COMPACTION_FAILED_EVENT: &str = "exo.compaction.failed.v1";

/// Marker appended when compaction runs, pointing at the summary artifact.
///
/// Field names are snake_case on the wire and match the TypeScript harness
/// exactly; the two implementations must stay interchangeable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactionCheckpoint {
    /// Inclusive: retained history is everything strictly after this id.
    pub up_to_event_id: EventId,
    /// Read directly by id; the alternative is a list_artifacts scan per round.
    pub artifact_id: exoharness::ArtifactId,
    pub artifact_path: String,
    pub artifact_version: u64,
    /// Previous checkpoint in the chain, for auditing.
    #[serde(default)]
    pub previous_checkpoint_id: Option<EventId>,
    pub compacted_event_count: u64,
    pub summary_chars: u64,
    #[serde(default)]
    pub prompt_tokens_before: Option<u64>,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    pub enabled: bool,
    /// Compact once the prompt exceeds this fraction of the model input limit.
    pub threshold_ratio: f64,
    /// Turns kept verbatim after the cut.
    pub keep_recent_turns: u32,
    /// Hard ceiling on summary size; the model is not trusted to respect it.
    pub max_summary_chars: u32,
    pub summary_model: Option<String>,
    /// Used when the price table has no input limit for the model.
    pub fallback_char_budget: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_ratio: 0.7,
            keep_recent_turns: 3,
            max_summary_chars: 8_000,
            summary_model: None,
            fallback_char_budget: 400_000,
        }
    }
}

impl CompactionConfig {
    /// A ratio of zero or less would compact on every round; above one would
    /// never fire before the provider rejects the request. Clamp rather than
    /// error: a bad knob should degrade to the default, not brick the agent.
    pub(crate) fn effective_threshold_ratio(&self) -> f64 {
        if !self.threshold_ratio.is_finite() || self.threshold_ratio <= 0.0 {
            return Self::default().threshold_ratio;
        }
        self.threshold_ratio.min(1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CutPoint {
    /// Inclusive id of the last event folded into the summary.
    pub up_to_event_id: EventId,
    pub compacted_event_count: u64,
}

/// Choose where to cut, or `None` when the conversation is too short to bother.
///
/// Cuts land only on `TurnEnded` boundaries. That is what makes compaction safe:
/// at a turn boundary no tool call is outstanding, so no `ToolRequested` can be
/// separated from its `ToolResult`. Splitting a tool round would make
/// `extend_message_history` either fabricate a failure for a call that actually
/// succeeded or silently drop a result — both corrupt the model's view.
///
/// `events` must be the ascending stream including `TurnEnded` markers.
pub(crate) fn select_cut_point(events: &[Event], keep_recent_turns: u32) -> Option<CutPoint> {
    let keep = keep_recent_turns as usize;
    let boundaries: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.data, EventData::TurnEnded))
        .map(|(index, _)| index)
        .collect();
    // Need one boundary to cut at plus `keep` completed turns to leave behind.
    if boundaries.len() <= keep {
        return None;
    }

    // Walk candidates newest-first from the deepest legal one. `TurnEnded`
    // should already guarantee no pending tool call, but a log truncated by a
    // crash can violate that; fall back to an earlier boundary rather than
    // emit an unsafe cut.
    for candidate in (0..=(boundaries.len() - 1 - keep)).rev() {
        let index = boundaries[candidate];
        if !has_pending_tool_call(&events[..=index]) {
            return Some(CutPoint {
                up_to_event_id: events[index].id,
                compacted_event_count: (index + 1) as u64,
            });
        }
    }
    None
}

/// True when some `ToolRequested` in `events` has no matching `ToolResult`.
fn has_pending_tool_call(events: &[Event]) -> bool {
    let mut pending: Vec<&str> = Vec::new();
    for event in events {
        match &event.data {
            EventData::ToolRequested { tool_call_id, .. } => pending.push(tool_call_id.as_str()),
            EventData::ToolResult { tool_call_id, .. } => {
                pending.retain(|id| *id != tool_call_id.as_str());
            }
            _ => {}
        }
    }
    !pending.is_empty()
}

/// Trigger predicate. Prefers the provider's own `prompt_tokens` against the
/// model's input limit — no client-side tokenizer needed, and it reflects what
/// the provider actually counted. Falls back to a character budget when either
/// number is unavailable, since the price table is fetched over the network and
/// is explicitly best-effort.
pub(crate) fn should_compact(
    config: &CompactionConfig,
    prompt_tokens: Option<u64>,
    max_input_tokens: Option<i64>,
    materialized_chars: u64,
) -> bool {
    if !config.enabled {
        return false;
    }
    if let (Some(prompt_tokens), Some(max_input_tokens)) = (prompt_tokens, max_input_tokens)
        && max_input_tokens > 0
    {
        return (prompt_tokens as f64)
            > config.effective_threshold_ratio() * max_input_tokens as f64;
    }
    materialized_chars > config.fallback_char_budget
}

/// Enforce the summary ceiling. Chained compaction feeds each summary back into
/// the next one, so without a hard cap the summary itself grows without bound —
/// the classic way this design rots. Truncation is deliberately blunt: the model
/// gets one chance to respect the cap, and this is the backstop.
pub(crate) fn cap_summary(summary: &str, max_chars: u32) -> String {
    let max_chars = max_chars as usize;
    let trimmed = summary.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    const MARKER: &str = "\n...[summary truncated]";
    let head_chars = max_chars.saturating_sub(MARKER.chars().count());
    let head: String = trimmed.chars().take(head_chars).collect();
    format!("{head}{MARKER}").chars().take(max_chars).collect()
}

// --- running a compaction ----------------------------------------------------

/// Input handed to the summarizer.
#[derive(Debug, Clone)]
pub(crate) struct SummarizeInput {
    /// Messages being folded into the summary — the compacted span only.
    pub messages: Vec<Message>,
    /// Summary from the previous checkpoint, to be merged rather than replaced.
    pub previous_summary: Option<String>,
    pub max_chars: u32,
}

/// Produces the summary text. Taken as a callback rather than called directly
/// so the orchestration below is testable without a model, and so callers can
/// point it at a cheaper model than the one running the conversation.
pub(crate) type Summarizer<'a> =
    dyn Fn(SummarizeInput) -> BoxFuture<'a, Result<String>> + Send + Sync + 'a;

#[derive(Debug)]
pub(crate) enum CompactionOutcome {
    Compacted {
        checkpoint: Box<CompactionCheckpoint>,
    },
    Skipped {
        reason: String,
    },
    Failed {
        error: String,
    },
}

/// Fold a conversation's older history into a summary checkpoint.
///
/// Nothing here is allowed to fail the caller's turn. Compaction is a
/// housekeeping step; if the summarizer is down or the artifact store rejects a
/// write, the right outcome is an oversized prompt (today's behaviour) rather
/// than a dead conversation. Failures are recorded as an event so the agent can
/// see why its context never shrank.
pub(crate) async fn run_compaction(
    conversation: &dyn ConversationHandle,
    turn: &dyn TurnHandle,
    config: &CompactionConfig,
    model: &str,
    prompt_tokens_before: Option<u64>,
    summarize: &Summarizer<'_>,
) -> CompactionOutcome {
    match compact(
        conversation,
        turn,
        config,
        model,
        prompt_tokens_before,
        summarize,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            let error = error.to_string();
            record_failure(turn, &error).await;
            CompactionOutcome::Failed { error }
        }
    }
}

async fn compact(
    conversation: &dyn ConversationHandle,
    turn: &dyn TurnHandle,
    config: &CompactionConfig,
    model: &str,
    prompt_tokens_before: Option<u64>,
    summarize: &Summarizer<'_>,
) -> Result<CompactionOutcome> {
    let previous = read_active_checkpoint(conversation).await?;
    let previous_summary = match &previous {
        Some(previous) => read_summary(conversation, previous).await?,
        None => None,
    };

    // Only look at events after the last checkpoint: everything before it is
    // already represented by `previous_summary`.
    let scan = conversation
        .get_events(Some(EventQuery {
            cursor: previous.as_ref().map(|previous| previous.up_to_event_id),
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![
                EventKind::MESSAGES,
                EventKind::TOOL_REQUESTED,
                EventKind::TOOL_RESULT,
                EventKind::TURN_ENDED,
            ]),
        }))
        .await?;

    let Some(cut) = select_cut_point(&scan.events, config.keep_recent_turns) else {
        return Ok(CompactionOutcome::Skipped {
            reason: "not enough completed turns to cut".to_string(),
        });
    };

    let cut_index = scan
        .events
        .iter()
        .position(|event| event.id == cut.up_to_event_id)
        .ok_or_else(|| anyhow!("cut point is not in the scanned events"))?;
    let mut messages = Vec::new();
    let mut tool_call_names = HashMap::new();
    extend_message_history(
        &mut messages,
        &mut tool_call_names,
        &scan.events[..=cut_index],
    );

    let summarized = summarize(SummarizeInput {
        messages,
        previous_summary,
        max_chars: config.max_summary_chars,
    })
    .await?;

    let summary = cap_summary(&summarized, config.max_summary_chars);
    if summary.is_empty() {
        // Checkpointing an empty summary would drop real history and put
        // nothing in its place — strictly worse than an oversized prompt.
        let error = "summarizer returned an empty summary".to_string();
        record_failure(turn, &error).await;
        return Ok(CompactionOutcome::Failed { error });
    }

    let written = conversation
        .write_artifact(WriteArtifactRequest {
            path: summary_artifact_path(conversation),
            contents: summary.clone().into_bytes(),
        })
        .await?;

    let checkpoint = CompactionCheckpoint {
        up_to_event_id: cut.up_to_event_id,
        artifact_id: written.artifact_id,
        artifact_path: written.path,
        artifact_version: written.version,
        previous_checkpoint_id: previous.map(|previous| previous.up_to_event_id),
        compacted_event_count: cut.compacted_event_count,
        summary_chars: summary.chars().count() as u64,
        prompt_tokens_before,
        model: model.to_string(),
    };
    turn.add_events(vec![EventData::Custom {
        event_type: COMPACTION_CHECKPOINT_EVENT.to_string(),
        payload: serde_json::to_value(&checkpoint)?,
    }])
    .await?;
    Ok(CompactionOutcome::Compacted {
        checkpoint: Box::new(checkpoint),
    })
}

/// The newest checkpoint, or `None` if this conversation was never compacted.
/// One bounded `desc` query rather than a scan of the whole log.
pub(crate) async fn read_active_checkpoint(
    conversation: &dyn ConversationHandle,
) -> Result<Option<CompactionCheckpoint>> {
    let result = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(1),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::custom(COMPACTION_CHECKPOINT_EVENT)]),
        }))
        .await?;
    let Some(event) = result.events.into_iter().next() else {
        return Ok(None);
    };
    let EventData::Custom { payload, .. } = event.data else {
        return Ok(None);
    };
    // A malformed checkpoint is treated as absent: falling back to full history
    // is safe, whereas half-reading one would assemble a prompt with a hole.
    Ok(serde_json::from_value(payload).ok())
}

/// Summary text for a checkpoint, or `None` if the artifact has gone missing.
pub(crate) async fn read_summary(
    conversation: &dyn ConversationHandle,
    checkpoint: &CompactionCheckpoint,
) -> Result<Option<String>> {
    let artifact = conversation
        .read_artifact(ReadArtifactRequest {
            artifact_id: checkpoint.artifact_id,
            version: Some(checkpoint.artifact_version),
        })
        .await?;
    Ok(artifact.and_then(|artifact| String::from_utf8(artifact.contents).ok()))
}

/// How a summary is presented to the model. A system message so it reads as
/// context the harness supplied, not as something the user said.
pub(crate) fn summary_message(summary: &str) -> Message {
    crate::harness_helpers::system_message(&format!(
        "Summary of earlier conversation history that has been compacted out of this prompt. \
Treat it as an accurate record of what happened before. The full raw history is still \
available through the conversation event log if you need detail this summary omits.\n\n{summary}"
    ))
}

fn summary_artifact_path(conversation: &dyn ConversationHandle) -> String {
    format!("compaction/{}/summary.md", conversation.record().id)
}

async fn record_failure(turn: &dyn TurnHandle, error: &str) {
    let payload = serde_json::json!({ "error": error });
    if let Err(error) = turn
        .add_events(vec![EventData::Custom {
            event_type: COMPACTION_FAILED_EVENT.to_string(),
            payload,
        }])
        .await
    {
        // Best-effort: recording the failure must not mask the original problem
        // or take the turn down with it.
        tracing::warn!(%error, "failed to record compaction failure event");
    }
}

/// Prompt instruction for the summarizer.
///
/// Ordered by what is most expensive to lose. Specifics (paths, ids, error
/// text) go first among the "preserve" rules because they are exactly what a
/// summary tends to drop and what is hardest to recover afterwards.
pub(crate) fn summarizer_instruction(input: &SummarizeInput) -> String {
    let merge = match &input.previous_summary {
        None => String::new(),
        Some(previous) => format!(
            "\n\nA summary of even earlier history is provided first. Merge it with the new \
material into a single summary covering both — do not simply append, and do not drop facts \
from the earlier summary.\n\n<earlier_summary>\n{previous}\n</earlier_summary>"
        ),
    };
    format!(
        "You are compacting the earlier portion of an agent conversation so it can be dropped \
from the prompt while remaining usable.\n\n\
Write a dense factual summary of what happened. Prioritise, in order:\n\
1. Decisions made and conclusions reached, with the reasoning that led to them.\n\
2. Durable facts about the user, the task, and the environment.\n\
3. Work completed, files or resources changed, and commands that mattered.\n\
4. Open threads: what was in progress, what failed, what was agreed for later.\n\n\
Rules:\n\
- Write in the third person about what \"the user\" and \"the agent\" did.\n\
- Preserve specifics: names, paths, ids, numbers, error messages. Those are what a summary \
usually loses and what is most expensive to lose.\n\
- Do not speculate or add anything not present in the material.\n\
- Do not address the reader or describe the summary itself. Output only the summary.\
{merge}\n\nKeep the summary under {} characters.",
        input.max_chars
    )
}

/// Character size of a prompt, for the fallback trigger when the price table
/// has no input limit for the model.
pub(crate) fn prompt_chars(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|message| serde_json::to_string(message).map(|s| s.len()).unwrap_or(0) as u64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_helpers::user_message;
    use exoharness::{ToolRequest, Uuid7};
    use serde_json::{Map, json};

    fn event(data: EventData) -> Event {
        Event {
            id: Uuid7::now(),
            conversation_id: Uuid7::now(),
            session_id: None,
            turn_id: None,
            created_at: Uuid7::now().timestamp().expect("uuid7 timestamp"),
            data,
        }
    }

    fn messages_event(text: &str) -> Event {
        event(EventData::Messages {
            messages: vec![user_message(text)],
            response_id: None,
            usage: None,
        })
    }

    fn tool_pair(call_id: &str) -> Vec<Event> {
        vec![
            event(EventData::ToolRequested {
                tool_call_id: call_id.to_string(),
                response_id: None,
                request: ToolRequest {
                    function_name: "shell".to_string(),
                    arguments: Map::new(),
                },
            }),
            event(EventData::ToolResult {
                tool_call_id: call_id.to_string(),
                result: json!({ "ok": true }),
            }),
        ]
    }

    /// A complete turn: a message, `tool_rounds` tool pairs, then TurnEnded.
    fn turn(label: &str, tool_rounds: usize) -> Vec<Event> {
        let mut events = vec![messages_event(&format!("turn {label}"))];
        for round in 0..tool_rounds {
            events.extend(tool_pair(&format!("{label}-call-{round}")));
        }
        events.push(event(EventData::TurnEnded));
        events
    }

    /// The invariant compaction exists to protect: a tool_requested and its
    /// tool_result must never land on opposite sides of the cut.
    fn splits_a_tool_round(events: &[Event], up_to: EventId) -> bool {
        let mut compacted: Vec<&str> = Vec::new();
        let mut retained: Vec<&str> = Vec::new();
        let mut seen_cut = false;
        for e in events {
            let call_id = match &e.data {
                EventData::ToolRequested { tool_call_id, .. }
                | EventData::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            };
            if let Some(call_id) = call_id {
                if seen_cut {
                    retained.push(call_id);
                } else {
                    compacted.push(call_id);
                }
            }
            if e.id == up_to {
                seen_cut = true;
            }
        }
        retained.iter().any(|id| compacted.contains(id))
    }

    #[test]
    fn returns_none_without_enough_turns_to_keep() {
        let mut events = turn("a", 1);
        events.extend(turn("b", 0));
        events.extend(turn("c", 2));
        assert!(select_cut_point(&events, 3).is_none());
    }

    #[test]
    fn returns_none_for_an_empty_stream() {
        assert!(select_cut_point(&[], 3).is_none());
    }

    #[test]
    fn keeps_exactly_keep_recent_turns_after_the_cut() {
        let mut events = Vec::new();
        for label in ["a", "b", "c", "d", "e"] {
            events.extend(turn(label, 1));
        }
        let cut = select_cut_point(&events, 3).expect("cut point");
        let cut_index = events
            .iter()
            .position(|e| e.id == cut.up_to_event_id)
            .expect("cut event present");
        let turns_after = events[cut_index + 1..]
            .iter()
            .filter(|e| matches!(e.data, EventData::TurnEnded))
            .count();
        assert_eq!(turns_after, 3);
    }

    #[test]
    fn always_cuts_on_a_turn_boundary() {
        let mut events = turn("a", 2);
        events.extend(turn("b", 2));
        events.extend(turn("c", 2));
        let cut = select_cut_point(&events, 1).expect("cut point");
        let cut_event = events
            .iter()
            .find(|e| e.id == cut.up_to_event_id)
            .expect("cut event present");
        assert!(matches!(cut_event.data, EventData::TurnEnded));
    }

    #[test]
    fn reports_how_many_events_it_compacts() {
        let first = turn("a", 1);
        let expected = first.len() as u64;
        let mut events = first;
        events.extend(turn("b", 1));
        events.extend(turn("c", 1));
        let cut = select_cut_point(&events, 2).expect("cut point");
        assert_eq!(cut.compacted_event_count, expected);
    }

    #[test]
    fn never_splits_a_tool_round() {
        // Deterministic pseudo-random shapes so a failure reproduces exactly.
        let mut state: u32 = 0x5eed;
        let mut next = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            state
        };
        for trial in 0..200 {
            let turn_count = 1 + (next() % 8) as usize;
            let mut events = Vec::new();
            for t in 0..turn_count {
                events.extend(turn(&format!("t{trial}-{t}"), (next() % 4) as usize));
            }
            let keep = next() % 4;
            if let Some(cut) = select_cut_point(&events, keep) {
                assert!(
                    !splits_a_tool_round(&events, cut.up_to_event_id),
                    "trial {trial} split a tool round"
                );
            }
        }
    }

    #[test]
    fn refuses_a_boundary_that_would_strand_an_unfinished_tool_call() {
        // A turn that died mid-tool-call: a request with no result and no
        // TurnEnded. The only safe cut is an earlier boundary.
        let mut events = turn("a", 1);
        events.extend(turn("b", 1));
        events.push(messages_event("turn c"));
        events.push(event(EventData::ToolRequested {
            tool_call_id: "c-orphan".to_string(),
            response_id: None,
            request: ToolRequest {
                function_name: "shell".to_string(),
                arguments: Map::new(),
            },
        }));
        let cut = select_cut_point(&events, 1).expect("cut point");
        assert!(!splits_a_tool_round(&events, cut.up_to_event_id));
    }

    #[test]
    fn should_compact_respects_the_enabled_flag() {
        let config = CompactionConfig {
            enabled: false,
            ..CompactionConfig::default()
        };
        assert!(!should_compact(&config, Some(1_000_000), Some(1_000), 0));
    }

    #[test]
    fn should_compact_fires_past_the_ratio_of_the_input_limit() {
        let config = CompactionConfig {
            threshold_ratio: 0.7,
            ..CompactionConfig::default()
        };
        assert!(!should_compact(&config, Some(69_000), Some(100_000), 0));
        assert!(should_compact(&config, Some(71_000), Some(100_000), 0));
    }

    #[test]
    fn should_compact_falls_back_to_a_char_budget() {
        let config = CompactionConfig {
            fallback_char_budget: 1_000,
            ..CompactionConfig::default()
        };
        assert!(!should_compact(&config, None, None, 999));
        assert!(should_compact(&config, None, None, 1_001));
        // Usage missing but a limit known: still the fallback path.
        assert!(should_compact(&config, None, Some(100_000), 1_001));
    }

    #[test]
    fn threshold_ratio_is_clamped_into_range() {
        let zero = CompactionConfig {
            threshold_ratio: 0.0,
            ..CompactionConfig::default()
        };
        assert_eq!(
            zero.effective_threshold_ratio(),
            CompactionConfig::default().threshold_ratio
        );
        let huge = CompactionConfig {
            threshold_ratio: 5.0,
            ..CompactionConfig::default()
        };
        assert_eq!(huge.effective_threshold_ratio(), 1.0);
    }

    #[test]
    fn cap_summary_leaves_short_input_alone() {
        assert_eq!(cap_summary("short", 100), "short");
    }

    #[test]
    fn chained_summaries_stay_bounded() {
        // The runaway-summary failure mode: each pass feeds the previous
        // summary back in. The cap, not the model, keeps this convergent.
        let mut summary = String::new();
        for round in 0..50 {
            let detail = "detail ".repeat(200);
            summary = cap_summary(&format!("{summary}\nround {round} {detail}"), 8_000);
            assert!(summary.chars().count() <= 8_000);
        }
    }

    #[test]
    fn checkpoint_round_trips_through_json() {
        let checkpoint = CompactionCheckpoint {
            up_to_event_id: Uuid7::now(),
            artifact_id: Uuid7::now(),
            artifact_path: "compaction/conv-1/1.md".to_string(),
            artifact_version: 1,
            previous_checkpoint_id: None,
            compacted_event_count: 12,
            summary_chars: 400,
            prompt_tokens_before: Some(150_000),
            model: "gpt-5.6-terra".to_string(),
        };
        let encoded = serde_json::to_value(&checkpoint).expect("serialize");
        // The wire shape must match the TypeScript harness exactly.
        assert!(encoded.get("up_to_event_id").is_some());
        assert!(encoded.get("artifact_id").is_some());
        assert!(encoded.get("artifact_path").is_some());
        let decoded: CompactionCheckpoint = serde_json::from_value(encoded).expect("deserialize");
        assert_eq!(decoded, checkpoint);
    }
}

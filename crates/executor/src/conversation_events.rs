//! Canonical conversation event log: host writers and the agent-facing read
//! tool. Host components (currently the adapter runner) append
//! `EventData::Custom` records so the exoharness event log stays the single
//! immutable history of what happened to the agent, and
//! `list_conversation_events` lets the agent read that history back.

use anyhow::Result;
use exoharness::{
    AddEventsRequest, ConversationHandle, EventData, EventId, EventKind, EventQuery,
    EventQueryDirection, ToolRequest, ToolResult,
};
use serde::Deserialize;
use serde_json::Value;

/// Custom event written when the adapter runner claims a reboot notice:
/// host services were restarted deliberately, with a reason.
pub const HOST_EVENT_REBOOT: &str = "host_reboot";
/// Custom event written whenever the adapter runner starts. A start without a
/// preceding `host_reboot` implies an unplanned restart (crash or manual).
pub const HOST_EVENT_ADAPTER_RUNNER_STARTED: &str = "adapter_runner_started";
/// Custom event written when the adapter runner claims a drain marker and
/// begins a graceful shutdown.
pub const HOST_EVENT_ADAPTER_RUNNER_DRAINING: &str = "adapter_runner_draining";

const DEFAULT_EVENT_LIMIT: u32 = 50;
const MAX_EVENT_LIMIT: u32 = 200;

/// Event kinds returned by `list_conversation_events` when the caller does not
/// ask for specific kinds: lifecycle and host records, not per-turn traffic
/// (messages, tool calls, stream chunks), which would drown the signal.
fn default_event_kinds() -> Vec<EventKind> {
    vec![
        EventKind::CONVERSATION_CREATED,
        EventKind::CONVERSATION_FORKED,
        EventKind::SESSION_STARTED,
        EventKind::SESSION_ENDED,
        EventKind::ERROR,
        EventKind::SANDBOX_CREATED,
        EventKind::SANDBOX_STARTED,
        EventKind::SANDBOX_STOPPED,
        EventKind::SANDBOX_SNAPSHOTTED,
        EventKind::custom(HOST_EVENT_REBOOT),
        EventKind::custom(HOST_EVENT_ADAPTER_RUNNER_STARTED),
        EventKind::custom(HOST_EVENT_ADAPTER_RUNNER_DRAINING),
        // Compaction is the one thing that removes history from the agent's
        // own prompt, so it belongs in the default lifecycle view rather than
        // behind an explicit query.
        EventKind::custom(crate::compaction::COMPACTION_CHECKPOINT_EVENT),
        EventKind::custom(crate::compaction::COMPACTION_FAILED_EVENT),
    ]
}

/// Appends a host-originated custom event to the conversation's canonical
/// event log. Uses an unconditional append (no expected head), which is safe
/// alongside active turns: `BasicTurnHandle::add_events` re-reads the head
/// under the write lock.
pub async fn record_host_event(
    conversation: &dyn ConversationHandle,
    event_type: &str,
    payload: Value,
) -> Result<()> {
    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::Custom {
                event_type: event_type.to_string(),
                payload,
            }],
        })
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListConversationEventsArguments {
    kinds: Option<Vec<String>>,
    limit: Option<u32>,
    cursor: Option<EventId>,
    direction: Option<EventQueryDirection>,
}

pub async fn execute_list_conversation_events_tool(
    conversation: &dyn ConversationHandle,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ListConversationEventsArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let kinds = match args.kinds {
        Some(kinds) => kinds.into_iter().map(EventKind::custom).collect(),
        None => default_event_kinds(),
    };
    let limit = args
        .limit
        .unwrap_or(DEFAULT_EVENT_LIMIT)
        .clamp(1, MAX_EVENT_LIMIT);
    let result = conversation
        .get_events(Some(EventQuery {
            cursor: args.cursor,
            direction: Some(args.direction.unwrap_or(EventQueryDirection::Desc)),
            limit: Some(limit),
            session_id: None,
            turn_id: None,
            types: Some(kinds),
        }))
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "events": result.events,
        "cursor": result.cursor,
    }))
}

#[cfg(test)]
mod tests {
    use exoharness::{BasicExoHarness, ExoHarness, NewAgentRequest, NewConversationRequest};
    use tempfile::TempDir;

    use super::*;
    use crate::test_support::local_test_config;

    async fn test_conversation(
        tempdir: &TempDir,
    ) -> std::sync::Arc<dyn exoharness::ConversationHandle> {
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
        agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap()
    }

    fn tool_request(arguments: serde_json::Value) -> ToolRequest {
        ToolRequest {
            function_name: "list_conversation_events".to_string(),
            arguments: arguments.as_object().unwrap().clone(),
        }
    }

    #[tokio::test]
    async fn records_and_lists_host_events_newest_first() {
        let tempdir = TempDir::new().unwrap();
        let conversation = test_conversation(&tempdir).await;

        record_host_event(
            conversation.as_ref(),
            HOST_EVENT_REBOOT,
            serde_json::json!({ "reason": "restart-all" }),
        )
        .await
        .unwrap();
        record_host_event(
            conversation.as_ref(),
            HOST_EVENT_ADAPTER_RUNNER_STARTED,
            serde_json::json!({ "adapterCount": 2 }),
        )
        .await
        .unwrap();

        let result = execute_list_conversation_events_tool(
            conversation.as_ref(),
            &tool_request(serde_json::json!({
                "kinds": null,
                "limit": null,
                "cursor": null,
                "direction": null
            })),
        )
        .await
        .unwrap();
        assert_eq!(result["ok"], true);
        let events = result["events"].as_array().unwrap();
        let kinds = events
            .iter()
            .map(|event| event["data"]["event_type"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        // Default kinds include conversation_created; custom host events come
        // first because the listing is newest first.
        assert_eq!(kinds[0], HOST_EVENT_ADAPTER_RUNNER_STARTED);
        assert_eq!(kinds[1], HOST_EVENT_REBOOT);
        assert_eq!(
            events.last().unwrap()["data"]["type"]
                .as_str()
                .unwrap_or_default(),
            "conversation_created"
        );
    }

    #[tokio::test]
    async fn filters_by_explicit_kinds_and_limit() {
        let tempdir = TempDir::new().unwrap();
        let conversation = test_conversation(&tempdir).await;
        for index in 0..3 {
            record_host_event(
                conversation.as_ref(),
                HOST_EVENT_REBOOT,
                serde_json::json!({ "reason": format!("restart-{index}") }),
            )
            .await
            .unwrap();
        }

        let result = execute_list_conversation_events_tool(
            conversation.as_ref(),
            &tool_request(serde_json::json!({
                "kinds": [HOST_EVENT_REBOOT],
                "limit": 2,
                "cursor": null,
                "direction": null
            })),
        )
        .await
        .unwrap();
        let events = result["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["data"]["payload"]["reason"]
                .as_str()
                .unwrap_or_default(),
            "restart-2"
        );
        assert!(result["cursor"].as_str().is_some());
    }
}

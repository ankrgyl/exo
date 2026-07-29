//! Canonical conversation event log: host writers and the agent-facing read
//! tool. Host components (currently the adapter runner) append
//! `EventData::Custom` records so the exoharness event log stays the single
//! immutable history of what happened to the agent, and
//! `list_conversation_events` lets the agent read that history back.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use exoharness::{
    AddEventsRequest, ConversationHandle, EventData, EventId, EventKind, EventQuery,
    EventQueryDirection, ToolRequest, ToolResult,
};
use serde::{Deserialize, Serialize};
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
/// Custom event written when a deferred `rebuild_and_restart_exo` finishes
/// (succeeded or failed). The side-channel outcome file under
/// `.exo/guardian-updates/` is updated first; this event is the durable
/// conversation-log record of the same result.
pub const HOST_EVENT_REBUILD_AND_RESTART: &str = "rebuild_and_restart_exo";

const DEFAULT_EVENT_LIMIT: u32 = 50;
const MAX_EVENT_LIMIT: u32 = 200;

/// Event kinds returned by `list_conversation_events` when the caller does not
/// ask for specific kinds: lifecycle and host records, not per-turn traffic
/// (messages, tool calls, stream chunks), which would drown the signal.
fn default_event_kinds() -> Vec<EventKind> {
    vec![
        EventKind::THREAD_CREATED,
        EventKind::THREAD_FORKED,
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
        EventKind::custom(HOST_EVENT_REBUILD_AND_RESTART),
    ]
}

/// Queued/completed durable record for `rebuild_and_restart_exo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuildUpdateRecord {
    pub update_id: String,
    pub operation: String,
    pub status: String,
    pub requested_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub agent_id: String,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// Reads a queued rebuild outcome and writes the completed status to the same
/// file. Does not touch the conversation event log.
pub fn finalize_rebuild_update_file(
    outcome_path: &Path,
    status: &str,
    exit_code: i32,
    completed_at: &str,
) -> Result<RebuildUpdateRecord> {
    if status != "succeeded" && status != "failed" {
        bail!("rebuild update status must be succeeded or failed, got {status}");
    }
    let contents = fs::read_to_string(outcome_path).map_err(|error| {
        anyhow!(
            "failed to read rebuild outcome {}: {error}",
            outcome_path.display()
        )
    })?;
    let mut record: RebuildUpdateRecord = serde_json::from_str(&contents).map_err(|error| {
        anyhow!(
            "failed to parse rebuild outcome {}: {error}",
            outcome_path.display()
        )
    })?;
    if record.operation != "rebuild_and_restart_exo" {
        bail!(
            "rebuild outcome {} has unexpected operation {}",
            outcome_path.display(),
            record.operation
        );
    }
    record.status = status.to_string();
    record.exit_code = Some(exit_code);
    record.completed_at = Some(completed_at.to_string());
    write_json_atomically(outcome_path, &record)?;
    Ok(record)
}

/// Finalizes the outcome file and appends `rebuild_and_restart_exo` to the
/// conversation event log.
pub async fn complete_rebuild_and_restart_update(
    conversation: &dyn ConversationHandle,
    outcome_path: &Path,
    status: &str,
    exit_code: i32,
    completed_at: &str,
) -> Result<RebuildUpdateRecord> {
    let record = finalize_rebuild_update_file(outcome_path, status, exit_code, completed_at)?;
    record_host_event(
        conversation,
        HOST_EVENT_REBUILD_AND_RESTART,
        serde_json::to_value(&record)?,
    )
    .await?;
    Ok(record)
}

fn write_json_atomically(path: &Path, value: &RebuildUpdateRecord) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("rebuild-update");
    let temporary_path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            parent.join(format!("{file_name}.tmp.{}", std::process::id()))
        }
        _ => PathBuf::from(format!("{file_name}.tmp.{}", std::process::id())),
    };
    fs::write(
        &temporary_path,
        format!("{}\n", serde_json::to_string(value)?),
    )
    .map_err(|error| {
        anyhow!(
            "failed to write temporary rebuild outcome {}: {error}",
            temporary_path.display()
        )
    })?;
    fs::rename(&temporary_path, path).map_err(|error| {
        anyhow!(
            "failed to finalize rebuild outcome {}: {error}",
            path.display()
        )
    })?;
    Ok(())
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
            namespace: None,
            function_name: "list_conversation_events".to_string(),
            arguments: arguments.as_object().unwrap().clone(),
        }
    }

    #[tokio::test]
    async fn completes_rebuild_update_file_and_records_host_event() {
        let tempdir = TempDir::new().unwrap();
        let conversation = test_conversation(&tempdir).await;
        let outcome_path = tempdir.path().join("update.json");
        let queued = RebuildUpdateRecord {
            update_id: "update-1".to_string(),
            operation: "rebuild_and_restart_exo".to_string(),
            status: "queued".to_string(),
            requested_at: "2026-07-28T17:00:00Z".to_string(),
            reason: Some("Add webhook adapter".to_string()),
            agent_id: "agent-id".to_string(),
            conversation_id: conversation.record().id.to_string(),
            agent_slug: Some("agent".to_string()),
            conversation_slug: Some("conversation".to_string()),
            exit_code: None,
            completed_at: None,
        };
        fs::write(
            &outcome_path,
            format!("{}\n", serde_json::to_string(&queued).unwrap()),
        )
        .unwrap();

        let completed = complete_rebuild_and_restart_update(
            conversation.as_ref(),
            &outcome_path,
            "succeeded",
            0,
            "2026-07-28T17:01:00Z",
        )
        .await
        .unwrap();
        assert_eq!(completed.status, "succeeded");
        assert_eq!(completed.exit_code, Some(0));
        assert_eq!(
            completed.completed_at.as_deref(),
            Some("2026-07-28T17:01:00Z")
        );

        let on_disk: RebuildUpdateRecord =
            serde_json::from_str(&fs::read_to_string(&outcome_path).unwrap()).unwrap();
        assert_eq!(on_disk.status, "succeeded");

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
        let events = result["events"].as_array().unwrap();
        assert_eq!(
            events[0]["data"]["event_type"].as_str().unwrap_or_default(),
            HOST_EVENT_REBUILD_AND_RESTART
        );
        assert_eq!(events[0]["data"]["payload"]["updateId"], "update-1");
        assert_eq!(events[0]["data"]["payload"]["status"], "succeeded");
        assert_eq!(
            events[0]["data"]["payload"]["reason"],
            "Add webhook adapter"
        );
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
        // Default kinds include thread_created; custom host events come
        // first because the listing is newest first.
        assert_eq!(kinds[0], HOST_EVENT_ADAPTER_RUNNER_STARTED);
        assert_eq!(kinds[1], HOST_EVENT_REBOOT);
        assert_eq!(
            events.last().unwrap()["data"]["type"]
                .as_str()
                .unwrap_or_default(),
            "thread_created"
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

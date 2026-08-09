use anyhow::{Context, Result, bail};
use exoharness::{AttachSandboxRequest, SandboxAttachment};
use serde::{Deserialize, Serialize};

use crate::HarnessConversation;
use crate::conversation_sandbox::attached_conversation_sandbox;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct TrialRunMetadata {
    request_id: String,
    container_id: String,
}

pub(super) struct PreparedTrialRun {
    request_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TrialCompletionSignal {
    TrialComplete {
        request_id: String,
        #[serde(default)]
        summary: Option<String>,
    },
}

#[derive(Debug, Serialize)]
struct TrialComplete<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: &'a str,
    target: &'a str,
    conversation_id: &'a str,
    summary: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct TrialStarted<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: &'a str,
    target: &'a str,
    conversation_id: &'a str,
}

pub(super) async fn prepare_trial_run(
    conversation: &dyn HarnessConversation,
    metadata: serde_json::Value,
) -> Result<PreparedTrialRun> {
    let metadata = serde_json::from_value::<TrialRunMetadata>(metadata)
        .context("trial_run metadata is invalid")?;
    if metadata.request_id.trim().is_empty() {
        bail!("trial_run request_id must not be empty");
    }
    if metadata.container_id.trim().is_empty() {
        bail!("trial_run container_id must not be empty");
    }

    // Replayed pending requests arrive after adapter restarts. The target is
    // mapped to one dedicated conversation, so its existing attachment is the
    // environment for this same trial and must not be attached a second time.
    if attached_conversation_sandbox(conversation.exoharness_handle().as_ref())
        .await?
        .is_some()
    {
        return Ok(PreparedTrialRun {
            request_id: metadata.request_id,
        });
    }

    conversation
        .exoharness_handle()
        .attach_sandbox(AttachSandboxRequest {
            attachment: SandboxAttachment::DockerContainer {
                container_id: metadata.container_id,
            },
            default_workdir: None,
        })
        .await?;
    Ok(PreparedTrialRun {
        request_id: metadata.request_id,
    })
}

pub(super) fn trial_started(
    prepared: &PreparedTrialRun,
    target: &str,
    conversation_id: &str,
) -> Result<String> {
    Ok(serde_json::to_string(&TrialStarted {
        message_type: "trial_started",
        request_id: &prepared.request_id,
        target,
        conversation_id,
    })?)
}

pub(super) fn finalize_trial_completion(
    text: &str,
    target: &str,
    conversation_id: &str,
) -> Result<String> {
    let completion = serde_json::from_str::<TrialCompletionSignal>(text)
        .context("trial completion must be a JSON object")?;
    let TrialCompletionSignal::TrialComplete {
        request_id,
        summary,
    } = completion;

    // TODO(trial-feedback): Before returning this response, snapshot the active
    // trial container and retain the snapshot with this target. A later
    // trial_feedback request can then create a new sandbox from that snapshot
    // and resume this same conversation.
    Ok(serde_json::to_string(&TrialComplete {
        message_type: "trial_complete",
        request_id: &request_id,
        target,
        conversation_id,
        summary: summary.as_deref(),
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_adds_runtime_owned_routing_fields() {
        let response = finalize_trial_completion(
            r#"{"type":"trial_complete","request_id":"request-1","summary":"done"}"#,
            "trial-1",
            "conversation-1",
        )
        .expect("completion");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).expect("response JSON"),
            serde_json::json!({
                "type": "trial_complete",
                "request_id": "request-1",
                "target": "trial-1",
                "conversation_id": "conversation-1",
                "summary": "done",
            })
        );
    }

    #[test]
    fn started_adds_runtime_owned_conversation_id() {
        let response = trial_started(
            &PreparedTrialRun {
                request_id: "request-1".to_string(),
            },
            "trial-1",
            "conversation-1",
        )
        .expect("started response");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).expect("response JSON"),
            serde_json::json!({
                "type": "trial_started",
                "request_id": "request-1",
                "target": "trial-1",
                "conversation_id": "conversation-1",
            })
        );
    }
}

use anyhow::{Context, Result, bail};
use exoharness::{
    AttachSandboxRequest, ConversationHandle, CreateSandboxFromSnapshotRequest, SandboxAttachment,
};
use serde::{Deserialize, Serialize};

use super::store::AdapterStore;
use super::types::{AdapterTrialRecord, now_ms};
use crate::HarnessConversation;
use crate::conversation_sandbox::attached_conversation_sandbox;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TrialMessageMetadata {
    #[serde(rename = "trial_run")]
    Run {
        request_id: String,
        container_id: String,
    },
    #[serde(rename = "trial_feedback")]
    Feedback { request_id: String },
    #[serde(rename = "trial_cancel")]
    Cancel { request_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrialPhase {
    Run,
    Feedback,
}

pub(super) enum PreparedTrialMessage {
    Started {
        phase: TrialPhase,
        request_id: String,
        sandbox_id: Option<String>,
    },
    Cancelled(String),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TrialCompletionSignal {
    TrialComplete {
        request_id: String,
        #[serde(default)]
        summary: Option<String>,
    },
    FeedbackComplete {
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
    snapshot_id: &'a str,
    summary: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct PhaseComplete<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: &'a str,
    target: &'a str,
    conversation_id: &'a str,
    summary: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct PhaseStarted<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: &'a str,
    target: &'a str,
    conversation_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct TrialCancelled<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: &'a str,
    target: &'a str,
    conversation_id: &'a str,
    snapshot_id: &'a str,
}

pub(super) async fn prepare_trial_message(
    store: &AdapterStore,
    adapter_id: &str,
    target: &str,
    conversation: &dyn HarnessConversation,
    metadata: serde_json::Value,
) -> Result<PreparedTrialMessage> {
    match serde_json::from_value::<TrialMessageMetadata>(metadata)
        .context("trial adapter message metadata is invalid")?
    {
        TrialMessageMetadata::Run {
            request_id,
            container_id,
        } => {
            prepare_trial_run(
                store,
                adapter_id,
                target,
                conversation,
                request_id,
                container_id,
            )
            .await
        }
        TrialMessageMetadata::Feedback { request_id } => {
            prepare_trial_feedback(store, adapter_id, target, conversation, request_id).await
        }
        TrialMessageMetadata::Cancel { request_id } => {
            cancel_trial(store, adapter_id, target, conversation, request_id).await
        }
    }
}

async fn prepare_trial_run(
    store: &AdapterStore,
    adapter_id: &str,
    target: &str,
    conversation: &dyn HarnessConversation,
    request_id: String,
    container_id: String,
) -> Result<PreparedTrialMessage> {
    require_non_empty("trial_run request_id", &request_id)?;
    require_non_empty("trial_run container_id", &container_id)?;
    if let Some(trial) = store.get_trial(adapter_id, target).await? {
        if trial.trial_request_id == request_id {
            bail!("trial target {target} is already complete");
        }
        bail!("trial target {target} was already used by another request");
    }

    if attached_conversation_sandbox(conversation.exoharness_handle().as_ref())
        .await?
        .is_none()
    {
        conversation
            .exoharness_handle()
            .attach_sandbox(AttachSandboxRequest {
                attachment: SandboxAttachment::DockerContainer { container_id },
                default_workdir: None,
            })
            .await?;
    }

    Ok(PreparedTrialMessage::Started {
        phase: TrialPhase::Run,
        request_id,
        sandbox_id: None,
    })
}

async fn prepare_trial_feedback(
    store: &AdapterStore,
    adapter_id: &str,
    target: &str,
    conversation: &dyn HarnessConversation,
    request_id: String,
) -> Result<PreparedTrialMessage> {
    require_non_empty("trial_feedback request_id", &request_id)?;
    let mut trial = store
        .get_trial(adapter_id, target)
        .await?
        .ok_or_else(|| anyhow::anyhow!("trial target {target} has no completed snapshot"))?;
    if trial.conversation_id != conversation.record().id.to_string() {
        bail!("trial target {target} is mapped to a different conversation");
    }
    if trial.feedback_completed {
        bail!("trial target {target} already completed feedback");
    }
    if let Some(active_request_id) = &trial.feedback_request_id
        && active_request_id != &request_id
    {
        bail!("trial target {target} already has an active feedback request");
    }

    let sandbox_id = if let Some(sandbox_id) = &trial.feedback_sandbox_id {
        sandbox_id.clone()
    } else {
        let sandbox_id = conversation
            .exoharness_handle()
            .create_sandbox_from_snapshot(CreateSandboxFromSnapshotRequest {
                snapshot_id: trial.snapshot_id.parse()?,
                idle_seconds: Some(300),
                provider: None,
            })
            .await?;
        trial.feedback_request_id = Some(request_id.clone());
        trial.feedback_sandbox_id = Some(sandbox_id.clone());
        trial.updated_at_ms = now_ms();
        store.put_trial(&trial).await?;
        sandbox_id
    };

    Ok(PreparedTrialMessage::Started {
        phase: TrialPhase::Feedback,
        request_id,
        sandbox_id: Some(sandbox_id),
    })
}

async fn cancel_trial(
    store: &AdapterStore,
    adapter_id: &str,
    target: &str,
    conversation: &dyn HarnessConversation,
    request_id: String,
) -> Result<PreparedTrialMessage> {
    require_non_empty("trial_cancel request_id", &request_id)?;
    let conversation_id = conversation.record().id.to_string();
    let trial = if let Some(trial) = store.get_trial(adapter_id, target).await? {
        if trial.trial_request_id != request_id
            && trial.feedback_request_id.as_deref() != Some(&request_id)
        {
            bail!("trial cancellation does not match the active request");
        }
        if trial.feedback_request_id.as_deref() == Some(&request_id)
            && let Some(sandbox_id) = &trial.feedback_sandbox_id
        {
            conversation
                .exoharness_handle()
                .stop_sandbox(sandbox_id.clone())
                .await?;
        }
        trial
    } else {
        let sandbox_id = attached_conversation_sandbox(conversation.exoharness_handle().as_ref())
            .await?
            .ok_or_else(|| anyhow::anyhow!("cancelled trial has no attached sandbox"))?;
        let snapshot_id = conversation
            .exoharness_handle()
            .snapshot_sandbox(sandbox_id.clone())
            .await?;
        let trial = AdapterTrialRecord {
            adapter_id: adapter_id.to_string(),
            target: target.to_string(),
            trial_request_id: request_id.clone(),
            conversation_id: conversation_id.clone(),
            source_sandbox_id: sandbox_id.clone(),
            snapshot_id: snapshot_id.to_string(),
            feedback_request_id: None,
            feedback_sandbox_id: None,
            feedback_completed: false,
            updated_at_ms: now_ms(),
        };
        store.put_trial(&trial).await?;
        conversation
            .exoharness_handle()
            .detach_sandbox(sandbox_id)
            .await?;
        trial
    };
    Ok(PreparedTrialMessage::Cancelled(serde_json::to_string(
        &TrialCancelled {
            message_type: "trial_cancelled",
            request_id: &request_id,
            target,
            conversation_id: &conversation_id,
            snapshot_id: &trial.snapshot_id,
        },
    )?))
}

pub(super) fn phase_started(
    prepared: &PreparedTrialMessage,
    target: &str,
    conversation_id: &str,
) -> Result<Option<String>> {
    let PreparedTrialMessage::Started {
        phase,
        request_id,
        sandbox_id,
    } = prepared
    else {
        return Ok(None);
    };
    Ok(Some(serde_json::to_string(&PhaseStarted {
        message_type: match phase {
            TrialPhase::Run => "trial_started",
            TrialPhase::Feedback => "feedback_started",
        },
        request_id,
        target,
        conversation_id,
        sandbox_id: sandbox_id.as_deref(),
    })?))
}

pub(super) fn cancellation_response(prepared: &PreparedTrialMessage) -> Option<&str> {
    match prepared {
        PreparedTrialMessage::Cancelled(response) => Some(response),
        PreparedTrialMessage::Started { .. } => None,
    }
}

pub(super) async fn finalize_trial_completion(
    store: &AdapterStore,
    adapter_id: &str,
    conversation: &dyn ConversationHandle,
    text: &str,
    target: &str,
    conversation_id: &str,
) -> Result<String> {
    match serde_json::from_str::<TrialCompletionSignal>(text)
        .context("trial completion must be a JSON object")?
    {
        TrialCompletionSignal::TrialComplete {
            request_id,
            summary,
        } => {
            let trial = if let Some(trial) = store.get_trial(adapter_id, target).await? {
                if trial.trial_request_id != request_id {
                    bail!("trial completion does not match the completed request");
                }
                trial
            } else {
                let sandbox_id = attached_conversation_sandbox(conversation)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("trial has no attached sandbox to snapshot"))?;
                let snapshot_id = conversation.snapshot_sandbox(sandbox_id.clone()).await?;
                let trial = AdapterTrialRecord {
                    adapter_id: adapter_id.to_string(),
                    target: target.to_string(),
                    trial_request_id: request_id.clone(),
                    conversation_id: conversation_id.to_string(),
                    source_sandbox_id: sandbox_id,
                    snapshot_id: snapshot_id.to_string(),
                    feedback_request_id: None,
                    feedback_sandbox_id: None,
                    feedback_completed: false,
                    updated_at_ms: now_ms(),
                };
                store.put_trial(&trial).await?;
                trial
            };
            if attached_conversation_sandbox(conversation)
                .await?
                .as_deref()
                == Some(trial.source_sandbox_id.as_str())
            {
                conversation
                    .detach_sandbox(trial.source_sandbox_id.clone())
                    .await?;
            }
            Ok(serde_json::to_string(&TrialComplete {
                message_type: "trial_complete",
                request_id: &request_id,
                target,
                conversation_id,
                snapshot_id: &trial.snapshot_id,
                summary: summary.as_deref(),
            })?)
        }
        TrialCompletionSignal::FeedbackComplete {
            request_id,
            summary,
        } => {
            let mut trial = store
                .get_trial(adapter_id, target)
                .await?
                .ok_or_else(|| anyhow::anyhow!("trial target {target} has no feedback state"))?;
            if trial.feedback_request_id.as_deref() != Some(&request_id) {
                bail!("feedback completion does not match the active request");
            }
            let sandbox_id = trial
                .feedback_sandbox_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("trial target {target} has no feedback sandbox"))?;
            conversation.stop_sandbox(sandbox_id).await?;
            trial.feedback_completed = true;
            trial.updated_at_ms = now_ms();
            store.put_trial(&trial).await?;
            Ok(serde_json::to_string(&PhaseComplete {
                message_type: "feedback_complete",
                request_id: &request_id,
                target,
                conversation_id,
                summary: summary.as_deref(),
            })?)
        }
    }
}

fn require_non_empty(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn feedback_started_includes_restored_sandbox() {
        let started = phase_started(
            &PreparedTrialMessage::Started {
                phase: TrialPhase::Feedback,
                request_id: "feedback-1".to_string(),
                sandbox_id: Some("sandbox-2".to_string()),
            },
            "trial-1",
            "conversation-1",
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&started).unwrap(),
            json!({
                "type": "feedback_started",
                "request_id": "feedback-1",
                "target": "trial-1",
                "conversation_id": "conversation-1",
                "sandbox_id": "sandbox-2",
            })
        );
    }

    #[test]
    fn trial_metadata_is_typed_by_phase() {
        let feedback = serde_json::from_value::<TrialMessageMetadata>(json!({
            "type": "trial_feedback",
            "request_id": "feedback-1",
        }))
        .unwrap();
        assert!(matches!(feedback, TrialMessageMetadata::Feedback { .. }));

        assert!(
            serde_json::from_value::<TrialMessageMetadata>(json!({
                "type": "trial_feedback",
                "request_id": "feedback-1",
                "container_id": "not-allowed",
            }))
            .is_err()
        );
    }
}

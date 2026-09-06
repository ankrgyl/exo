use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context;
use exoharness::Result;
use lingua::Message;
use lingua::universal::UserContent;
use tokio::sync::Mutex as AsyncMutex;

use crate::inbox::{Inbox, InboxItem};
use crate::{HarnessConversation, SendRequest, SendResult};

/// Default on-disk location of the durable conversation inbox.
///
/// Overridable via `EXO_INBOX_DIR` so deployments can place it on a
/// volume with the same durability guarantees as the event log.
pub fn default_inbox() -> Inbox {
    let root = std::env::var_os("EXO_INBOX_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("exo-inbox"));
    Inbox::new(root)
}

pub async fn send_conversation_wakeup(
    conversation: &dyn HarnessConversation,
    prompt: String,
) -> Result<SendResult> {
    send_conversation_wakeup_content(conversation, UserContent::String(prompt)).await
}

/// Wakeup variant for multimodal content, e.g. adapter messages that carry
/// inbound images for the model to analyze.
///
/// The content is first appended to the conversation's durable inbox and
/// only then injected. This gives producers FIFO delivery (items drain in
/// UUIDv7 arrival order) and crash safety (an unacked item survives a
/// restart and is redelivered), replacing the old race-prone lock-file
/// handoff (issue #207).
pub async fn send_conversation_wakeup_content(
    conversation: &dyn HarnessConversation,
    content: UserContent,
) -> Result<SendResult> {
    let conversation_id = conversation.record().id.to_string();
    let inbox = default_inbox();
    let pending = inbox.enqueue(&conversation_id, content).await?;

    // Serialize turn start across tasks in this process; items drain in
    // arrival order under this per-conversation lock.
    let send_lock = conversation_send_lock(&conversation_id);
    let _send_guard = send_lock.lock().await;

    let items = inbox.drain(&conversation_id).await?;
    let mut result = None;
    for item in items {
        let sent = deliver_item(conversation, &item).await?;
        if item.item_id == pending {
            result = Some(sent);
        }
        inbox.ack(&conversation_id, item.item_id).await?;
    }
    match result {
        Some(result) => Ok(result),
        // The just-enqueued item was already drained+acked by another task
        // between our enqueue and drain; treat it as delivered.
        None => Err(anyhow::anyhow!(
            "wakeup item {} was consumed by a concurrent sender",
            pending
        )
        .into()),
    }
}

async fn deliver_item(
    conversation: &dyn HarnessConversation,
    item: &InboxItem,
) -> Result<SendResult> {
    let result = conversation
        .send(SendRequest {
            input: vec![Message::User {
                content: item.content.clone(),
            }],
            session_id: None,
        })
        .await?;
    conversation.close_session(result.session_id).await?;
    Ok(result)
}

pub(crate) fn conversation_send_lock(conversation_id: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .expect("conversation wakeup lock registry poisoned");
    Arc::clone(
        locks
            .entry(conversation_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_lock_serializes_same_conversation_only() {
        let id = "test-lock-conv";
        let first_lock = conversation_send_lock(id);
        let guard = first_lock.lock().await;
        let second = conversation_send_lock(id);

        // A different conversation is never blocked by ours.
        let other_conv = conversation_send_lock("other-conv");
        let other =
            tokio::time::timeout(std::time::Duration::from_millis(20), other_conv.lock()).await;
        assert!(other.is_ok(), "unrelated conversation lock was blocked");

        // The same conversation cannot re-enter while held.
        let reentry =
            tokio::time::timeout(std::time::Duration::from_millis(20), second.lock()).await;
        assert!(
            reentry.is_err(),
            "same conversation acquired lock twice concurrently"
        );

        drop(guard);
        let same_conv = conversation_send_lock(id);
        let again =
            tokio::time::timeout(std::time::Duration::from_secs(1), same_conv.lock()).await;
        assert!(again.is_ok(), "lock was not released after guard drop");
    }
}

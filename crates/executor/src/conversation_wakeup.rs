use std::collections::HashMap;
use std::fs::TryLockError;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use exoharness::Result;
use lingua::Message;
use lingua::universal::UserContent;
use tokio::sync::Mutex as AsyncMutex;

use crate::{HarnessConversation, SendRequest, SendResult};

pub async fn send_conversation_wakeup(
    conversation: &dyn HarnessConversation,
    prompt: String,
) -> Result<SendResult> {
    send_conversation_wakeup_content(conversation, UserContent::String(prompt)).await
}

/// Wakeup variant for multimodal content, e.g. adapter messages that carry
/// inbound images for the model to analyze.
pub async fn send_conversation_wakeup_content(
    conversation: &dyn HarnessConversation,
    content: UserContent,
) -> Result<SendResult> {
    let _file_guard = acquire_wakeup_lock(&conversation.record().id.to_string()).await?;
    let result = conversation
        .send(SendRequest {
            input: vec![Message::User { content }],
            session_id: None,
        })
        .await?;
    conversation.close_session(result.session_id).await?;
    Ok(result)
}

fn wakeup_lock_path(conversation_id: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join("exo-wakeup-locks")
        .join(format!("{conversation_id}.lock"))
}

/// Cross-process exclusion for wakeup turns on one conversation, held as an
/// OS advisory lock on a per-conversation file. The returned file holds the
/// lock until it is dropped.
///
/// The lock lives in the open file description, so the kernel releases it
/// when the holder exits or dies — there is no stale-lock heuristic to get
/// wrong, and a holder whose turn outlives any timeout keeps its exclusivity.
/// The file itself is never removed: unlinking a lock file lets a waiter that
/// already opened the old inode and a newcomer that creates a fresh one both
/// "win", so the (empty, per-conversation) files are left in place.
async fn acquire_wakeup_lock(conversation_id: &str) -> Result<std::fs::File> {
    let path = wakeup_lock_path(conversation_id);
    let dir = path.parent().expect("wakeup lock path has a parent");
    tokio::fs::create_dir_all(dir).await?;
    let file = std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open wakeup lock {}", path.display()))?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("failed to acquire wakeup lock {}", path.display()));
            }
        }
    }
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
    use std::pin::pin;

    use exoharness::Uuid7;

    use super::*;
    use crate::test_support::backdate_file;

    #[tokio::test]
    async fn wakeup_file_lock_serializes_conversation_ids() {
        let conversation_id = format!("test-{}", Uuid7::now());
        let first = acquire_wakeup_lock(&conversation_id).await.unwrap();
        // Backdated past the old 30-minute staleness window: the lock file's
        // age must never let a waiter steal a live holder's lock.
        backdate_file(
            &wakeup_lock_path(&conversation_id),
            Duration::from_secs(31 * 60),
        );
        let mut second = pin!(acquire_wakeup_lock(&conversation_id));

        tokio::select! {
            _ = &mut second => {
                panic!("second lock acquired while first lock was held");
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap();
        drop(second);
    }
}

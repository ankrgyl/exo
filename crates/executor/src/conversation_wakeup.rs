use std::collections::HashMap;
use std::fs::File;
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
    let _file_guard = WakeupFileLock::acquire(&conversation.record().id.to_string()).await?;
    let result = conversation
        .send(SendRequest {
            input: vec![Message::User { content }],
            session_id: None,
        })
        .await?;
    conversation.close_session(result.session_id).await?;
    Ok(result)
}

struct WakeupFileLock {
    // The operating system releases the advisory lock when this handle drops.
    _file: File,
}

impl WakeupFileLock {
    async fn acquire(conversation_id: &str) -> Result<Self> {
        let dir = std::env::temp_dir().join("exo-wakeup-locks");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{conversation_id}.lock"));
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open wakeup lock {}", path.display()))?;
        // Keep the path in place: unlinking it could let waiters lock different
        // inodes and enter the same conversation concurrently.
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error).with_context(|| {
                        format!("failed to acquire wakeup lock {}", path.display())
                    });
                }
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
    use std::time::{Duration, SystemTime};

    use exoharness::Uuid7;

    use super::*;

    #[tokio::test]
    async fn wakeup_file_lock_serializes_live_holders() {
        let conversation_id = format!("test-{}", Uuid7::now());
        let path = std::env::temp_dir()
            .join("exo-wakeup-locks")
            .join(format!("{conversation_id}.lock"));
        let first = WakeupFileLock::acquire(&conversation_id).await.unwrap();
        let stale_time = SystemTime::now() - Duration::from_secs(31 * 60);
        first
            ._file
            .set_times(std::fs::FileTimes::new().set_modified(stale_time))
            .unwrap();
        let mut second = pin!(WakeupFileLock::acquire(&conversation_id));

        tokio::select! {
            _ = &mut second => {
                panic!("second lock stole an old but actively held lock");
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap();
        drop(second);
        std::fs::remove_file(path).unwrap();
    }
}

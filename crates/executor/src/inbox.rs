use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context;
use exoharness::Uuid7;
use lingua::universal::UserContent;
use serde::{Deserialize, Serialize};
use tokio::fs;

/// A durably enqueued message awaiting injection into a conversation.
///
/// Items are stored one JSON file per item under
/// `<root>/<conversation_id>/<uuid7>.json`. Because [`Uuid7`] embeds a
/// millisecond timestamp in its top bits, lexicographic file order equals
/// arrival order, which restores FIFO delivery that the previous lock-file
/// wakeup path could not guarantee (issue #207).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub item_id: Uuid7,
    pub conversation_id: String,
    pub content: UserContent,
}

#[derive(Debug, Clone)]
pub struct Inbox {
    root: PathBuf,
}

impl Inbox {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Durably append an item for `conversation_id`.
    ///
    /// The file is written to a temp name and renamed into place so readers
    /// never observe a partial write. Producers return as soon as this
    /// resolves; delivery happens later during drain.
    pub async fn enqueue(
        &self,
        conversation_id: &str,
        content: UserContent,
    ) -> anyhow::Result<Uuid7> {
        let dir = self.conversation_dir(conversation_id);
        fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("failed to create inbox dir {}", dir.display()))?;

        let item = InboxItem {
            item_id: Uuid7::now(),
            conversation_id: conversation_id.to_string(),
            content,
        };

        let final_path = self.item_path(&dir, item.item_id, "");
        let tmp_path = self.item_path(&dir, item.item_id, ".tmp");
        let json = serde_json::to_vec_pretty(&item)?;

        fs::write(&tmp_path, &json).await.with_context(|| {
            format!("failed to write inbox item {}", tmp_path.display())
        })?;
        fs::rename(&tmp_path, &final_path).await.with_context(|| {
            format!(
                "failed to finalize inbox item {} -> {}",
                tmp_path.display(),
                final_path.display()
            )
        })?;
        Ok(item.item_id)
    }

    /// Return pending items for `conversation_id` in arrival order.
    ///
    /// Items whose `.json.done` marker exists are considered delivered and
    /// are skipped; leftover `.tmp` files from interrupted enqueues are
    /// ignored until their rename completes.
    pub async fn drain(&self, conversation_id: &str) -> anyhow::Result<Vec<InboxItem>> {
        let dir = self.conversation_dir(conversation_id);
        if fs::metadata(&dir).await.is_err() {
            return Ok(Vec::new());
        }

        let mut ids: Vec<Uuid7> = Vec::new();
        let mut entries = fs::read_dir(&dir)
            .await
            .with_context(|| format!("failed to read inbox dir {}", dir.display()))?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            if let Some(id) = parse_pending_item_name(&name.to_string_lossy()) {
                ids.push(id);
            }
        }
        // Uuid7 is time-ordered, so sorting by id sorts by arrival time.
        ids.sort();

        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            let path = self.item_path(&dir, id, "");
            let bytes = match fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).context(format!("failed to read {}", path.display()))
                }
            };
            let item: InboxItem = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse inbox item {}", path.display()))?;
            items.push(item);
        }
        Ok(items)
    }

    /// Mark an item delivered after its content was successfully sent.
    ///
    /// Acknowledgement is a rename to `<id>.json.done`, so a crash between
    /// send and ack simply redelivers the item on the next drain
    /// (at-least-once semantics).
    pub async fn ack(&self, conversation_id: &str, item_id: Uuid7) -> anyhow::Result<()> {
        let dir = self.conversation_dir(conversation_id);
        let from = self.item_path(&dir, item_id, "");
        let to = self.item_path(&dir, item_id, ".done");
        match fs::rename(&from, &to).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context(format!("failed to ack {}", from.display())),
        }
    }

    fn conversation_dir(&self, conversation_id: &str) -> PathBuf {
        self.root.join(sanitize(conversation_id))
    }

    fn item_path(&self, dir: &Path, id: Uuid7, suffix: &str) -> PathBuf {
        dir.join(format!("{id}.json{suffix}"))
    }
}

/// Accept `<uuid>.json`; reject `.tmp`, `.done`, and unrelated files.
fn parse_pending_item_name(name: &str) -> Option<Uuid7> {
    let rest = name.strip_suffix(".json")?;
    Uuid7::from_str(rest).ok()
}

/// Keep conversation ids from escaping the inbox root when used as a dirname.
fn sanitize(conversation_id: &str) -> String {
    conversation_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_inbox() -> (tempfile::TempDir, Inbox) {
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::new(dir.path().join("inbox"));
        (dir, inbox)
    }

    #[tokio::test]
    async fn enqueue_then_drain_returns_items_in_arrival_order() {
        let (_dir, inbox) = temp_inbox();
        for text in ["first", "second", "third"] {
            inbox.enqueue("conv-1", UserContent::String(text.into())).await.unwrap();
            // Tiny sleep keeps millisecond uuid7 timestamps distinct.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        let items = inbox.drain("conv-1").await.unwrap();
        let texts: Vec<&str> = items
            .iter()
            .map(|i| match &i.content {
                UserContent::String(s) => s.as_str(),
                _ => panic!("unexpected content"),
            })
            .collect();
        assert_eq!(texts, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn acked_items_are_not_redelivered() {
        let (_dir, inbox) = temp_inbox();
        let id = inbox.enqueue("conv", UserContent::String("hello".into())).await.unwrap();
        assert_eq!(inbox.drain("conv").await.unwrap().len(), 1);

        inbox.ack("conv", id).await.unwrap();
        assert!(inbox.drain("conv").await.unwrap().is_empty());
        // Acking twice is idempotent.
        inbox.ack("conv", id).await.unwrap();
    }

    #[tokio::test]
    async fn unacked_items_survive_as_new_inbox_instance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("inbox");
        let inbox_a = Inbox::new(&root);
        inbox_a.enqueue("conv", UserContent::String("durable".into())).await.unwrap();

        // Simulate restart: fresh handle over the same root.
        let inbox_b = Inbox::new(root);
        let items = inbox_b.drain("conv").await.unwrap();
        assert_eq!(items.len(), 1);
        match &items[0].content {
            UserContent::String(s) => assert_eq!(s, "durable"),
            _ => panic!("unexpected content"),
        }
    }

    #[tokio::test]
    async fn tmp_files_are_ignored_by_drain() {
        let (_dir, inbox) = temp_inbox();
        inbox.enqueue("conv", UserContent::String("real".into())).await.unwrap();

        let conv_dir = inbox.root.join("conv");
        fs::write(conv_dir.join("00000000-0000-7000-8000-000000000000.json.tmp"), b"junk")
            .await
            .unwrap();

        let items = inbox.drain("conv").await.unwrap();
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn conversations_are_isolated_and_unknown_are_empty() {
        let (_dir, inbox) = temp_inbox();
        inbox.enqueue("a", UserContent::String("for-a".into())).await.unwrap();
        assert_eq!(inbox.drain("b").await.unwrap().len(), 0);
        assert_eq!(inbox.drain("a").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn conversation_ids_with_unsafe_chars_are_sanitized() {
        let (_dir, inbox) = temp_inbox();
        inbox
            .enqueue("../evil", UserContent::String("contained".into()))
            .await
            .unwrap();
        // The escaped id must stay inside the root, not above it.
        assert_eq!(inbox.drain("../evil").await.unwrap().len(), 1);
        assert!(!inbox.root.parent().unwrap().join("evil").exists());
    }
}

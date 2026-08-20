//! File-backed JSON record helpers shared by the scheduler and adapter stores:
//! atomic staged writes, tolerant removes, and cleanup of the staging files a
//! crashed writer leaves behind.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use exoharness::Uuid7;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::fs;

/// Staging files older than this are leftovers of a writer that died between
/// its temp write and its rename; anything younger may still be mid-write.
const STALE_TEMP_FILE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Writes JSON through a temp file so a crash mid-write leaves the previous
/// record intact instead of a half-written one.
pub(crate) async fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temp_path = path.with_extension(format!("json.{}.tmp", Uuid7::now()));
    fs::write(&temp_path, serde_json::to_vec_pretty(value)?)
        .await
        .with_context(|| format!("failed to write temp file {}", temp_path.display()))?;
    if let Err(error) = fs::rename(&temp_path, path).await {
        let context = format!(
            "failed to replace {} with temp file {}",
            path.display(),
            temp_path.display()
        );
        remove_file_if_exists(temp_path).await?;
        return Err(error).context(context);
    }
    Ok(())
}

pub(crate) async fn read_json_file_if_exists<T: DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// True when `entry` is a JSON record. Anything else is removed by
/// [`remove_stale_temp_file`] if it is a leftover staging file, so every
/// directory listing doubles as the sweep for the litter a crashed writer
/// leaves behind; nothing else ever cleans it up.
pub(crate) async fn is_record_entry(entry: &fs::DirEntry) -> bool {
    if entry.path().extension().and_then(|ext| ext.to_str()) == Some("json") {
        return true;
    }
    remove_stale_temp_file(entry).await;
    false
}

/// Removes `entry` if it is a staging file a crashed writer left behind.
/// Best-effort maintenance: a failed removal (or the entry disappearing under
/// a live writer's rename) must not fail the listing that triggered it.
async fn remove_stale_temp_file(entry: &fs::DirEntry) {
    if !entry.file_name().to_str().is_some_and(is_staging_file_name) {
        return;
    }
    let Ok(metadata) = entry.metadata().await else {
        return;
    };
    let is_stale = metadata.is_file()
        && metadata.modified().is_ok_and(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age > STALE_TEMP_FILE_AFTER)
        });
    if is_stale && let Err(error) = fs::remove_file(entry.path()).await {
        tracing::debug!(
            path = %entry.path().display(),
            %error,
            "failed to remove stale staging file"
        );
    }
}

/// Only the exact staging shape `write_json_file` creates
/// (`<name>.json.<uuid>.tmp`); anything else in the directory is not ours to
/// delete.
fn is_staging_file_name(name: &str) -> bool {
    name.strip_suffix(".tmp")
        .and_then(|rest| rest.rsplit_once('.'))
        .is_some_and(|(prefix, uuid)| prefix.ends_with(".json") && uuid.parse::<Uuid7>().is_ok())
}

pub(crate) async fn remove_file_if_exists(path: PathBuf) -> Result<()> {
    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to delete file {}", path.display()))
        }
    }
}

pub(crate) async fn remove_dir_if_exists(path: PathBuf) -> Result<()> {
    match fs::remove_dir_all(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to delete directory {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_name_matches_only_the_writer_shape() {
        assert!(is_staging_file_name(&format!(
            "record.json.{}.tmp",
            Uuid7::now()
        )));
        for name in [
            "record.json",
            "record.json.backup.tmp",
            "record.tmp",
            "archive.tmp",
            "record.json.tmp",
        ] {
            assert!(!is_staging_file_name(name), "{name} is not a staging file");
        }
    }
}

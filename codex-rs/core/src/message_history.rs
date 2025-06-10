//! Persistence layer for the global, append-only *message history* file.
//!
//! The history is stored at `~/.codex/history.jsonl` with **one JSON object per
//! line** so that it can be efficiently appended to and parsed with standard
//! JSON-Lines tooling. Each record has the following schema:
//!
//! ````text
//! {"session_id":"<uuid>","ts":<unix_seconds>,"text":"<message>"}
//! ````
//!
//! To minimise the chance of interleaved writes when multiple processes are
//! appending concurrently, callers should *prepare the full line* (record +
//! trailing `\n`) and write it with a **single `write(2)` system call** while
//! the file descriptor is opened with the `O_APPEND` flag. POSIX guarantees
//! that writes up to `PIPE_BUF` bytes are atomic in that case.

use std::fs::File;
use std::fs::OpenOptions;
use std::io::Result;
use std::io::Write;
use std::path::PathBuf;

use regex_lite::Regex;
use serde::Deserialize;
use serde::Serialize;
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::config::Config;
use crate::config_types::HistoryPersistence;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Filename that stores the message history inside `~/.codex`.
const HISTORY_FILENAME: &str = "history.jsonl";

const MAX_RETRIES: usize = 10;
const RETRY_SLEEP: Duration = Duration::from_millis(100);

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HistoryEntry {
    pub session_id: String,
    pub ts: u64,
    pub text: String,
}

fn history_filepath(config: &Config) -> PathBuf {
    let mut path = config.codex_home.clone();
    path.push(HISTORY_FILENAME);
    path
}

/// Append a `text` entry associated with `session_id` to the history file. Uses
/// advisory file locking to ensure that concurrent writes do not interleave,
/// which entails a small amount of blocking I/O internally.
pub(crate) async fn append_entry(text: &str, session_id: &Uuid, config: &Config) -> Result<()> {
    match config.history.persistence {
        HistoryPersistence::SaveAll => {
            // Save everything: proceed.
        }
        HistoryPersistence::None => {
            // No history persistence requested.
            return Ok(());
        }
    }

    if let Some(patterns) = &config.history.sensitive_patterns {
        for pat in patterns {
            match Regex::new(pat) {
                Ok(re) => {
                    if re.is_match(text) {
                        return Ok(());
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, pattern = %pat, "invalid sensitive regex");
                }
            }
        }
    }

    // Resolve `~/.codex/history.jsonl` and ensure the parent directory exists.
    let path = history_filepath(config);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Compute timestamp (seconds since the Unix epoch).
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| std::io::Error::other(format!("system clock before Unix epoch: {e}")))?
        .as_secs();

    // Construct the JSON line first so we can write it in a single syscall.
    let entry = HistoryEntry {
        session_id: session_id.to_string(),
        ts,
        text: text.to_string(),
    };
    let mut line = serde_json::to_string(&entry)
        .map_err(|e| std::io::Error::other(format!("failed to serialise history entry: {e}")))?;
    line.push('\n');

    // Open the history file for reading and writing.
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }

    let mut history_file = options.open(&path)?;

    // Ensure permissions.
    ensure_owner_only_permissions(&history_file).await?;

    // Lock file.
    acquire_exclusive_lock_with_retry(&history_file).await?;

    // We use sync I/O with spawn_blocking() because we are using a
    // [`std::fs::File`] instead of a [`tokio::fs::File`] to leverage an
    // advisory file locking API that is not available in the async API.
    let max_bytes = config.history.max_bytes;
    let line_bytes = line.into_bytes();

    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};

        if let Some(limit) = max_bytes {
            history_file.seek(SeekFrom::Start(0))?;
            let mut data = Vec::new();
            history_file.read_to_end(&mut data)?;
            data.extend_from_slice(&line_bytes);

            if data.len() > limit {
                let mut start = 0usize;
                while data.len() - start > limit {
                    match data[start..].iter().position(|&b| b == b'\n') {
                        Some(pos) => start += pos + 1,
                        None => {
                            start = data.len() - limit;
                            break;
                        }
                    }
                }
                let trimmed = &data[start..];
                history_file.set_len(0)?;
                history_file.seek(SeekFrom::Start(0))?;
                history_file.write_all(trimmed)?;
                history_file.flush()?;
                return Ok(());
            }

            history_file.seek(SeekFrom::End(0))?;
            history_file.write_all(&line_bytes)?;
            history_file.flush()?;
            return Ok(());
        }

        history_file.seek(SeekFrom::End(0))?;
        history_file.write_all(&line_bytes)?;
        history_file.flush()?;
        Ok(())
    })
    .await??;

    Ok(())
}

/// Attempt to acquire an exclusive advisory lock on `file`, retrying up to 10
/// times if the lock is currently held by another process. This prevents a
/// potential indefinite wait while still giving other writers some time to
/// finish their operation.
async fn acquire_exclusive_lock_with_retry(file: &std::fs::File) -> Result<()> {
    use tokio::time::sleep;

    for _ in 0..MAX_RETRIES {
        match fs2::FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                sleep(RETRY_SLEEP).await;
            }
            Err(e) => return Err(e),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "could not acquire exclusive lock on history file after multiple attempts",
    ))
}

/// Asynchronously fetch the history file's *identifier* (inode on Unix) and
/// the current number of entries by counting newline characters.
pub(crate) async fn history_metadata(config: &Config) -> (u64, usize) {
    let path = history_filepath(config);

    #[cfg(unix)]
    let log_id = {
        use std::os::unix::fs::MetadataExt;
        // Obtain metadata (async) to get the identifier.
        let meta = match fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (0, 0),
            Err(_) => return (0, 0),
        };
        meta.ino()
    };
    #[cfg(not(unix))]
    let log_id = 0u64;

    // Open the file.
    let mut file = match fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return (log_id, 0),
    };

    // Count newline bytes.
    let mut buf = [0u8; 8192];
    let mut count = 0usize;
    loop {
        match file.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                count += buf[..n].iter().filter(|&&b| b == b'\n').count();
            }
            Err(_) => return (log_id, 0),
        }
    }

    (log_id, count)
}

/// Given a `log_id` (on Unix this is the file's inode number) and a zero-based
/// `offset`, return the corresponding `HistoryEntry` if the identifier matches
/// the current history file **and** the requested offset exists. Any I/O or
/// parsing errors are logged and result in `None`.
///
/// Note this function is not async because it uses a sync advisory file
/// locking API.
#[cfg(unix)]
pub(crate) fn lookup(log_id: u64, offset: usize, config: &Config) -> Option<HistoryEntry> {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::os::unix::fs::MetadataExt;

    let path = history_filepath(config);
    let file: File = match OpenOptions::new().read(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open history file");
            return None;
        }
    };

    let metadata = match file.metadata() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "failed to stat history file");
            return None;
        }
    };

    if metadata.ino() != log_id {
        return None;
    }

    // Open & lock file for reading.
    if let Err(e) = acquire_shared_lock_with_retry(&file) {
        tracing::warn!(error = %e, "failed to acquire shared lock on history file");
        return None;
    }

    let reader = BufReader::new(&file);
    for (idx, line_res) in reader.lines().enumerate() {
        let line = match line_res {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "failed to read line from history file");
                return None;
            }
        };

        if idx == offset {
            match serde_json::from_str::<HistoryEntry>(&line) {
                Ok(entry) => return Some(entry),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse history entry");
                    return None;
                }
            }
        }
    }

    None
}

/// Fallback stub for non-Unix systems: currently always returns `None`.
#[cfg(not(unix))]
pub(crate) fn lookup(log_id: u64, offset: usize, config: &Config) -> Option<HistoryEntry> {
    let _ = (log_id, offset, config);
    None
}

#[cfg(unix)]
fn acquire_shared_lock_with_retry(file: &File) -> Result<()> {
    for _ in 0..MAX_RETRIES {
        match fs2::FileExt::try_lock_shared(file) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(RETRY_SLEEP);
            }
            Err(e) => return Err(e),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "could not acquire shared lock on history file after multiple attempts",
    ))
}

/// On Unix systems ensure the file permissions are `0o600` (rw-------). If the
/// permissions cannot be changed the error is propagated to the caller.
#[cfg(unix)]
async fn ensure_owner_only_permissions(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    let current_mode = metadata.permissions().mode() & 0o777;
    if current_mode != 0o600 {
        let mut perms = metadata.permissions();
        perms.set_mode(0o600);
        let perms_clone = perms.clone();
        let file_clone = file.try_clone()?;
        tokio::task::spawn_blocking(move || file_clone.set_permissions(perms_clone)).await??;
    }
    Ok(())
}

#[cfg(not(unix))]
async fn ensure_owner_only_permissions(_file: &File) -> Result<()> {
    // For now, on non-Unix, simply succeed.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::ConfigOverrides;
    use crate::config::ConfigToml;
    use tempfile::TempDir;

    fn load_default_config_for_test(codex_home: &TempDir) -> Config {
        let toml = {
            let mut t = ConfigToml::default();
            t.model_provider = Some("openai".into());
            t
        };
        Config::load_from_base_config_with_overrides(
            toml,
            ConfigOverrides::default(),
            codex_home.path().to_path_buf(),
        )
        .expect("defaults for test should always succeed")
    }

    #[tokio::test]
    async fn trims_history_to_limit() {
        let dir = TempDir::new().unwrap();
        let mut config = load_default_config_for_test(&dir);
        config.history.max_bytes = Some(100);
        let id = Uuid::new_v4();

        append_entry("first", &id, &config).await.unwrap();
        append_entry("second", &id, &config).await.unwrap();
        append_entry("third", &id, &config).await.unwrap();

        let data = tokio::fs::read_to_string(dir.path().join(HISTORY_FILENAME))
            .await
            .unwrap();
        assert!(data.len() <= 100);
        let lines: Vec<HistoryEntry> = data
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(1, lines.len());
        assert_eq!("third", lines[0].text);
    }

    #[tokio::test]
    async fn filters_sensitive_text() {
        let dir = TempDir::new().unwrap();
        let mut config = load_default_config_for_test(&dir);
        config.history.sensitive_patterns = Some(vec!["secret".into()]);
        let id = Uuid::new_v4();

        append_entry("this is secret", &id, &config).await.unwrap();
        append_entry("ok", &id, &config).await.unwrap();

        let data = tokio::fs::read_to_string(dir.path().join(HISTORY_FILENAME))
            .await
            .unwrap();
        let lines: Vec<HistoryEntry> = data
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(1, lines.len());
        assert_eq!("ok", lines[0].text);
    }
}

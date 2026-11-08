use crate::model::Session;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

const MAX_JSONL_BYTES: u64 = 1_048_576;
const MAX_JSONL_EVENTS: usize = 2000;
const MAX_EVENT_BYTES: usize = 262_144;
const CACHE_SIZE: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityStamp {
    pub at: DateTime<Utc>,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileSignature {
    inode: u64,
    bytes: u64,
    modified: u128,
}

fn signature(path: &Path) -> Option<FileSignature> {
    let stat = path.metadata().ok()?;
    Some(FileSignature {
        inode: stat.ino(),
        bytes: stat.len(),
        modified: stat
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Signature {
    File(FileSignature),
    // SQLite can append conversation rows to the WAL without changing the main database file.
    Databases(Vec<(PathBuf, Option<FileSignature>, Option<FileSignature>)>),
}

pub struct ActivityReader {
    pub codex_home: PathBuf,
    pub claude_config_dir: PathBuf,
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
    cache: HashMap<String, (Signature, Option<ActivityStamp>)>,
    order: VecDeque<String>,
    claude_paths: HashMap<String, (Instant, Option<PathBuf>)>,
}

impl ActivityReader {
    pub fn new(codex_home: impl Into<PathBuf>, claude_config_dir: impl Into<PathBuf>) -> Self {
        Self {
            codex_home: codex_home.into(),
            claude_config_dir: claude_config_dir.into(),
            timeout: Duration::from_millis(800),
            cancelled: Arc::new(AtomicBool::new(false)),
            cache: HashMap::new(),
            order: VecDeque::new(),
            claude_paths: HashMap::new(),
        }
    }

    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self::new(
            std::env::var_os("CODEX_HOME")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
            std::env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
        )
    }

    pub fn with_cancel(mut self, cancelled: Arc<AtomicBool>) -> Self {
        self.cancelled = cancelled;
        self
    }

    fn stopped(&self, deadline: Instant) -> bool {
        self.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline
    }

    pub fn read(&mut self, sessions: &[Session]) -> HashMap<String, ActivityStamp> {
        let deadline = Instant::now() + self.timeout;
        let mut resolved: HashMap<(String, String, String), Option<ActivityStamp>> = HashMap::new();
        let mut result = HashMap::new();

        for session in sessions {
            if self.stopped(deadline) {
                break;
            }
            let Some(reference) = &session.native_reference else {
                continue;
            };
            let stamp = if let Some(stamp) = resolved.get(reference) {
                stamp.clone()
            } else {
                let stamp = self.resolve(reference, deadline);
                resolved.insert(reference.clone(), stamp.clone());
                stamp
            };
            if let Some(stamp) = stamp {
                result.insert(session.terminal_id.clone(), stamp);
            }
        }
        result
    }

    fn cache_get(&mut self, key: &str, signature: &Signature) -> Option<Option<ActivityStamp>> {
        let value = self
            .cache
            .get(key)
            .filter(|(stored, _)| stored == signature)
            .map(|(_, value)| value.clone())?;
        self.order.retain(|k| k != key);
        self.order.push_back(key.into());
        Some(value)
    }

    fn cache_put(&mut self, key: String, signature: Signature, value: Option<ActivityStamp>) {
        self.order.retain(|k| k != &key);
        self.order.push_back(key.clone());
        self.cache.insert(key, (signature, value));
        while self.order.len() > CACHE_SIZE {
            if let Some(key) = self.order.pop_front() {
                self.cache.remove(&key);
            }
        }
    }

    fn databases(&self, prefix: &str) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.codex_home) else {
            return Vec::new();
        };
        let mut paths: Vec<_> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_name()?.to_str()?;
                let version = name
                    .strip_prefix(&format!("{prefix}_"))?
                    .strip_suffix(".sqlite")?
                    .parse::<u64>()
                    .ok()?;
                Some((version, path))
            })
            .collect();
        paths.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
        paths.into_iter().take(8).map(|(_, path)| path).collect()
    }

    fn database(&self, path: &Path, deadline: Instant) -> Option<Connection> {
        if self.stopped(deadline) {
            return None;
        }
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;
        connection
            .busy_timeout(
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
            )
            .ok()?;
        let cancelled = self.cancelled.clone();
        connection
            .progress_handler(
                1000,
                Some(move || cancelled.load(Ordering::Acquire) || Instant::now() >= deadline),
            )
            .ok()?;
        Some(connection)
    }

    fn resolve(
        &mut self,
        reference: &(String, String, String),
        deadline: Instant,
    ) -> Option<ActivityStamp> {
        let (provider, kind, value) = reference;
        if !["codex", "claude"].contains(&provider.as_str())
            || !["id", "path"].contains(&kind.as_str())
        {
            return None;
        }
        let path = if kind == "path" {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return None;
            }
            path
        } else {
            if value.is_empty()
                || value.len() > 128
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return None;
            }
            if provider == "claude" {
                self.claude_path(value, deadline)?
            } else {
                let paths = self.databases("thread_history");
                let sig = Signature::Databases(
                    paths
                        .iter()
                        .map(|path| {
                            (
                                path.clone(),
                                signature(path),
                                signature(&{
                                    let mut wal = path.as_os_str().to_os_string();
                                    wal.push("-wal");
                                    PathBuf::from(wal)
                                }),
                            )
                        })
                        .collect(),
                );
                let key = format!("codexdb:{value}");
                let stamp = if let Some(stamp) = self.cache_get(&key, &sig) {
                    stamp
                } else {
                    let stamp = self.codex_messages(&paths, value, deadline);
                    if !self.stopped(deadline) {
                        self.cache_put(key, sig, stamp.clone());
                    }
                    stamp
                };
                if stamp.is_some() {
                    return stamp;
                }
                self.codex_rollout(value, deadline)?
            }
        };
        let sig = Signature::File(signature(&path)?);
        let key = format!("{provider}:{}", path.display());
        if let Some(stamp) = self.cache_get(&key, &sig) {
            return stamp;
        }
        let stamp = self.jsonl(
            &path,
            provider,
            if kind == "id" { Some(value) } else { None },
            deadline,
        );
        if !self.stopped(deadline) {
            self.cache_put(key, sig, stamp.clone());
        }
        stamp
    }

    fn codex_messages(
        &self,
        paths: &[PathBuf],
        session_id: &str,
        deadline: Instant,
    ) -> Option<ActivityStamp> {
        let mut latest = None;
        for path in paths {
            if self.stopped(deadline) {
                return None;
            }
            let Some(connection) = self.database(path, deadline) else {
                continue;
            };
            let timestamp = connection
                .query_row(
                    "SELECT MAX(created_at_ms) FROM thread_items \
                     WHERE thread_id=?1 AND item_type IN ('userMessage','agentMessage')",
                    params![session_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .ok()
                .flatten()
                .and_then(DateTime::<Utc>::from_timestamp_millis);

            if let Some(timestamp) = timestamp {
                latest = Some(latest.map_or(timestamp, |old: DateTime<Utc>| old.max(timestamp)));
            }
        }
        latest.map(|at| ActivityStamp {
            at,
            source: "codex_message_db".into(),
        })
    }

    fn codex_rollout(&self, session_id: &str, deadline: Instant) -> Option<PathBuf> {
        for path in self.databases("state") {
            if self.stopped(deadline) {
                return None;
            }
            let Some(connection) = self.database(&path, deadline) else {
                continue;
            };
            let path = connection
                .query_row(
                    "SELECT rollout_path FROM threads WHERE id=?1 LIMIT 1",
                    params![session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .ok()
                .flatten();
            if let Some(path) = path {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    return Some(path);
                }
            }
        }
        None
    }

    fn claude_path(&mut self, session_id: &str, deadline: Instant) -> Option<PathBuf> {
        if let Some((at, path)) = self.claude_paths.get(session_id)
            && at.elapsed() < Duration::from_secs(5)
        {
            return path.clone();
        }
        let mut found = None;
        let entries = fs::read_dir(self.claude_config_dir.join("projects")).ok()?;
        for (index, entry) in entries.enumerate() {
            if self.stopped(deadline) {
                return None;
            }
            if index >= 4096 {
                found = None;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let candidate = entry.path().join(format!("{session_id}.jsonl"));
            if candidate.is_file() {
                // An ambiguous session ID must not borrow a timestamp from an arbitrary project.
                if found.is_some() {
                    found = None;
                    break;
                }
                found = Some(candidate);
            }
        }
        if self.claude_paths.len() >= CACHE_SIZE {
            self.claude_paths.clear();
        }
        self.claude_paths
            .insert(session_id.into(), (Instant::now(), found.clone()));
        found
    }

    fn jsonl(
        &self,
        path: &Path,
        provider: &str,
        session_id: Option<&str>,
        deadline: Instant,
    ) -> Option<ActivityStamp> {
        if self.stopped(deadline) {
            return None;
        }
        let mut file = File::open(path).ok()?;
        let size = file.seek(SeekFrom::End(0)).ok()?;
        let start = size.saturating_sub(MAX_JSONL_BYTES);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut raw = Vec::with_capacity(MAX_JSONL_BYTES.min(size) as usize);
        file.take(MAX_JSONL_BYTES).read_to_end(&mut raw).ok()?;

        // A bounded suffix can start inside a JSON record; skip the first incomplete line.
        let raw = if start > 0 {
            raw.iter()
                .position(|b| *b == b'\n')
                .map(|index| &raw[index + 1..])
                .unwrap_or(&[])
        } else {
            &raw
        };
        let mut latest = None;
        for line in raw
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .rev()
            .take(MAX_JSONL_EVENTS)
        {
            if self.stopped(deadline) {
                return None;
            }
            if line.len() > MAX_EVENT_BYTES {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let event_type = event["type"].as_str().unwrap_or("");
            // Progress, metadata, and tool events must not advance the conversation timestamp.
            let relevant = if provider == "claude" {
                ["user", "assistant"].contains(&event_type)
                    && event["message"]["role"].as_str() == Some(event_type)
                    && event["isMeta"] != true
                    && session_id.is_none_or(|id| {
                        event
                            .get("sessionId")
                            .is_none_or(|v| v.as_str() == Some(id))
                    })
            } else {
                (event_type == "response_item"
                    && event["payload"]["type"] == "message"
                    && [Some("user"), Some("assistant")]
                        .contains(&event["payload"]["role"].as_str()))
                    || (event_type == "event_msg"
                        && [Some("user_message"), Some("agent_message")]
                            .contains(&event["payload"]["type"].as_str()))
            };
            if relevant
                && let Some(timestamp) = event["timestamp"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc))
            {
                latest = Some(latest.map_or(timestamp, |old: DateTime<Utc>| old.max(timestamp)));
            }
        }
        latest.map(|at| ActivityStamp {
            at,
            source: format!("{provider}_jsonl"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn session(provider: &str, kind: &str, value: &str) -> Session {
        Session {
            terminal_id: "term".into(),
            kind: provider.into(),
            native_reference: Some((provider.into(), kind.into(), value.into())),
            ..Session::default()
        }
    }

    fn write_events(path: &Path, events: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(path).unwrap();
        for event in events {
            serde_json::to_writer(&mut file, event).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }

    fn event(role: &str, timestamp: &str) -> Value {
        json!({
            "type": role,
            "timestamp": timestamp,
            "message": { "role": role },
        })
    }

    fn reader(root: &Path) -> ActivityReader {
        ActivityReader::new(root.join("codex"), root.join("claude"))
    }

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn claude_exact_id_uses_message_timestamp_not_progress_meta_or_mtime() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("claude/projects/project/exact-id.jsonl");
        let mut meta = event("user", "2099-01-01T00:00:00Z");
        meta["isMeta"] = json!(true);
        write_events(
            &path,
            &[
                event("user", "2026-11-01T01:02:03Z"),
                event("assistant", "2026-11-02T01:02:03+09:00"),
                json!({
                    "type": "progress",
                    "timestamp": "2099-01-01T00:00:00Z",
                }),
                meta,
            ],
        );
        File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(4_000_000_000))
            .unwrap();
        let stamp = reader(root.path())
            .read(&[session("claude", "id", "exact-id")])
            .remove("term")
            .unwrap();
        assert_eq!(stamp.at, utc("2026-11-01T16:02:03Z"));
        assert_eq!(stamp.source, "claude_jsonl");
    }

    #[test]
    fn codex_database_uses_only_exact_thread_user_and_assistant_items_read_only() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("codex")).unwrap();
        let path = root.path().join("codex/thread_history_1.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE thread_items(thread_id TEXT,item_type TEXT,created_at_ms INTEGER);",
            )
            .unwrap();
        for (id, kind, time) in [
            ("exact-id", "userMessage", 1_700_000_000_000_i64),
            ("exact-id", "agentMessage", 1_700_000_001_000),
            ("exact-id", "commandExecution", 1_900_000_000_000),
            ("exact-id", "reasoning", 1_900_000_000_000),
            ("other-id", "agentMessage", 2_000_000_000_000),
        ] {
            connection
                .execute(
                    "INSERT INTO thread_items VALUES(?1,?2,?3)",
                    params![id, kind, time],
                )
                .unwrap();
        }
        drop(connection);
        let before = fs::read(&path).unwrap();
        let stamp = reader(root.path())
            .read(&[session("codex", "id", "exact-id")])
            .remove("term")
            .unwrap();
        assert_eq!(
            stamp.at,
            DateTime::from_timestamp_millis(1_700_000_001_000).unwrap()
        );
        assert_eq!(stamp.source, "codex_message_db");
        assert_eq!(before, fs::read(path).unwrap());
    }

    #[test]
    fn codex_exact_rollout_mapping_ignores_updated_at_and_nonconversation_events() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("codex")).unwrap();
        let path = root.path().join("rollout.jsonl");
        write_events(
            &path,
            &[
                json!({
                    "type": "response_item",
                    "timestamp": "2026-11-01T00:00:00Z",
                    "payload": {
                        "type": "message",
                        "role": "user",
                    },
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-11-02T00:00:00Z",
                    "payload": { "type": "agent_message" },
                }),
                json!({
                    "type": "response_item",
                    "timestamp": "2099-01-01T00:00:00Z",
                    "payload": {
                        "type": "message",
                        "role": "developer",
                    },
                }),
            ],
        );
        let connection = Connection::open(root.path().join("codex/state_5.sqlite")).unwrap();
        connection
            .execute_batch("CREATE TABLE threads(id TEXT,rollout_path TEXT,updated_at INTEGER);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES(?1,?2,?3)",
                params!["exact-id", path.to_str().unwrap(), 4_000_000_000_i64],
            )
            .unwrap();
        drop(connection);
        let stamp = reader(root.path())
            .read(&[session("codex", "id", "exact-id")])
            .remove("term")
            .unwrap();
        assert_eq!(stamp.at, utc("2026-11-02T00:00:00Z"));
        assert_eq!(stamp.source, "codex_jsonl");
    }

    #[test]
    fn absent_or_ambiguous_reference_never_guesses_from_title_cwd_or_file_order() {
        let root = tempfile::tempdir().unwrap();
        for project in ["one", "two"] {
            write_events(
                &root
                    .path()
                    .join(format!("claude/projects/{project}/exact-id.jsonl")),
                &[event("assistant", "2026-11-01T00:00:00Z")],
            );
        }
        let mut no_reference = session("claude", "id", "exact-id");
        no_reference.native_reference = None;
        no_reference.title = "exact-id".into();
        no_reference.cwd = root.path().display().to_string();
        assert!(
            reader(root.path())
                .read(&[
                    no_reference,
                    session("claude", "id", "exact-id"),
                    session("claude", "id", "../exact-id")
                ])
                .is_empty()
        );
    }

    #[test]
    fn exact_path_signature_cache_tracks_messages_and_skips_malformed_records() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("conversation.jsonl");
        write_events(
            &path,
            &[
                json!({
                    "type": [],
                    "timestamp": "2099-01-01T00:00:00Z",
                }),
                json!({
                    "type": "assistant",
                    "message": "invalid",
                    "timestamp": "2099-01-01T00:00:00Z",
                }),
                event("assistant", "2026-11-01T00:00:00Z"),
            ],
        );
        let row = session("claude", "path", path.to_str().unwrap());
        let mut reader = reader(root.path());
        let first = reader.read(std::slice::from_ref(&row))["term"].clone();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        serde_json::to_writer(&mut file, &event("assistant", "2026-11-02T00:00:00Z")).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        assert!(reader.read(&[row])["term"].at > first.at);
    }

    #[test]
    fn bounded_tail_and_missing_timestamps_do_not_use_file_activity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("conversation.jsonl");
        write_events(&path, &[event("assistant", "2026-11-01T00:00:00Z")]);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let progress = json!({
            "type": "progress",
            "timestamp": "2099-01-01T00:00:00Z",
            "padding": "x".repeat(MAX_JSONL_BYTES as usize + 100),
        });
        serde_json::to_writer(&mut file, &progress).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        assert!(
            reader(root.path())
                .read(&[session("claude", "path", path.to_str().unwrap())])
                .is_empty()
        );
    }

    #[test]
    fn corrupt_optional_database_does_not_block_valid_claude_source() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("codex")).unwrap();
        fs::write(
            root.path().join("codex/thread_history_1.sqlite"),
            b"not a database",
        )
        .unwrap();
        let path = root.path().join("conversation.jsonl");
        write_events(&path, &[event("user", "2026-11-01T00:00:00Z")]);
        let mut row = session("claude", "path", path.to_str().unwrap());
        row.terminal_id = "other".into();
        let result = reader(root.path()).read(&[session("codex", "id", "exact-id"), row]);
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("other"));
    }

    #[test]
    fn cancellation_and_locked_database_have_bounded_exit() {
        let root = tempfile::tempdir().unwrap();
        let flag = Arc::new(AtomicBool::new(true));
        let mut cancelled = reader(root.path()).with_cancel(flag);
        assert!(
            cancelled
                .read(&[session("codex", "id", "exact-id")])
                .is_empty()
        );
        fs::create_dir(root.path().join("codex")).unwrap();
        let connection =
            Connection::open(root.path().join("codex/thread_history_1.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE thread_items(thread_id TEXT,item_type TEXT,created_at_ms INTEGER);\
                 BEGIN EXCLUSIVE;",
            )
            .unwrap();
        let started = Instant::now();
        assert!(
            reader(root.path())
                .read(&[session("codex", "id", "exact-id")])
                .is_empty()
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        connection.execute_batch("ROLLBACK;").unwrap();
    }
}

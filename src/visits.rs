use crate::model::{FinderError, Result, clean_text};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct VisitHistory {
    path: PathBuf,
    socket: Vec<u8>,
}

fn database_error(error: rusqlite::Error) -> FinderError {
    FinderError::new("visit_history", error.to_string())
}

pub fn visit_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(i64::MAX as u128) as i64
}

impl VisitHistory {
    pub fn new(state_dir: &Path, socket: &Path) -> Self {
        Self {
            path: state_dir.join("workspace-visits.sqlite3"),
            socket: socket.as_os_str().as_bytes().to_vec(),
        }
    }

    pub fn from_env(socket: &Path) -> Option<Self> {
        let state_dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR").filter(|p| !p.is_empty())?;
        Some(Self::new(Path::new(&state_dir), socket))
    }

    pub fn recent(&self) -> Result<HashMap<String, i64>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let connection = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(database_error)?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(database_error)?;
        let mut statement = connection
            .prepare("SELECT workspace_id, visited_at FROM workspace_visits WHERE socket = ?1")
            .map_err(database_error)?;
        statement
            .query_map([&self.socket], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(database_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    fn connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path).map_err(database_error)?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(database_error)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS workspace_visits (
                    socket BLOB NOT NULL,
                    workspace_id TEXT NOT NULL,
                    visited_at INTEGER NOT NULL,
                    PRIMARY KEY (socket, workspace_id)
                ) WITHOUT ROWID;",
            )
            .map_err(database_error)?;
        Ok(connection)
    }

    pub fn record(&self, workspace_id: &str, at: i64) -> Result<()> {
        self.connection()?
            .execute(
                "INSERT INTO workspace_visits (socket, workspace_id, visited_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(socket, workspace_id) DO UPDATE
                 SET visited_at = MAX(visited_at, excluded.visited_at)",
                params![self.socket, workspace_id, at],
            )
            .map_err(database_error)?;
        Ok(())
    }

    pub fn event(&self, event: &Value, at: i64) -> Result<()> {
        let kind = event["event"].as_str().unwrap_or_default();
        if !matches!(kind, "workspace_focused" | "workspace_closed") {
            return Ok(());
        }
        let workspace_id = event["data"]["workspace_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 4096 && clean_text(id, false) == *id)
            .ok_or_else(|| FinderError::new("invalid_event", "Missing workspace identity"))?;
        if kind == "workspace_focused" {
            self.record(workspace_id, at)
        } else {
            self.connection()?
                .execute(
                    "DELETE FROM workspace_visits WHERE socket = ?1 AND workspace_id = ?2",
                    params![self.socket, workspace_id],
                )
                .map_err(database_error)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn visits_survive_reopening_and_stale_writes_cannot_replace_newer_visits() {
        let directory = tempfile::tempdir().unwrap();
        let socket = Path::new("/session-a.sock");
        let history = VisitHistory::new(directory.path(), socket);
        assert!(history.recent().unwrap().is_empty());
        history.record("w1", 300).unwrap();
        history.record("w2", 200).unwrap();
        history.record("w1", 100).unwrap();
        let reopened = VisitHistory::new(directory.path(), socket);
        assert_eq!(
            reopened.recent().unwrap(),
            HashMap::from([("w1".into(), 300), ("w2".into(), 200)])
        );
    }

    #[test]
    fn session_sockets_isolate_reused_workspace_ids_and_closures() {
        let directory = tempfile::tempdir().unwrap();
        let first = VisitHistory::new(directory.path(), Path::new("/a.sock"));
        let second = VisitHistory::new(directory.path(), Path::new("/b.sock"));
        first.record("w1", 100).unwrap();
        second.record("w1", 200).unwrap();
        first
            .event(
                &json!({"event": "workspace_closed", "data": {"workspace_id": "w1"}}),
                300,
            )
            .unwrap();
        assert!(first.recent().unwrap().is_empty());
        assert_eq!(second.recent().unwrap()["w1"], 200);
    }

    #[test]
    fn only_workspace_focus_events_create_visits() {
        let directory = tempfile::tempdir().unwrap();
        let history = VisitHistory::new(directory.path(), Path::new("/a.sock"));
        history
            .event(
                &json!({"event": "workspace_focused", "data": {"workspace_id": "w1"}}),
                100,
            )
            .unwrap();
        for kind in [
            "pane_output_changed",
            "pane_agent_status_changed",
            "workspace_updated",
        ] {
            history
                .event(&json!({"event": kind, "data": {"workspace_id": "w2"}}), 200)
                .unwrap();
        }
        assert_eq!(
            history.recent().unwrap(),
            HashMap::from([("w1".into(), 100)])
        );
        assert!(
            history
                .event(&json!({"event": "workspace_focused", "data": {}}), 300)
                .is_err()
        );
    }

    #[test]
    fn concurrent_hook_writes_preserve_every_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let history = VisitHistory::new(directory.path(), Path::new("/a.sock"));
                std::thread::spawn(move || history.record(&format!("w{index}"), index).unwrap())
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let history = VisitHistory::new(directory.path(), Path::new("/a.sock"));
        assert_eq!(history.recent().unwrap().len(), 8);
    }
}

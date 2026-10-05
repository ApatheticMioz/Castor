//! Append-only JSONL session event ledger.
//!
//! One event per turn is appended to `<state>/sessions/<id>/events.jsonl`:
//! a `dispatch` event (the model's turn: tool calls + finish reason), a
//! `tool_result` event per executed tool call, and a `final` event when the
//! session ends.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::state::StateDir;

/// Append-only JSONL event ledger for a single session.
#[derive(Debug, Clone)]
pub struct EventLogger {
    path: PathBuf,
    session_id: String,
}

impl EventLogger {
    /// Open (creating the session dir as needed) the ledger for `session_id`.
    pub fn new(state: &StateDir, session_id: &str) -> Self {
        let dir = state.sessions().join(session_id);
        let _ = fs::create_dir_all(&dir);
        Self {
            path: dir.join("events.jsonl"),
            session_id: session_id.to_string(),
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// The session id this ledger is scoped to.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Append one event, stamped with a timestamp and the session id.
    pub fn append(&self, event: Value) -> std::io::Result<()> {
        let mut entry = event;
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("timestamp".into(), json!(now_iso()));
            obj.insert("sessionId".into(), json!(self.session_id));
        }
        let line = serde_json::to_string(&entry)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }

    /// Read all events (for tests / inspection). Malformed lines are skipped.
    pub fn read_all(&self) -> Vec<Value> {
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

/// Current time as a standards-compliant ISO-8601 / RFC-3339 UTC string
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`), reusing the crate's single civil-date
/// formatter (see [`crate::telemetry::iso_from_ms`]).
fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    crate::telemetry::iso_from_ms(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateDir;

    #[test]
    fn appends_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("castor_ev_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let state = StateDir::new(&dir);
        let logger = EventLogger::new(&state, "s1");
        logger
            .append(json!({ "type": "dispatch", "turn": 1 }))
            .unwrap();
        logger
            .append(json!({ "type": "tool_result", "name": "bash" }))
            .unwrap();
        logger
            .append(json!({ "type": "final", "status": "completed" }))
            .unwrap();

        let events = logger.read_all();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["type"], "dispatch");
        assert_eq!(events[0]["sessionId"], "s1");
        assert!(events[0].get("timestamp").is_some());
        assert_eq!(events[2]["status"], "completed");
        let _ = fs::remove_dir_all(&dir);
    }
}

//! Append-only JSONL event log for shipping to Loki / external observability.
//!
//! When `AGENT_SALON_JSONL_LOG` points at a writable path, the salon emits one
//! line of JSON per event (relayed message, session lifecycle, delivery skip).
//! Each line shares a `ts` (RFC3339) and `kind` discriminator so LogQL queries
//! like `{job="agent-salon"} | json | kind="message" | source="miu"` work.
//! When the env var is unset the logger is a no-op — text logs to stderr keep
//! working as before for ad-hoc launchd debugging.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{SecondsFormat, Utc};
use serde::Serialize;

use crate::db::MessageRow;

pub struct JsonlLogger {
    /// Open append-mode file, or None when env was unset or open() failed.
    file: Mutex<Option<File>>,
    path: Option<PathBuf>,
}

impl JsonlLogger {
    /// Open the logger if `AGENT_SALON_JSONL_LOG` is set. Failure to open the
    /// path is logged once and produces a no-op logger — we never want
    /// observability plumbing to crash the salon.
    pub fn from_env() -> Self {
        let path = std::env::var("AGENT_SALON_JSONL_LOG")
            .ok()
            .map(PathBuf::from);
        let file = match path.as_ref() {
            Some(p) => match OpenOptions::new().create(true).append(true).open(p) {
                Ok(f) => {
                    log!("agent-salon: jsonl log -> {}", p.display());
                    Some(f)
                }
                Err(e) => {
                    log!("agent-salon: cannot open jsonl log {}: {e}", p.display());
                    None
                }
            },
            None => None,
        };
        Self {
            file: Mutex::new(file),
            path,
        }
    }

    pub fn enabled(&self) -> bool {
        self.path.is_some()
    }

    fn write_line<T: Serialize>(&self, value: &T) {
        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        let Some(f) = guard.as_mut() else { return };
        let Ok(line) = serde_json::to_string(value) else {
            return;
        };
        // Best-effort. Disk-full / FS errors are dropped silently rather than
        // looping a `log!` warning that would itself flood stderr.
        let _ = writeln!(f, "{line}");
    }

    /// Emit a "message" line carrying the full MessageRow body. Content is
    /// included verbatim — the operator opted in to shipping bodies to Loki
    /// when they set this env var.
    ///
    /// `ts` comes from the row itself, not regenerated, so Loki and the SQLite
    /// store agree on when the message was processed.
    pub fn message(&self, row: &MessageRow) {
        if !self.enabled() {
            return;
        }
        #[derive(Serialize)]
        struct Entry<'a> {
            kind: &'static str,
            #[serde(flatten)]
            row: &'a MessageRow,
        }
        self.write_line(&Entry {
            kind: "message",
            row,
        });
    }

    /// Emit an "event" line. `event` is a short stable identifier
    /// (e.g. "session_connected"); `fields` is an arbitrary JSON object
    /// merged into the top level.
    pub fn event(&self, event: &str, fields: serde_json::Value) {
        if !self.enabled() {
            return;
        }
        #[derive(Serialize)]
        struct Entry<'a> {
            ts: String,
            kind: &'static str,
            event: &'a str,
            #[serde(flatten)]
            fields: serde_json::Value,
        }
        self.write_line(&Entry {
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            kind: "event",
            event,
            fields,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn parse_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        let s = std::fs::read_to_string(path).unwrap();
        s.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn no_op_when_env_unset() {
        // SAFETY: tests in this module are sequential; we touch a process-wide env.
        unsafe { std::env::remove_var("AGENT_SALON_JSONL_LOG") };
        let logger = JsonlLogger::from_env();
        assert!(!logger.enabled());
        logger.event("session_connected", serde_json::json!({"label": "miu"}));
        // nothing to assert other than the call didn't panic.
    }

    #[test]
    fn writes_event_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        // SAFETY: tests in this module are sequential.
        unsafe { std::env::set_var("AGENT_SALON_JSONL_LOG", &path) };
        let logger = JsonlLogger::from_env();
        assert!(logger.enabled());
        logger.event(
            "session_connected",
            serde_json::json!({"label": "miu", "active": 7, "evicted": 0}),
        );
        logger.event(
            "delivery_skipped",
            serde_json::json!({"target": "claudep", "reason": "liveness_timeout"}),
        );
        // SAFETY: as above.
        unsafe { std::env::remove_var("AGENT_SALON_JSONL_LOG") };

        let lines = parse_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["kind"], "event");
        assert_eq!(lines[0]["event"], "session_connected");
        assert_eq!(lines[0]["label"], "miu");
        assert_eq!(lines[0]["active"], 7);
        assert_eq!(lines[1]["event"], "delivery_skipped");
        assert_eq!(lines[1]["reason"], "liveness_timeout");
        // ts present and parseable
        let ts = lines[0]["ts"].as_str().unwrap();
        chrono::DateTime::parse_from_rfc3339(ts).unwrap();
    }
}

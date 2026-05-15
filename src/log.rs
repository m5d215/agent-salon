use chrono::{SecondsFormat, Utc};

/// Timestamp prefix for log lines: RFC3339 in UTC with millisecond
/// precision, matching the `ts` format used for channel messages
/// (see `db.rs`).
pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `eprintln!`-compatible logging that prefixes each line with a
/// timestamp. Call sites keep their existing message text (including
/// any `agent-salon:` prefix) — only the timestamp is added. Each
/// invocation writes one line via a single `eprintln!`, so lines stay
/// intact even when emitted concurrently from multiple tasks.
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("{} {}", $crate::log::now(), format_args!($($arg)*))
    };
}

//! Prometheus metrics exposed at `GET /metrics` for scraping by Alloy /
//! Prometheus / Mimir. The metric set is intentionally small and bounded:
//! every label value comes from a controlled vocabulary (session labels,
//! known kinds, fixed result strings) so series cardinality is predictable.

use std::sync::Mutex;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

/// Labels attached to a relayed message. `target` is `"<broadcast>"` when the
/// sender didn't pick one. `kind` is taken from `meta.kind`; values outside
/// the documented set (info / status / ack / request / question / reply)
/// are folded to `"other"` so callers can't blow up cardinality.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MessageLabels {
    pub source: String,
    pub target: String,
    pub kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct LivenessLabels {
    pub target: String,
    /// `ok` | `timeout`
    pub result: String,
    /// 1-based attempt number that produced `result`. For an `ok` outcome,
    /// this is the attempt that succeeded; for a `timeout` outcome, the last
    /// attempt that was made (i.e. how many tries were burned). Bounded by
    /// `LIVENESS_PROBE_MAX_ATTEMPTS` so cardinality stays small.
    pub attempt: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct SendFailureLabels {
    /// `target_unknown` | `liveness_timeout` | `delivery_error`
    pub reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct SessionEventLabels {
    /// `connected` | `evicted`
    pub event: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BuildInfoLabels {
    pub version: String,
}

/// Set of canonical `kind` values from the salon INSTRUCTIONS doc. Anything
/// outside this set is normalised to `"other"` before being used as a label
/// value, so a chatty client can't expand cardinality with arbitrary kinds.
const KNOWN_KINDS: &[&str] = &["info", "status", "ack", "request", "question", "reply"];

pub fn normalise_kind(raw: Option<&str>) -> String {
    match raw {
        Some(k) if KNOWN_KINDS.contains(&k) => k.to_string(),
        Some(_) => "other".to_string(),
        None => "none".to_string(),
    }
}

/// Bucket boundaries (in seconds) for `send_message` duration. The liveness
/// probe budget is 15s, so buckets need to reach beyond that to capture the
/// worst case; below 50ms is rarely interesting for a network round-trip on
/// localhost or a Tailnet hop.
const DURATION_BUCKETS: &[f64] = &[0.05, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0];

pub struct Metrics {
    pub registry: Mutex<Registry>,
    pub messages: Family<MessageLabels, Counter>,
    pub liveness: Family<LivenessLabels, Counter>,
    pub send_failures: Family<SendFailureLabels, Counter>,
    pub send_message_duration: Histogram,
    pub active_sessions: Gauge,
    pub session_events: Family<SessionEventLabels, Counter>,
    pub messages_stored: Counter,
    pub db_size_bytes: Gauge,
    pub build_info: Family<BuildInfoLabels, Gauge>,
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("agent_salon");

        let messages = Family::<MessageLabels, Counter>::default();
        registry.register(
            "messages",
            "Messages relayed to subscribers, by source/target/kind",
            messages.clone(),
        );

        let liveness = Family::<LivenessLabels, Counter>::default();
        registry.register(
            "liveness_probe",
            "Outcomes of the per-message liveness probe (server-initiated ping)",
            liveness.clone(),
        );

        let send_failures = Family::<SendFailureLabels, Counter>::default();
        registry.register(
            "send_failures",
            "Messages that failed to reach any subscriber, by reason",
            send_failures.clone(),
        );

        let send_message_duration = Histogram::new(DURATION_BUCKETS.iter().copied());
        registry.register(
            "send_message_duration_seconds",
            "End-to-end duration of a send_message call (probe + send + persist)",
            send_message_duration.clone(),
        );

        let active_sessions = Gauge::default();
        registry.register(
            "active_sessions",
            "Currently connected MCP sessions",
            active_sessions.clone(),
        );

        let session_events = Family::<SessionEventLabels, Counter>::default();
        registry.register(
            "session_events",
            "Session lifecycle events (connected, evicted by same-label reconnect)",
            session_events.clone(),
        );

        let messages_stored = Counter::default();
        registry.register(
            "messages_stored",
            "Messages successfully persisted to the SQLite store",
            messages_stored.clone(),
        );

        let db_size_bytes = Gauge::default();
        registry.register(
            "db_size_bytes",
            "Size of the SQLite database file on disk",
            db_size_bytes.clone(),
        );

        let build_info = Family::<BuildInfoLabels, Gauge>::default();
        registry.register(
            "build_info",
            "Build identity (always 1); label-only carrier for the running version",
            build_info.clone(),
        );

        Self {
            registry: Mutex::new(registry),
            messages,
            liveness,
            send_failures,
            send_message_duration,
            active_sessions,
            session_events,
            messages_stored,
            db_size_bytes,
            build_info,
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::normalise_kind;

    #[test]
    fn normalises_known_kinds_verbatim() {
        for k in ["info", "status", "ack", "request", "question", "reply"] {
            assert_eq!(normalise_kind(Some(k)), k);
        }
    }

    #[test]
    fn unknown_kind_collapses_to_other() {
        assert_eq!(normalise_kind(Some("urgent-typhoon")), "other");
        assert_eq!(normalise_kind(Some("")), "other");
    }

    #[test]
    fn missing_kind_is_none() {
        assert_eq!(normalise_kind(None), "none");
    }
}

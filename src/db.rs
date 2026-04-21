use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

/// Persisted message record. Every call to `deliver_notification` produces one
/// row in this table.
#[derive(Debug, Clone, Serialize)]
pub struct MessageRow {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub via: Via,
    pub source: String,
    pub target: Option<String>,
    pub content: String,
    /// The full meta JSON object that went into the outgoing notification
    /// (caller-supplied keys plus the server-injected `ts` and `source`).
    pub meta: serde_json::Value,
    /// Labels of sessions that successfully received this notification.
    pub delivered_to: Vec<String>,
    /// Labels of sessions whose send failed and were pruned as a result.
    pub delivery_errors: Vec<String>,
    /// Remote socket addr of the `POST /notify` caller.
    /// `None` when the notification originated from an in-session tool call.
    pub sender_addr: Option<String>,
    /// MCP session id of the caller. Populated only for tool-originated sends.
    pub sender_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// External HTTP webhook: `POST /notify?label=...`
    Notify,
    /// MCP `send_message` tool invoked from a connected session.
    Tool,
}

impl Via {
    pub fn as_str(&self) -> &'static str {
        match self {
            Via::Notify => "notify",
            Via::Tool => "tool",
        }
    }
}

pub async fn open(db_path: &str) -> Result<SqlitePool, sqlx::Error> {
    // If the file doesn't exist yet, create it.
    if db_path != ":memory:" && !Path::new(db_path).exists() {
        std::fs::File::create(db_path).map_err(sqlx::Error::Io)?;
    }
    let options = SqliteConnectOptions::from_str(db_path)?.create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    initialize_schema(&pool).await?;
    Ok(pool)
}

async fn initialize_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS messages (
            id                 TEXT PRIMARY KEY,
            ts                 TEXT NOT NULL,
            via                TEXT NOT NULL,
            source             TEXT NOT NULL,
            target             TEXT,
            content            TEXT NOT NULL,
            meta               TEXT NOT NULL,
            delivered_to       TEXT NOT NULL,
            delivery_errors    TEXT NOT NULL,
            sender_addr        TEXT,
            sender_session_id  TEXT
        );
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_ts ON messages(ts)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_source ON messages(source)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_target ON messages(target)")
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn insert_message(pool: &SqlitePool, row: &MessageRow) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO messages (
            id, ts, via, source, target, content, meta,
            delivered_to, delivery_errors, sender_addr, sender_session_id
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(row.id.to_string())
    .bind(row.ts.to_rfc3339())
    .bind(row.via.as_str())
    .bind(&row.source)
    .bind(row.target.as_deref())
    .bind(&row.content)
    .bind(row.meta.to_string())
    .bind(serde_json::to_string(&row.delivered_to).unwrap_or_else(|_| "[]".into()))
    .bind(serde_json::to_string(&row.delivery_errors).unwrap_or_else(|_| "[]".into()))
    .bind(row.sender_addr.as_deref())
    .bind(row.sender_session_id.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Filters for the admin UI message list.
#[derive(Debug, Default)]
pub struct ListFilters {
    /// One-directional filter: sender label.
    pub source: Option<String>,
    /// One-directional filter: target label.
    pub target: Option<String>,
    /// Conversation filter. Each participant (if set) must be either source
    /// or target of the message. With both set this selects every message
    /// exchanged between the two participants, in either direction.
    pub participant_a: Option<String>,
    pub participant_b: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: i64,
    pub offset: i64,
}

/// Build the WHERE clause and bind the values. Shared between list and count.
fn build_where(f: &ListFilters) -> (String, Vec<String>) {
    let mut sql = String::from("WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();
    if let Some(s) = &f.source {
        sql.push_str(" AND source = ?");
        binds.push(s.clone());
    }
    if let Some(t) = &f.target {
        sql.push_str(" AND target = ?");
        binds.push(t.clone());
    }
    if let Some(a) = &f.participant_a {
        sql.push_str(" AND (source = ? OR target = ?)");
        binds.push(a.clone());
        binds.push(a.clone());
    }
    if let Some(b) = &f.participant_b {
        sql.push_str(" AND (source = ? OR target = ?)");
        binds.push(b.clone());
        binds.push(b.clone());
    }
    if let Some(s) = f.since {
        sql.push_str(" AND ts >= ?");
        binds.push(s.to_rfc3339());
    }
    if let Some(u) = f.until {
        sql.push_str(" AND ts <= ?");
        binds.push(u.to_rfc3339());
    }
    (sql, binds)
}

pub async fn list_messages(
    pool: &SqlitePool,
    f: &ListFilters,
) -> Result<Vec<MessageRow>, sqlx::Error> {
    let (where_clause, binds) = build_where(f);
    let sql = format!(
        "SELECT id, ts, via, source, target, content, meta, delivered_to, delivery_errors, \
         sender_addr, sender_session_id FROM messages {where_clause} \
         ORDER BY ts DESC LIMIT ? OFFSET ?"
    );
    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    q = q.bind(f.limit).bind(f.offset);
    let rows = q.fetch_all(pool).await?;
    rows.iter().map(row_to_message).collect()
}

pub async fn count_messages(pool: &SqlitePool, f: &ListFilters) -> Result<i64, sqlx::Error> {
    let (where_clause, binds) = build_where(f);
    let sql = format!("SELECT COUNT(*) as c FROM messages {where_clause}");
    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    let row = q.fetch_one(pool).await?;
    Ok(row.get::<i64, _>("c"))
}

pub async fn get_message(pool: &SqlitePool, id: Uuid) -> Result<Option<MessageRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, ts, via, source, target, content, meta, delivered_to, delivery_errors, \
         sender_addr, sender_session_id FROM messages WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(row_to_message).transpose()
}

/// Return the set of distinct values in `column` (must be one of "source" or "target"),
/// for populating filter dropdowns in the admin UI.
pub async fn distinct_labels(
    pool: &SqlitePool,
    column: &'static str,
) -> Result<Vec<String>, sqlx::Error> {
    assert!(column == "source" || column == "target");
    let sql = format!(
        "SELECT DISTINCT {column} FROM messages WHERE {column} IS NOT NULL ORDER BY {column}"
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    Ok(rows.iter().map(|r| r.get::<String, _>(column)).collect())
}

/// Union of every label that has ever appeared as either sender or target.
/// Populates the conversation filter dropdowns.
pub async fn distinct_participants(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT DISTINCT label FROM ( \
             SELECT source AS label FROM messages \
             UNION \
             SELECT target AS label FROM messages WHERE target IS NOT NULL \
         ) ORDER BY label",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("label")).collect())
}

fn row_to_message(row: &sqlx::sqlite::SqliteRow) -> Result<MessageRow, sqlx::Error> {
    let id_s: String = row.get("id");
    let id = Uuid::parse_str(&id_s).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let ts_s: String = row.get("ts");
    let ts = DateTime::parse_from_rfc3339(&ts_s)
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?
        .with_timezone(&Utc);
    let via_s: String = row.get("via");
    let via = match via_s.as_str() {
        "notify" => Via::Notify,
        "tool" => Via::Tool,
        other => {
            return Err(sqlx::Error::Decode(
                format!("unknown via value: {other}").into(),
            ));
        }
    };
    let meta_s: String = row.get("meta");
    let meta: serde_json::Value =
        serde_json::from_str(&meta_s).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let delivered_s: String = row.get("delivered_to");
    let delivered_to: Vec<String> =
        serde_json::from_str(&delivered_s).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let errors_s: String = row.get("delivery_errors");
    let delivery_errors: Vec<String> =
        serde_json::from_str(&errors_s).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

    Ok(MessageRow {
        id,
        ts,
        via,
        source: row.get("source"),
        target: row.get("target"),
        content: row.get("content"),
        meta,
        delivered_to,
        delivery_errors,
        sender_addr: row.get("sender_addr"),
        sender_session_id: row.get("sender_session_id"),
    })
}

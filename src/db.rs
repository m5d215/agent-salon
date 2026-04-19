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
        std::fs::File::create(db_path).map_err(|e| sqlx::Error::Io(e))?;
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
    pub source: Option<String>,
    pub target: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: i64,
    pub offset: i64,
}

pub async fn list_messages(
    pool: &SqlitePool,
    f: &ListFilters,
) -> Result<Vec<MessageRow>, sqlx::Error> {
    let mut sql = String::from(
        "SELECT id, ts, via, source, target, content, meta, delivered_to, delivery_errors, \
         sender_addr, sender_session_id FROM messages WHERE 1=1",
    );
    if f.source.is_some() {
        sql.push_str(" AND source = ?");
    }
    if f.target.is_some() {
        sql.push_str(" AND target = ?");
    }
    if f.since.is_some() {
        sql.push_str(" AND ts >= ?");
    }
    if f.until.is_some() {
        sql.push_str(" AND ts <= ?");
    }
    sql.push_str(" ORDER BY ts DESC LIMIT ? OFFSET ?");

    let mut q = sqlx::query(&sql);
    if let Some(s) = &f.source {
        q = q.bind(s);
    }
    if let Some(t) = &f.target {
        q = q.bind(t);
    }
    if let Some(s) = f.since {
        q = q.bind(s.to_rfc3339());
    }
    if let Some(u) = f.until {
        q = q.bind(u.to_rfc3339());
    }
    q = q.bind(f.limit).bind(f.offset);

    let rows = q.fetch_all(pool).await?;
    rows.iter().map(row_to_message).collect()
}

pub async fn count_messages(
    pool: &SqlitePool,
    f: &ListFilters,
) -> Result<i64, sqlx::Error> {
    let mut sql = String::from("SELECT COUNT(*) as c FROM messages WHERE 1=1");
    if f.source.is_some() {
        sql.push_str(" AND source = ?");
    }
    if f.target.is_some() {
        sql.push_str(" AND target = ?");
    }
    if f.since.is_some() {
        sql.push_str(" AND ts >= ?");
    }
    if f.until.is_some() {
        sql.push_str(" AND ts <= ?");
    }
    let mut q = sqlx::query(&sql);
    if let Some(s) = &f.source {
        q = q.bind(s);
    }
    if let Some(t) = &f.target {
        q = q.bind(t);
    }
    if let Some(s) = f.since {
        q = q.bind(s.to_rfc3339());
    }
    if let Some(u) = f.until {
        q = q.bind(u.to_rfc3339());
    }
    let row = q.fetch_one(pool).await?;
    Ok(row.get::<i64, _>("c"))
}

pub async fn get_message(
    pool: &SqlitePool,
    id: Uuid,
) -> Result<Option<MessageRow>, sqlx::Error> {
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

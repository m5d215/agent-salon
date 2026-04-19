use std::fmt::Write;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use chrono::{DateTime, FixedOffset, Utc};
use html_escape::{encode_double_quoted_attribute, encode_text};
use serde::Deserialize;
use uuid::Uuid;

use crate::db::{self, ListFilters, MessageRow};
use crate::http::AppState;

const PAGE_SIZE: i64 = 50;

/// Timezone used for both rendering timestamps and interpreting
/// `datetime-local` input on the filter form. Hardcoded to JST; this is a
/// personal tool and the operator lives in +09:00.
fn display_tz() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).expect("valid offset")
}

fn format_ts(ts: DateTime<Utc>) -> String {
    ts.with_timezone(&display_tz())
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// Query parameters for `GET /admin`. Mirrors `<form>` field names so the form
/// just submits back to the same URL.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    /// Conversation filter: each of these must appear as either source or
    /// target. With both set, pulls up every message exchanged between the
    /// two participants (both directions).
    #[serde(default)]
    pub participant_a: Option<String>,
    #[serde(default)]
    pub participant_b: Option<String>,
    /// ISO-like local datetime: `YYYY-MM-DDTHH:MM` (datetime-local input).
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub page: Option<i64>,
}

pub async fn list_page(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Response {
    let source = q.source.clone().filter(|s| !s.is_empty());
    let target = q.target.clone().filter(|s| !s.is_empty());
    let participant_a = q.participant_a.clone().filter(|s| !s.is_empty());
    let participant_b = q.participant_b.clone().filter(|s| !s.is_empty());
    let since = parse_display_datetime(q.since.as_deref());
    let until = parse_display_datetime(q.until.as_deref());

    let page = q.page.unwrap_or(0).max(0);
    let filters = ListFilters {
        source: source.clone(),
        target: target.clone(),
        participant_a: participant_a.clone(),
        participant_b: participant_b.clone(),
        since,
        until,
        limit: PAGE_SIZE,
        offset: page * PAGE_SIZE,
    };

    let messages = match db::list_messages(&state.salon.db, &filters).await {
        Ok(m) => m,
        Err(e) => return render_error(&format!("query failed: {e}")),
    };
    let total = db::count_messages(&state.salon.db, &filters).await.unwrap_or(0);
    let sources = db::distinct_labels(&state.salon.db, "source")
        .await
        .unwrap_or_default();
    let targets = db::distinct_labels(&state.salon.db, "target")
        .await
        .unwrap_or_default();
    let participants = db::distinct_participants(&state.salon.db)
        .await
        .unwrap_or_default();

    let html = render_list_page(&q, &messages, total, page, &sources, &targets, &participants);
    Html(html).into_response()
}

pub async fn detail_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Ok(uuid) = Uuid::parse_str(&id) else {
        return (StatusCode::BAD_REQUEST, "invalid id").into_response();
    };
    let msg = match db::get_message(&state.salon.db, uuid).await {
        Ok(Some(m)) => m,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return render_error(&format!("query failed: {e}")),
    };
    Html(render_detail_page(&msg)).into_response()
}

fn render_error(msg: &str) -> Response {
    let body = format!(
        "{STYLE}\n<h1>error</h1><pre>{}</pre>",
        encode_text(msg)
    );
    (StatusCode::INTERNAL_SERVER_ERROR, Html(body)).into_response()
}

/// Parse a `datetime-local` input value as wall-clock time in `display_tz()`,
/// then convert to UTC for the query.
fn parse_display_datetime(s: Option<&str>) -> Option<DateTime<Utc>> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    // datetime-local produces "YYYY-MM-DDTHH:MM" (or with seconds).
    let formats = ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"];
    let tz = display_tz();
    for fmt in formats {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return naive
                .and_local_timezone(tz)
                .single()
                .map(|dt| dt.with_timezone(&Utc));
        }
    }
    None
}

fn render_list_page(
    q: &ListQuery,
    messages: &[MessageRow],
    total: i64,
    page: i64,
    sources: &[String],
    targets: &[String],
    participants: &[String],
) -> String {
    let mut body = String::new();
    let _ = writeln!(body, "{STYLE}");
    let _ = writeln!(body, "<h1>agent-salon &middot; messages</h1>");
    let _ = writeln!(body, "<form method=\"get\" action=\"/admin\" class=\"filters\">");

    // Conversation filter (bidirectional, both must be involved).
    let _ = writeln!(body, "<fieldset class=\"group\"><legend>conversation</legend>");
    render_select(&mut body, "participant_a", q.participant_a.as_deref(), participants);
    let _ = writeln!(body, "<span class=\"sep\">&harr;</span>");
    render_select(&mut body, "participant_b", q.participant_b.as_deref(), participants);
    let _ = writeln!(body, "</fieldset>");

    // One-directional filter.
    let _ = writeln!(body, "<fieldset class=\"group\"><legend>one-way</legend>");
    render_select(&mut body, "source", q.source.as_deref(), sources);
    let _ = writeln!(body, "<span class=\"sep\">&rarr;</span>");
    render_select(&mut body, "target", q.target.as_deref(), targets);
    let _ = writeln!(body, "</fieldset>");

    // Time range.
    let _ = writeln!(body, "<fieldset class=\"group\"><legend>time (JST)</legend>");
    let _ = writeln!(
        body,
        "<label>since<input type=\"datetime-local\" name=\"since\" value=\"{}\"></label>",
        encode_double_quoted_attribute(q.since.as_deref().unwrap_or(""))
    );
    let _ = writeln!(
        body,
        "<label>until<input type=\"datetime-local\" name=\"until\" value=\"{}\"></label>",
        encode_double_quoted_attribute(q.until.as_deref().unwrap_or(""))
    );
    let _ = writeln!(body, "</fieldset>");

    let _ = writeln!(
        body,
        "<div class=\"buttons\"><button type=\"submit\">apply</button> <a href=\"/admin\">reset</a></div>"
    );
    let _ = writeln!(body, "</form>");

    let start = page * PAGE_SIZE + 1;
    let end = (page * PAGE_SIZE + messages.len() as i64).max(start.saturating_sub(1));
    let _ = writeln!(
        body,
        "<p class=\"meta\">{} total &middot; showing {}-{}</p>",
        total, start, end
    );

    let _ = writeln!(body, "<table>");
    let _ = writeln!(
        body,
        "<thead><tr><th>ts</th><th>via</th><th>source &rarr; target</th><th>content</th><th>delivered</th></tr></thead><tbody>"
    );
    for m in messages {
        let arrow = format!(
            "{} &rarr; {}",
            encode_text(&m.source),
            encode_text(m.target.as_deref().unwrap_or("(broadcast)")),
        );
        let preview = truncate(&m.content, 120);
        let delivered = if m.delivered_to.is_empty() {
            "—".to_string()
        } else {
            m.delivered_to.join(", ")
        };
        let _ = writeln!(
            body,
            "<tr><td class=\"ts\"><a href=\"/admin/messages/{}\">{}</a></td>\
             <td>{}</td><td>{}</td><td class=\"content\">{}</td><td>{}</td></tr>",
            encode_double_quoted_attribute(&m.id.to_string()),
            encode_text(&format_ts(m.ts)),
            encode_text(m.via.as_str()),
            arrow,
            encode_text(&preview),
            encode_text(&delivered),
        );
    }
    let _ = writeln!(body, "</tbody></table>");

    // Pagination
    let pages = (total + PAGE_SIZE - 1) / PAGE_SIZE;
    if pages > 1 {
        let _ = writeln!(body, "<nav class=\"pagination\">");
        if page > 0 {
            let _ = writeln!(
                body,
                "<a href=\"{}\">&laquo; prev</a>",
                encode_double_quoted_attribute(&pagination_url(q, page - 1))
            );
        }
        let _ = writeln!(body, "<span>page {} / {}</span>", page + 1, pages);
        if page + 1 < pages {
            let _ = writeln!(
                body,
                "<a href=\"{}\">next &raquo;</a>",
                encode_double_quoted_attribute(&pagination_url(q, page + 1))
            );
        }
        let _ = writeln!(body, "</nav>");
    }

    body
}

fn render_select(body: &mut String, name: &str, current: Option<&str>, options: &[String]) {
    let _ = writeln!(body, "<label>{}", name);
    let _ = writeln!(body, "<select name=\"{}\">", name);
    let _ = writeln!(body, "<option value=\"\">(any)</option>");
    for opt in options {
        let selected = if Some(opt.as_str()) == current {
            " selected"
        } else {
            ""
        };
        let _ = writeln!(
            body,
            "<option value=\"{v}\"{sel}>{label}</option>",
            v = encode_double_quoted_attribute(opt),
            sel = selected,
            label = encode_text(opt),
        );
    }
    let _ = writeln!(body, "</select></label>");
}

fn pagination_url(q: &ListQuery, page: i64) -> String {
    let mut pairs: Vec<(&str, String)> = Vec::new();
    let push_non_empty = |pairs: &mut Vec<(&'static str, String)>, k: &'static str, v: &Option<String>| {
        if let Some(s) = v {
            if !s.is_empty() {
                pairs.push((k, s.clone()));
            }
        }
    };
    push_non_empty(&mut pairs, "source", &q.source);
    push_non_empty(&mut pairs, "target", &q.target);
    push_non_empty(&mut pairs, "participant_a", &q.participant_a);
    push_non_empty(&mut pairs, "participant_b", &q.participant_b);
    push_non_empty(&mut pairs, "since", &q.since);
    push_non_empty(&mut pairs, "until", &q.until);
    pairs.push(("page", page.to_string()));
    let qs: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding(v)))
        .collect();
    format!("/admin?{}", qs.join("&"))
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => {
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn render_detail_page(m: &MessageRow) -> String {
    let mut body = String::new();
    let _ = writeln!(body, "{STYLE}");
    let _ = writeln!(
        body,
        "<p><a href=\"/admin\">&laquo; back</a></p><h1>message {}</h1>",
        encode_text(&m.id.to_string())
    );
    let _ = writeln!(body, "<dl>");
    row(&mut body, "ts", &format_ts(m.ts));
    row(&mut body, "via", m.via.as_str());
    row(&mut body, "source", &m.source);
    row(
        &mut body,
        "target",
        m.target.as_deref().unwrap_or("(broadcast)"),
    );
    row(
        &mut body,
        "delivered_to",
        &if m.delivered_to.is_empty() {
            "(none)".into()
        } else {
            m.delivered_to.join(", ")
        },
    );
    row(
        &mut body,
        "delivery_errors",
        &if m.delivery_errors.is_empty() {
            "(none)".into()
        } else {
            m.delivery_errors.join(", ")
        },
    );
    row(
        &mut body,
        "sender_addr",
        m.sender_addr.as_deref().unwrap_or("—"),
    );
    row(
        &mut body,
        "sender_session_id",
        m.sender_session_id.as_deref().unwrap_or("—"),
    );
    let _ = writeln!(body, "</dl>");
    let _ = writeln!(
        body,
        "<h2>content</h2><pre class=\"content\">{}</pre>",
        encode_text(&m.content)
    );
    let pretty = serde_json::to_string_pretty(&m.meta).unwrap_or_default();
    let _ = writeln!(
        body,
        "<h2>meta</h2><pre>{}</pre>",
        encode_text(&pretty)
    );
    body
}

fn row(body: &mut String, k: &str, v: &str) {
    let _ = writeln!(
        body,
        "<dt>{}</dt><dd>{}</dd>",
        encode_text(k),
        encode_text(v)
    );
}

const STYLE: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>agent-salon admin</title>
<style>
  body { font-family: ui-sans-serif, system-ui, -apple-system, "Helvetica Neue", sans-serif;
         margin: 1.5rem; color: #222; background: #fafafa; }
  h1 { margin-top: 0; font-weight: 600; }
  .filters { display: flex; flex-wrap: wrap; gap: 0.5rem 0.75rem; align-items: flex-end;
             padding: 0.5rem 0.75rem 0.75rem; background: #fff; border: 1px solid #e2e2e2;
             border-radius: 6px; margin-bottom: 1rem; }
  .filters .group { display: flex; gap: 0.5rem; align-items: flex-end;
                    border: 1px solid #e8e8e8; border-radius: 6px;
                    padding: 0.25rem 0.75rem 0.5rem; margin: 0; }
  .filters .group legend { font-size: 0.7rem; color: #888; padding: 0 0.25rem;
                           text-transform: uppercase; letter-spacing: 0.03em; }
  .filters .sep { align-self: center; color: #888; padding-bottom: 0.35rem; }
  .filters label { display: flex; flex-direction: column; font-size: 0.8rem;
                   color: #555; gap: 0.15rem; }
  .filters input, .filters select { padding: 0.3rem 0.45rem; border: 1px solid #ccc;
                                    border-radius: 4px; font: inherit; }
  .filters .buttons { display: flex; align-items: center; gap: 0.75rem;
                      padding-bottom: 0.1rem; }
  .filters button { padding: 0.35rem 0.9rem; background: #1a73e8; color: #fff;
                    border: 0; border-radius: 4px; cursor: pointer; }
  .filters a { color: #1a73e8; }
  .meta { color: #666; font-size: 0.85rem; }
  table { width: 100%; border-collapse: collapse; background: #fff; }
  th, td { text-align: left; padding: 0.4rem 0.6rem; border-bottom: 1px solid #eee;
           font-size: 0.9rem; vertical-align: top; }
  th { background: #f0f0f0; }
  td.ts a { color: #1a73e8; text-decoration: none; font-variant-numeric: tabular-nums; }
  td.content { max-width: 40rem; white-space: pre-wrap; word-break: break-word; }
  .pagination { margin-top: 1rem; display: flex; gap: 1rem; align-items: center; }
  pre { background: #f4f4f4; padding: 0.75rem; overflow: auto; border-radius: 4px;
        font-size: 0.85rem; }
  dl { display: grid; grid-template-columns: max-content 1fr; gap: 0.25rem 1rem;
       background: #fff; padding: 0.75rem 1rem; border: 1px solid #e2e2e2; border-radius: 6px; }
  dt { color: #666; }
  dd { margin: 0; font-variant-numeric: tabular-nums; }
</style>
"#;

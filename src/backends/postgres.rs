//! Postgres backend (sync `postgres` crate — blocking wrapper over
//! tokio-postgres, so no async refactor and still a single binary).
//!
//! We use the *simple-query protocol* (`simple_query`): the server returns every
//! column already formatted as text, exactly like psql. That sidesteps generic
//! per-type binary decoding entirely, and NULL stays distinct from empty string.
//! Trade-off (Phase 3): values arrive as Postgres' canonical text rather than
//! typed, and binding parameters isn't available — fine for ad-hoc SQL.
//!
//! Password resolution (when the URL has none): PGPASSWORD env > OS keyring.
//! TLS is not configured yet (NoTls) — managed cloud DBs needing SSL are Phase 3.

use crate::config::Profile;
use crate::db::{first_keyword, Cell, QueryResult, ROW_CAP};
use crate::{secrets, util};
use anyhow::{Context, Result};
use postgres::SimpleQueryMessage;
use std::str::FromStr;

pub fn run(profile: &Profile, sql: &str, all: bool) -> Result<QueryResult> {
    let mut cfg = postgres::Config::from_str(&profile.url)
        .with_context(|| format!("parsing connection url {}", util::redact_url(&profile.url)))?;
    if !url_has_password(&profile.url) {
        if let Some(pw) = resolve_password(profile) {
            cfg.password(pw);
        }
    }

    let mut client = cfg
        .connect(postgres::NoTls)
        .with_context(|| format!("connecting to {}", util::redact_url(&profile.url)))?;

    let messages = client
        .simple_query(sql.trim())
        .context("executing SQL")?;

    let cap = if all { usize::MAX } else { ROW_CAP };
    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut truncated = None;
    let mut affected: Option<u64> = None;
    let mut saw_row = false;

    for msg in messages {
        match msg {
            SimpleQueryMessage::Row(row) => {
                saw_row = true;
                if columns.is_empty() {
                    columns = row
                        .columns()
                        .iter()
                        .map(|c| c.name().to_string())
                        .collect();
                }
                if rows.len() >= cap {
                    if truncated.is_none() {
                        truncated = Some(rows.len());
                    }
                    continue;
                }
                let mut cells = Vec::with_capacity(row.len());
                for i in 0..row.len() {
                    cells.push(match row.get(i) {
                        Some(s) => Cell::Text(s.to_string()),
                        None => Cell::Null,
                    });
                }
                rows.push(cells);
            }
            SimpleQueryMessage::CommandComplete(n) => affected = Some(n),
            _ => {}
        }
    }

    // A zero-row SELECT yields no Row messages (and the simple protocol gives us
    // no column names in that case) — still report it as an (empty) result set
    // rather than "0 rows affected".
    let kw = first_keyword(sql);
    let query_ish = matches!(
        kw.as_str(),
        "SELECT" | "WITH" | "VALUES" | "SHOW" | "EXPLAIN" | "TABLE"
    );
    if saw_row || query_ish {
        Ok(QueryResult::Rows {
            columns,
            rows,
            truncated,
        })
    } else {
        Ok(QueryResult::Affected {
            changes: affected.unwrap_or(0) as usize,
        })
    }
}

fn url_has_password(url: &str) -> bool {
    url.split_once("://")
        .and_then(|(_, rest)| rest.split_once('@'))
        .map(|(userinfo, _)| userinfo.contains(':'))
        .unwrap_or(false)
}

fn resolve_password(profile: &Profile) -> Option<String> {
    if let Ok(v) = std::env::var("PGPASSWORD") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    secrets::get(&profile.name)
}

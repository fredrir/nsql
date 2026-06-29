use crate::config::Profile;
use crate::db::{first_keyword, Cell, QueryResult, ROW_CAP};
use crate::util::{self, url_has_password};
use anyhow::{Context, Result};
use postgres::SimpleQueryMessage;
use std::str::FromStr;

pub fn run(profile: &Profile, sql: &str, all: bool) -> Result<QueryResult> {
    let mut cfg = postgres::Config::from_str(&profile.url)
        .with_context(|| format!("parsing connection url {}", util::redact_url(&profile.url)))?;
    if !url_has_password(&profile.url) {
        if let Some(pw) = crate::creds::resolve_password(profile) {
            cfg.password(pw);
        }
    }
    cfg.connect_timeout(std::time::Duration::from_secs(8));

    let mut client = cfg
        .connect(postgres::NoTls)
        .with_context(|| format!("connecting to {}", util::redact_url(&profile.url)))?;

    let _ = client.simple_query("SET statement_timeout = '30s'");

    let messages = client.simple_query(sql.trim()).context("executing SQL")?;

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
                    columns = row.columns().iter().map(|c| c.name().to_string()).collect();
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

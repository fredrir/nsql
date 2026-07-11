//! DuckDB backend (opt-in: `--features duckdb-backend`). The duckdb crate
//! mirrors rusqlite's API, so this is structurally the sqlite backend.

use crate::config::Profile;
use crate::db::{Cell, QueryResult, RunOpts};
use crate::sql::{self, Dialect};
use anyhow::{Context, Result};

pub struct DuckConn {
    conn: duckdb::Connection,
}

pub fn connect(profile: &Profile) -> Result<DuckConn> {
    let target = profile
        .url
        .strip_prefix("duckdb://")
        .or_else(|| profile.url.strip_prefix("duckdb:"))
        .unwrap_or(&profile.url);

    let conn = if target.is_empty() || target.contains(":memory:") {
        duckdb::Connection::open_in_memory()
    } else if profile.readonly {
        duckdb::Connection::open_with_flags(
            target,
            duckdb::Config::default()
                .access_mode(duckdb::AccessMode::ReadOnly)
                .context("duckdb read-only config")?,
        )
    } else {
        duckdb::Connection::open(target)
    }
    .with_context(|| format!("opening duckdb database `{target}`"))?;

    Ok(DuckConn { conn })
}

impl DuckConn {
    pub fn cancel_closure(&self) -> Box<dyn Fn() + Send> {
        let h = self.conn.interrupt_handle();
        Box::new(move || h.interrupt())
    }
}

pub fn run_on(conn: &mut DuckConn, sql_text: &str, opts: &RunOpts) -> Result<Vec<QueryResult>> {
    let mut results = Vec::new();
    for stmt_text in sql::split_statements(sql_text, Dialect::for_scheme("duckdb")) {
        results.push(run_one(&conn.conn, &stmt_text, opts.cap)?);
    }
    Ok(results)
}

fn run_one(conn: &duckdb::Connection, stmt_text: &str, cap: usize) -> Result<QueryResult> {
    use duckdb::types::ValueRef;

    let mut stmt = conn
        .prepare(stmt_text)
        .with_context(|| format!("preparing `{}`", preview(stmt_text)))?;

    let mut q = stmt.query([]).context("executing SQL")?;

    // duckdb-rs exposes column info on the rows handle after query()
    let columns: Vec<String> = match q.as_ref() {
        Some(s) => s
            .column_names()
            .into_iter()
            .map(|c| c.to_string())
            .collect(),
        None => Vec::new(),
    };

    let ncol = columns.len();
    if ncol == 0 {
        return Ok(QueryResult::Affected { changes: 0 });
    }

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut truncated = None;
    while let Some(r) = q.next()? {
        if rows.len() >= cap {
            truncated = Some(rows.len());
            break;
        }
        let mut cells = Vec::with_capacity(ncol);
        for i in 0..ncol {
            let v = r.get_ref(i)?;
            cells.push(match v {
                ValueRef::Null => Cell::Null,
                ValueRef::Boolean(b) => Cell::Bool(b),
                ValueRef::TinyInt(i) => Cell::Int(i as i64),
                ValueRef::SmallInt(i) => Cell::Int(i as i64),
                ValueRef::Int(i) => Cell::Int(i as i64),
                ValueRef::BigInt(i) => Cell::Int(i),
                ValueRef::HugeInt(i) => i64::try_from(i)
                    .map(Cell::Int)
                    .unwrap_or_else(|_| Cell::Text(i.to_string())),
                ValueRef::UTinyInt(u) => Cell::Int(u as i64),
                ValueRef::USmallInt(u) => Cell::Int(u as i64),
                ValueRef::UInt(u) => Cell::Int(u as i64),
                ValueRef::UBigInt(u) => i64::try_from(u)
                    .map(Cell::Int)
                    .unwrap_or_else(|_| Cell::Text(u.to_string())),
                ValueRef::Float(f) => Cell::Real(f as f64),
                ValueRef::Double(d) => Cell::Real(d),
                ValueRef::Text(b) => Cell::Text(String::from_utf8_lossy(b).into_owned()),
                ValueRef::Blob(b) => Cell::Bytes(b.to_vec()),
                other => Cell::Text(format!("{other:?}")),
            });
        }
        rows.push(cells);
    }

    Ok(QueryResult::Rows {
        columns,
        rows,
        truncated,
    })
}

fn preview(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(60).collect()
}

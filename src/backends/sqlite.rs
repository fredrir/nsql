use crate::db::{Cell, QueryResult, ROW_CAP};
use anyhow::{Context, Result};

pub fn run(target: &str, sql: &str, all: bool) -> Result<QueryResult> {
    use rusqlite::types::ValueRef;

    let conn = if target == ":memory:" {
        rusqlite::Connection::open_in_memory()
    } else {
        rusqlite::Connection::open(target)
    }
    .with_context(|| format!("opening sqlite database `{target}`"))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));

    let trimmed = sql.trim();
    let mut stmt = conn.prepare(trimmed).context("preparing SQL")?;
    let ncol = stmt.column_count();

    if ncol == 0 {
        drop(stmt);
        conn.execute_batch(trimmed).context("executing SQL")?;
        return Ok(QueryResult::Affected {
            changes: conn.changes() as usize,
        });
    }

    let columns: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect();

    let cap = if all { usize::MAX } else { ROW_CAP };
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut truncated = None;

    let mut q = stmt.query([])?;
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
                ValueRef::Integer(i) => Cell::Int(i),
                ValueRef::Real(f) => Cell::Real(f),
                ValueRef::Text(b) => Cell::Text(String::from_utf8_lossy(b).into_owned()),
                ValueRef::Blob(b) => Cell::Bytes(b.to_vec()),
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

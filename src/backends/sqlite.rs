use crate::db::{Cell, QueryResult, RunOpts};
use crate::sql::{self, Dialect};
use anyhow::{Context, Result};

pub fn connect(target: &str, readonly: bool) -> Result<rusqlite::Connection> {
    use rusqlite::OpenFlags;

    let conn = if target == ":memory:" {
        // A read-only empty in-memory DB would be useless; the guard's keyword
        // scan still refuses writes on readonly profiles.
        rusqlite::Connection::open_in_memory()
    } else if readonly {
        rusqlite::Connection::open_with_flags(
            target,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    } else {
        rusqlite::Connection::open(target)
    }
    .with_context(|| format!("opening sqlite database `{target}`"))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    Ok(conn)
}

/// Run every statement in `sql_text` (the old code silently dropped anything
/// after a first row-returning statement). Each statement yields its own
/// result.
pub fn run_on(
    conn: &rusqlite::Connection,
    sql_text: &str,
    opts: &RunOpts,
) -> Result<Vec<QueryResult>> {
    let mut results = Vec::new();
    for stmt_text in sql::split_statements(sql_text, Dialect::default()) {
        results.push(run_one(conn, &stmt_text, opts.cap)?);
    }
    Ok(results)
}

fn run_one(conn: &rusqlite::Connection, stmt_text: &str, cap: usize) -> Result<QueryResult> {
    use rusqlite::types::ValueRef;

    let mut stmt = conn
        .prepare(stmt_text)
        .with_context(|| format!("preparing `{}`", preview(stmt_text)))?;
    let ncol = stmt.column_count();

    if ncol == 0 {
        let changes = stmt
            .execute([])
            .with_context(|| format!("executing `{}`", preview(stmt_text)))?;
        return Ok(QueryResult::Affected { changes });
    }

    let columns: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect();

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

fn preview(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(60).collect()
}

use crate::config::Profile;
use crate::db::{Cell, QueryResult, RunOpts};
use crate::sql::{self, Dialect};
use crate::util::{self, url_has_password};
use anyhow::{Context, Result};
use mysql::prelude::Queryable;

pub struct MyConn {
    conn: mysql::Conn,
}

pub fn connect(profile: &Profile) -> Result<MyConn> {
    // mariadb:// is the same wire protocol; the mysql crate only knows mysql://
    let url = profile
        .url
        .strip_prefix("mariadb://")
        .map(|rest| format!("mysql://{rest}"))
        .unwrap_or_else(|| profile.url.clone());

    let opts = mysql::Opts::from_url(&url)
        .with_context(|| format!("parsing connection url {}", util::redact_url(&profile.url)))?;
    let mut builder = mysql::OptsBuilder::from_opts(opts);
    if !url_has_password(&profile.url) {
        if let Some(pw) = resolve_password(profile) {
            builder = builder.pass(Some(pw));
        }
    }
    builder = builder.tcp_connect_timeout(Some(std::time::Duration::from_secs(8)));

    let mut conn = mysql::Conn::new(builder)
        .with_context(|| format!("connecting to {}", util::redact_url(&profile.url)))?;

    // Statement timeout, best effort: MySQL ≥5.7 (ms, SELECT only) and the
    // MariaDB equivalent (seconds, all statements).
    let _ = conn.query_drop("SET SESSION max_execution_time = 30000");
    let _ = conn.query_drop("SET SESSION max_statement_time = 30");
    if profile.readonly {
        conn.query_drop("SET SESSION TRANSACTION READ ONLY")
            .context("enforcing read-only session")?;
    }

    Ok(MyConn { conn })
}

fn resolve_password(profile: &Profile) -> Option<String> {
    if let Ok(v) = std::env::var("MYSQL_PWD") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    let id = crate::creds::url_identity(&profile.url, 3306)?;
    crate::secrets::get(&crate::creds::identity_key(&id))
}

pub fn run_on(conn: &mut MyConn, sql_text: &str, opts: &RunOpts) -> Result<Vec<QueryResult>> {
    let dialect = Dialect {
        backslash_strings: true,
    };
    let mut results = Vec::new();
    for stmt in sql::split_statements(sql_text, dialect) {
        results.push(run_one(&mut conn.conn, &stmt, opts.cap)?);
    }
    Ok(results)
}

fn run_one(conn: &mut mysql::Conn, stmt: &str, cap: usize) -> Result<QueryResult> {
    let mut qr = conn
        .query_iter(stmt)
        .with_context(|| format!("executing `{}`", preview(stmt)))?;

    let columns: Vec<String> = qr
        .columns()
        .as_ref()
        .iter()
        .map(|c| c.name_str().into_owned())
        .collect();

    if columns.is_empty() {
        return Ok(QueryResult::Affected {
            changes: qr.affected_rows() as usize,
        });
    }

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut truncated = None;
    for row in qr.by_ref() {
        let row = row.context("reading row")?;
        if rows.len() >= cap {
            if truncated.is_none() {
                truncated = Some(rows.len());
            }
            continue; // drain the result set
        }
        let mut cells = Vec::with_capacity(row.len());
        for i in 0..row.len() {
            cells.push(match row.as_ref(i) {
                None | Some(mysql::Value::NULL) => Cell::Null,
                Some(mysql::Value::Bytes(b)) => match std::str::from_utf8(b) {
                    Ok(s) => Cell::Text(s.to_string()),
                    Err(_) => Cell::Bytes(b.clone()),
                },
                Some(mysql::Value::Int(i)) => Cell::Int(*i),
                Some(mysql::Value::UInt(u)) => i64::try_from(*u)
                    .map(Cell::Int)
                    .unwrap_or(Cell::Text(u.to_string())),
                Some(mysql::Value::Float(f)) => Cell::Real(*f as f64),
                Some(mysql::Value::Double(d)) => Cell::Real(*d),
                Some(mysql::Value::Date(y, mo, d, h, mi, s, us)) => {
                    Cell::Text(if *h == 0 && *mi == 0 && *s == 0 && *us == 0 {
                        format!("{y:04}-{mo:02}-{d:02}")
                    } else {
                        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
                    })
                }
                Some(mysql::Value::Time(neg, d, h, mi, s, us)) => {
                    let sign = if *neg { "-" } else { "" };
                    let hours = *d * 24 + u32::from(*h);
                    Cell::Text(format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}"))
                }
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

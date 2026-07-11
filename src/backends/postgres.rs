use crate::config::Profile;
use crate::db::{Cell, QueryResult, RunOpts};
use crate::sql::{self, Dialect};
use crate::tunnel::Tunnel;
use crate::util::{self, url_has_password};
use anyhow::{bail, Context, Result};
use postgres::config::SslMode;
use postgres::SimpleQueryMessage;
use postgres_native_tls::MakeTlsConnector;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

pub struct PgConn {
    pub client: postgres::Client,
    cancel: postgres::CancelToken,
    connector: MakeTlsConnector,
    notices: Arc<Mutex<Vec<String>>>,
    _tunnel: Option<Tunnel>,
}

impl PgConn {
    pub fn cancel_closure(&self) -> Box<dyn Fn() + Send> {
        let token = self.cancel.clone();
        let connector = self.connector.clone();
        Box::new(move || {
            let _ = token.cancel_query(connector.clone());
        })
    }

    pub fn take_notices(&mut self) -> Vec<String> {
        self.notices
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }
}

/// sslmode / sslrootcert are handled by nsql itself (tokio-postgres rejects
/// verify-ca / verify-full), so strip them from the URL before Config parsing.
struct SslParams {
    mode: Option<String>,
    rootcert: Option<String>,
}

fn extract_ssl_params(url: &str) -> (String, SslParams) {
    // Anchor past the userinfo so a raw `?` in a password can't be mistaken
    // for the query-string separator.
    let anchor = url.rfind('@').map(|i| i + 1).unwrap_or(0);
    let Some(q) = url[anchor..].find('?').map(|p| anchor + p) else {
        return (
            url.to_string(),
            SslParams {
                mode: None,
                rootcert: None,
            },
        );
    };
    let (base, query) = (&url[..q], &url[q + 1..]);
    let mut mode = None;
    let mut rootcert = None;
    let mut kept: Vec<&str> = Vec::new();
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("sslmode", v)) => mode = Some(v.to_string()),
            Some(("sslrootcert", v)) => rootcert = Some(v.to_string()),
            _ => kept.push(pair),
        }
    }
    let clean = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    (clean, SslParams { mode, rootcert })
}

/// libpq-compatible sslmode semantics:
///   disable          — no TLS
///   allow/prefer     — TLS if the server offers it, no verification
///   require          — TLS mandatory, no verification (libpq behaviour)
///   verify-ca        — verify the chain, not the hostname
///   verify-full      — verify chain + hostname
/// A provided sslrootcert without an explicit mode implies verify-full.
fn build_connector(mode: &str, rootcert: Option<&str>) -> Result<MakeTlsConnector> {
    let mut b = native_tls::TlsConnector::builder();
    match mode {
        "disable" | "verify-full" => {}
        "allow" | "prefer" | "require" => {
            b.danger_accept_invalid_certs(true);
            b.danger_accept_invalid_hostnames(true);
        }
        "verify-ca" => {
            b.danger_accept_invalid_hostnames(true);
        }
        other => bail!("unsupported sslmode `{other}`"),
    }
    if let Some(path) = rootcert {
        let pem = std::fs::read(path).with_context(|| format!("reading sslrootcert {path}"))?;
        b.add_root_certificate(
            native_tls::Certificate::from_pem(&pem)
                .with_context(|| format!("parsing sslrootcert {path}"))?,
        );
    }
    Ok(MakeTlsConnector::new(
        b.build().context("building TLS connector")?,
    ))
}

pub fn connect(profile: &Profile) -> Result<PgConn> {
    let (clean_url, ssl) = extract_ssl_params(&profile.url);
    let mode = ssl.mode.clone().unwrap_or_else(|| {
        if ssl.rootcert.is_some() {
            "verify-full".to_string()
        } else {
            "prefer".to_string()
        }
    });
    let connector = build_connector(&mode, ssl.rootcert.as_deref())?;

    let mut tunnel = None;
    let mut cfg = if let Some(ssh) = &profile.ssh {
        let id = crate::creds::pg_identity(&profile.url)
            .context("cannot parse host/port out of the url for the ssh tunnel")?;
        let tun = Tunnel::open(ssh, &id.host, id.port)?;
        // host stays the real hostname (TLS verification / SNI must be pinned
        // to it, never 127.0.0.1); hostaddr carries the actual TCP target.
        let mut c = postgres::Config::new();
        c.host(&id.host);
        c.hostaddr(std::net::IpAddr::from([127, 0, 0, 1]));
        c.port(tun.local_port);
        if !id.user.is_empty() {
            c.user(&id.user);
        }
        if !id.db.is_empty() {
            c.dbname(&id.db);
        }
        if let Some(pw) = util::url_password(&profile.url) {
            c.password(pw);
        }
        tunnel = Some(tun);
        c
    } else {
        postgres::Config::from_str(&clean_url)
            .with_context(|| format!("parsing connection url {}", util::redact_url(&profile.url)))?
    };

    cfg.ssl_mode(match mode.as_str() {
        "disable" => SslMode::Disable,
        "allow" | "prefer" => SslMode::Prefer,
        _ => SslMode::Require,
    });

    if !url_has_password(&profile.url) {
        if let Some(pw) = crate::creds::resolve_password(profile) {
            cfg.password(pw);
        }
    }
    cfg.connect_timeout(std::time::Duration::from_secs(8));

    let notices: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let sink = Arc::clone(&notices);
        cfg.notice_callback(move |e: postgres::error::DbError| {
            if let Ok(mut v) = sink.lock() {
                v.push(format!("{}: {}", e.severity(), e.message()));
            }
        });
    }

    let mut client = cfg
        .connect(connector.clone())
        .with_context(|| format!("connecting to {}", util::redact_url(&profile.url)))?;

    let _ = client.simple_query("SET statement_timeout = '30s'");
    if profile.readonly {
        client
            .simple_query("SET default_transaction_read_only = on")
            .context("enforcing read-only session")?;
    }

    let cancel = client.cancel_token();
    Ok(PgConn {
        client,
        cancel,
        connector,
        notices,
        _tunnel: tunnel,
    })
}

pub fn run_on(conn: &mut PgConn, sql_text: &str, opts: &RunOpts) -> Result<Vec<QueryResult>> {
    if opts.typed {
        let stmts = sql::split_statements(sql_text, Dialect::default());
        if stmts.len() == 1 {
            if let Some(result) = try_typed(&mut conn.client, &stmts[0], opts.cap)? {
                return Ok(vec![result]);
            }
        }
    }

    let messages = conn
        .client
        .simple_query(sql_text.trim())
        .context("executing SQL")?;

    // simple_query interleaves messages for every statement in the batch;
    // CommandComplete closes one statement's result. Segmenting here is what
    // gives per-statement results (and headers for zero-row SELECTs, via
    // RowDescription).
    let mut results = Vec::new();
    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut truncated: Option<usize> = None;
    let mut is_query = false;

    for msg in messages {
        match msg {
            SimpleQueryMessage::RowDescription(cols) => {
                columns = cols.iter().map(|c| c.name().to_string()).collect();
                is_query = true;
            }
            SimpleQueryMessage::Row(row) => {
                if columns.is_empty() {
                    columns = row.columns().iter().map(|c| c.name().to_string()).collect();
                }
                is_query = true;
                if rows.len() >= opts.cap {
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
            SimpleQueryMessage::CommandComplete(n) => {
                if is_query {
                    results.push(QueryResult::Rows {
                        columns: std::mem::take(&mut columns),
                        rows: std::mem::take(&mut rows),
                        truncated: truncated.take(),
                    });
                } else {
                    results.push(QueryResult::Affected {
                        changes: n as usize,
                    });
                }
                is_query = false;
            }
            _ => {}
        }
    }
    if is_query {
        // A trailing result without CommandComplete (shouldn't happen, but
        // don't drop data if it does).
        results.push(QueryResult::Rows {
            columns,
            rows,
            truncated,
        });
    }

    Ok(results)
}

/// Extended-protocol path for --json: values come back binary and typed, so
/// numbers are JSON numbers and booleans are booleans. Returns Ok(None) when
/// any column type isn't covered — the caller falls back to the text
/// protocol, which handles everything as strings.
fn try_typed(
    client: &mut postgres::Client,
    stmt_text: &str,
    cap: usize,
) -> Result<Option<QueryResult>> {
    use postgres::types::Type;

    let Ok(prepared) = client.prepare(stmt_text) else {
        return Ok(None);
    };
    if prepared.columns().is_empty() {
        return Ok(None); // not a row-returning statement
    }
    let columns: Vec<String> = prepared
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();

    // Columns whose types have no binary decoder here (NUMERIC, uuid, inet,
    // intervals, …) get re-selected as ::text; NUMERIC text is then parsed
    // into an exact JSON number (serde_json arbitrary_precision).
    let unsupported: Vec<usize> = prepared
        .columns()
        .iter()
        .enumerate()
        .filter(|(_, c)| !type_supported(c.type_()))
        .map(|(i, _)| i)
        .collect();

    let (prepared, numeric_cols): (postgres::Statement, Vec<bool>) = if unsupported.is_empty() {
        (prepared, vec![false; columns.len()])
    } else {
        // The rewrite needs unique column names and a wrappable statement.
        let mut seen = std::collections::HashSet::new();
        if !columns.iter().all(|c| seen.insert(c.as_str())) {
            return Ok(None);
        }
        if !matches!(
            sql::first_keyword(stmt_text, Dialect::default()).as_str(),
            "SELECT" | "WITH" | "VALUES" | "TABLE"
        ) {
            return Ok(None);
        }
        let numeric_cols: Vec<bool> = prepared
            .columns()
            .iter()
            .map(|c| *c.type_() == Type::NUMERIC)
            .collect();
        let select_list: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let q = sql::quote_ident(name);
                if unsupported.contains(&i) {
                    format!("{q}::text AS {q}")
                } else {
                    q
                }
            })
            .collect();
        // newline before ')' so a trailing line comment can't eat the wrapper
        let wrapped = format!(
            "SELECT {} FROM (\n{}\n) AS _nsql_typed",
            select_list.join(", "),
            stmt_text
        );
        match client.prepare(&wrapped) {
            Ok(p) => (p, numeric_cols),
            Err(_) => return Ok(None),
        }
    };

    let Ok(raw) = client.query(&prepared, &[]) else {
        return Ok(None);
    };

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut truncated = None;
    for (n, row) in raw.iter().enumerate() {
        if n >= cap {
            truncated = Some(cap);
            break;
        }
        let mut cells = Vec::with_capacity(row.len());
        for (i, col) in row.columns().iter().enumerate() {
            match typed_cell(row, i, col.type_()) {
                Some(cell) => cells.push(numeric_fixup(cell, numeric_cols[i])),
                None => return Ok(None),
            }
        }
        rows.push(cells);
    }

    Ok(Some(QueryResult::Rows {
        columns,
        rows,
        truncated,
    }))
}

/// A NUMERIC column came back as text — turn it into an exact JSON number.
fn numeric_fixup(cell: Cell, was_numeric: bool) -> Cell {
    if !was_numeric {
        return cell;
    }
    match cell {
        Cell::Text(s) => match s.parse::<serde_json::Number>() {
            Ok(n) => Cell::Json(serde_json::Value::Number(n)),
            Err(_) => Cell::Text(s), // NaN / Infinity stay textual
        },
        other => other,
    }
}

// Temporal types deliberately go through the ::text cast wrapper instead of
// binary chrono decoding: values like 'infinity'::timestamp have no chrono
// representation, and a mid-row decode failure would re-run the whole query
// via the text protocol (double-executing volatile functions).
fn type_supported(ty: &postgres::types::Type) -> bool {
    use postgres::types::Type;
    matches!(
        *ty,
        Type::BOOL
            | Type::INT2
            | Type::INT4
            | Type::INT8
            | Type::OID
            | Type::FLOAT4
            | Type::FLOAT8
            | Type::TEXT
            | Type::VARCHAR
            | Type::BPCHAR
            | Type::NAME
            | Type::UNKNOWN
            | Type::JSON
            | Type::JSONB
            | Type::BYTEA
    )
}

fn typed_cell(row: &postgres::Row, i: usize, ty: &postgres::types::Type) -> Option<Cell> {
    use postgres::types::Type;

    macro_rules! get {
        ($t:ty, $wrap:expr) => {
            row.try_get::<_, Option<$t>>(i)
                .ok()
                .map(|v| v.map($wrap).unwrap_or(Cell::Null))
        };
    }

    match *ty {
        Type::BOOL => get!(bool, Cell::Bool),
        Type::INT2 => get!(i16, |v| Cell::Int(v as i64)),
        Type::INT4 => get!(i32, |v| Cell::Int(v as i64)),
        Type::INT8 => get!(i64, Cell::Int),
        Type::OID => get!(u32, |v| Cell::Int(v as i64)),
        Type::FLOAT4 => get!(f32, |v| Cell::Real(v as f64)),
        Type::FLOAT8 => get!(f64, Cell::Real),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            get!(String, Cell::Text)
        }
        Type::JSON | Type::JSONB => get!(serde_json::Value, Cell::Json),
        Type::BYTEA => get!(Vec<u8>, Cell::Bytes),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_params_are_extracted_and_url_cleaned() {
        let (clean, ssl) = extract_ssl_params(
            "postgres://u@h/db?sslmode=verify-full&application_name=x&sslrootcert=/ca.pem",
        );
        assert_eq!(clean, "postgres://u@h/db?application_name=x");
        assert_eq!(ssl.mode.as_deref(), Some("verify-full"));
        assert_eq!(ssl.rootcert.as_deref(), Some("/ca.pem"));

        let (clean, ssl) = extract_ssl_params("postgres://u@h/db");
        assert_eq!(clean, "postgres://u@h/db");
        assert!(ssl.mode.is_none());
    }

    #[test]
    fn question_mark_in_password_does_not_eat_sslmode() {
        let (clean, ssl) = extract_ssl_params("postgres://u:p?ss@h/db?sslmode=verify-full");
        assert_eq!(clean, "postgres://u:p?ss@h/db");
        assert_eq!(ssl.mode.as_deref(), Some("verify-full"));
    }

    #[test]
    fn rejects_unknown_sslmode() {
        assert!(build_connector("bogus", None).is_err());
        assert!(build_connector("verify-full", None).is_ok());
        assert!(build_connector("require", None).is_ok());
    }
}

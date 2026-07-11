use crate::backends;
use crate::config::Profile;
use crate::sql::{self, Dialect};
use anyhow::{bail, Result};

pub const ROW_CAP: usize = 1000;

#[derive(Debug)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
}

#[derive(Debug)]
pub enum QueryResult {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Cell>>,
        truncated: Option<usize>,
    },
    Affected {
        changes: usize,
    },
}

/// Everything one execution produced: per-statement results plus any server
/// notices (Postgres RAISE NOTICE / WARNING).
#[derive(Debug, Default)]
pub struct RunOutput {
    pub results: Vec<QueryResult>,
    pub notices: Vec<String>,
}

pub struct RunOpts {
    pub cap: usize,
    /// Prefer the typed (extended) protocol so --json gets real numbers and
    /// booleans; falls back to the text protocol for exotic types.
    pub typed: bool,
}

impl RunOpts {
    pub fn new(all: bool) -> Self {
        RunOpts {
            cap: if all { usize::MAX } else { ROW_CAP },
            typed: false,
        }
    }
}

/// A pinned backend connection.
/// Holding one across statements is what makes
/// transactions, --watch, and --repeat possible; it also carries the cancel
/// handle for Ctrl-C.
#[allow(clippy::large_enum_variant)]
pub enum Conn {
    Sqlite(rusqlite::Connection),
    Pg(backends::postgres::PgConn),
    #[cfg(feature = "mysql-backend")]
    MySql(backends::mysql::MyConn),
    #[cfg(feature = "duckdb-backend")]
    Duck(backends::duck::DuckConn),
}

impl Conn {
    pub fn dialect(&self) -> Dialect {
        match self {
            #[cfg(feature = "mysql-backend")]
            Conn::MySql(_) => Dialect {
                backslash_strings: true,
            },
            _ => Dialect::default(),
        }
    }

    /// A closure that cancels the statement currently running on this
    /// connection, if the backend supports it.
    pub fn cancel_closure(&self) -> Option<Box<dyn Fn() + Send>> {
        match self {
            Conn::Sqlite(c) => {
                let h = c.get_interrupt_handle();
                Some(Box::new(move || h.interrupt()))
            }
            Conn::Pg(c) => Some(c.cancel_closure()),
            #[cfg(feature = "mysql-backend")]
            Conn::MySql(c) => Some(c.cancel_closure()),
            #[cfg(feature = "duckdb-backend")]
            Conn::Duck(c) => Some(c.cancel_closure()),
        }
    }

    /// Drain server notices accumulated since the last call.
    pub fn take_notices(&mut self) -> Vec<String> {
        match self {
            Conn::Pg(c) => c.take_notices(),
            _ => Vec::new(),
        }
    }
}

pub fn connect(profile: &Profile) -> Result<Conn> {
    match profile.scheme() {
        "sqlite" => Ok(Conn::Sqlite(backends::sqlite::connect(
            &profile.sqlite_target(),
            profile.readonly,
        )?)),
        "postgres" | "postgresql" => Ok(Conn::Pg(backends::postgres::connect(profile)?)),
        #[cfg(feature = "mysql-backend")]
        "mysql" | "mariadb" => Ok(Conn::MySql(backends::mysql::connect(profile)?)),
        #[cfg(not(feature = "mysql-backend"))]
        other @ ("mysql" | "mariadb") => bail!(
            "this build has no `{other}` support (rebuild with `--features mysql-backend`).\n\
             profile `{}` url: {}",
            profile.name,
            crate::util::redact_url(&profile.url)
        ),
        #[cfg(feature = "duckdb-backend")]
        "duckdb" => Ok(Conn::Duck(backends::duck::connect(profile)?)),
        #[cfg(not(feature = "duckdb-backend"))]
        "duckdb" => bail!(
            "this build has no DuckDB support (rebuild with `--features duckdb-backend`).\n\
             profile `{}` url: {}",
            profile.name,
            crate::util::redact_url(&profile.url)
        ),
        other => bail!(
            "unsupported url scheme `{other}` in profile `{}`",
            profile.name
        ),
    }
}

pub fn run_on(conn: &mut Conn, sql_text: &str, opts: &RunOpts) -> Result<RunOutput> {
    let results = match conn {
        Conn::Sqlite(c) => backends::sqlite::run_on(c, sql_text, opts)?,
        Conn::Pg(c) => backends::postgres::run_on(c, sql_text, opts)?,
        #[cfg(feature = "mysql-backend")]
        Conn::MySql(c) => backends::mysql::run_on(c, sql_text, opts)?,
        #[cfg(feature = "duckdb-backend")]
        Conn::Duck(c) => backends::duck::run_on(c, sql_text, opts)?,
    };
    Ok(RunOutput {
        results,
        notices: conn.take_notices(),
    })
}

/// One-shot convenience: connect, arm Ctrl-C cancellation, run, disconnect.
pub fn run_all(profile: &Profile, sql_text: &str, opts: &RunOpts) -> Result<RunOutput> {
    let mut conn = connect(profile)?;
    crate::cancel::reset();
    let _guard = conn.cancel_closure().map(crate::cancel::arm);
    run_on(&mut conn, sql_text, opts)
}

/// Single-result convenience used by the embed session (its pane renders one
/// result). Multi-statement batches collapse via `primary`.
pub fn run(profile: &Profile, sql_text: &str, all: bool) -> Result<QueryResult> {
    run_all(profile, sql_text, &RunOpts::new(all)).map(primary)
}

/// Collapse a multi-statement output to one result for single-result surfaces
/// (the embed pane): the last row-returning result wins, otherwise the summed
/// affected count.
pub fn primary(out: RunOutput) -> QueryResult {
    let mut affected = 0usize;
    let mut last_rows = None;
    for r in out.results {
        match r {
            QueryResult::Rows { .. } => last_rows = Some(r),
            QueryResult::Affected { changes } => affected += changes,
        }
    }
    last_rows.unwrap_or(QueryResult::Affected { changes: affected })
}

pub fn strip_sql_comments(sql_text: &str) -> String {
    sql::strip_comments(sql_text, Dialect::default())
}

pub fn first_keyword(sql_text: &str) -> String {
    sql::first_keyword(sql_text, Dialect::default())
}

/// Keywords that may open a statement in a read-only session.
const READ_OK: &[&str] = &[
    "SELECT", "WITH", "EXPLAIN", "PRAGMA", "SHOW", "VALUES", "DESCRIBE", "DESC", "TABLE", "",
];

/// Any bare occurrence of these anywhere in a statement is refused on a
/// read-only profile. Deliberately over-broad: it exists to catch writes
/// smuggled past the first keyword (`SELECT 1; DROP …`, `WITH d AS
/// (DELETE …) SELECT …`, `SET default_transaction_read_only = off`). The
/// engine-level enforcement (SQLITE_OPEN_READONLY / default_transaction_
/// read_only=on) is the authoritative backstop.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "DROP", "TRUNCATE", "ALTER", "CREATE", "REPLACE", "GRANT",
    "REVOKE", "MERGE", "ATTACH", "DETACH", "VACUUM", "REINDEX", "COPY", "CALL", "DO", "SET",
    "RESET", "LOCK", "INTO", "IMPORT", "INSTALL", "LOAD",
];

const DESTRUCTIVE: &[&str] = &[
    "DELETE", "DROP", "UPDATE", "TRUNCATE", "ALTER", "INSERT", "REPLACE", "CREATE", "GRANT",
    "REVOKE", "MERGE",
];

pub fn guard(profile: &Profile, sql_text: &str, assume_yes: bool, is_tty: bool) -> Result<()> {
    let dialect = Dialect::for_scheme(profile.scheme());
    let statements = sql::split_statements(sql_text, dialect);

    let mut destructive_kw: Option<String> = None;
    for stmt in &statements {
        let kw = sql::first_keyword(stmt, dialect);

        if profile.readonly {
            if !READ_OK.contains(&kw.as_str()) {
                bail!(
                    "profile `{}` is read-only — refusing `{}` statement",
                    profile.name,
                    kw
                );
            }
            if let Some(w) = sql::keywords(stmt, dialect)
                .iter()
                .find(|k| WRITE_KEYWORDS.contains(&k.as_str()))
            {
                bail!(
                    "profile `{}` is read-only — refusing statement containing `{}` \
                     (writes can hide in CTEs and batches)",
                    profile.name,
                    w
                );
            }
        }

        if destructive_kw.is_none() && DESTRUCTIVE.contains(&kw.as_str()) {
            destructive_kw = Some(kw);
        }
    }

    if profile.prod {
        if let Some(kw) = destructive_kw {
            if assume_yes {
                return Ok(());
            }
            if !is_tty {
                bail!(
                    "refusing destructive `{}` on PROD profile `{}` without --yes",
                    kw,
                    profile.name
                );
            }
            use std::io::Write;
            let label = if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                format!("\x1b[1;31m{kw} on PROD\x1b[0m profile `{}`", profile.name)
            } else {
                format!("{kw} on PROD profile `{}`", profile.name)
            };
            eprint!("\u{26a0}\u{fe0f}  {label}. Type 'yes' to proceed: ");
            std::io::stderr().flush().ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if line.trim() != "yes" {
                bail!("aborted");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn p(prod: bool, readonly: bool) -> Profile {
        Profile {
            name: "t".into(),
            url: "sqlite://x.db".into(),
            prod,
            readonly,
            no_history: false,
            ssh: None,
        }
    }

    #[test]
    fn readonly_blocks_writes() {
        assert!(guard(&p(false, true), "delete from t", false, false).is_err());
        assert!(guard(&p(false, true), "select 1", false, false).is_ok());
    }

    #[test]
    fn readonly_blocks_multi_statement_smuggling() {
        assert!(guard(&p(false, true), "select 1; drop table users", false, false).is_err());
    }

    #[test]
    fn readonly_blocks_cte_writes() {
        assert!(guard(
            &p(false, true),
            "with d as (delete from users returning *) select * from d",
            false,
            false
        )
        .is_err());
    }

    #[test]
    fn readonly_blocks_set() {
        assert!(guard(
            &p(false, true),
            "set default_transaction_read_only = off",
            false,
            false
        )
        .is_err());
    }

    #[test]
    fn readonly_allows_quoted_write_words() {
        assert!(guard(
            &p(false, true),
            "select \"drop\" as x, 'delete' as y from t",
            false,
            false
        )
        .is_ok());
    }

    #[test]
    fn prod_requires_confirmation_when_not_tty() {
        assert!(guard(&p(true, false), "drop table t", false, false).is_err());
        assert!(guard(&p(true, false), "drop table t", true, false).is_ok());
        assert!(guard(&p(true, false), "select 1", false, false).is_ok());
    }

    #[test]
    fn prod_catches_destructive_after_select() {
        assert!(guard(&p(true, false), "select 1; drop table t", false, false).is_err());
    }

    #[test]
    fn non_prod_non_readonly_is_open() {
        assert!(guard(&p(false, false), "drop table t", false, false).is_ok());
    }

    #[test]
    fn primary_prefers_last_rows() {
        let out = RunOutput {
            results: vec![
                QueryResult::Affected { changes: 2 },
                QueryResult::Rows {
                    columns: vec!["a".into()],
                    rows: vec![],
                    truncated: None,
                },
            ],
            notices: vec![],
        };
        assert!(matches!(primary(out), QueryResult::Rows { .. }));
    }
}

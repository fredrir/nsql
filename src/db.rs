use crate::backends;
use crate::config::Profile;
use crate::util;
use anyhow::{bail, Result};

pub const ROW_CAP: usize = 1000;

#[derive(Debug)]
pub enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Bytes(Vec<u8>),
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

pub fn run(profile: &Profile, sql: &str, all: bool) -> Result<QueryResult> {
    match profile.scheme() {
        "sqlite" => backends::sqlite::run(&profile.sqlite_target(), sql, all),
        "postgres" | "postgresql" => backends::postgres::run(profile, sql, all),
        other @ ("mysql" | "mariadb") => bail!(
            "the `{other}` backend isn't wired yet (Postgres + SQLite are). \
             It slots in next behind the dispatch in src/db.rs / src/backends/.\n\
             profile `{}` url: {}",
            profile.name,
            util::redact_url(&profile.url)
        ),
        other => bail!("unsupported url scheme `{other}` in profile `{}`", profile.name),
    }
}

pub fn strip_sql_comments(sql: &str) -> String {
    sql.lines()
        .map(|l| match l.find("--") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn first_keyword(sql: &str) -> String {
    for raw in sql.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        return line
            .chars()
            .take_while(|c| c.is_alphabetic())
            .collect::<String>()
            .to_uppercase();
    }
    String::new()
}

pub fn guard(profile: &Profile, sql: &str, assume_yes: bool, is_tty: bool) -> Result<()> {
    let kw = first_keyword(sql);

    let read_only_ok = matches!(
        kw.as_str(),
        "SELECT" | "WITH" | "EXPLAIN" | "PRAGMA" | "SHOW" | "VALUES" | "DESCRIBE" | ""
    );
    if profile.readonly && !read_only_ok {
        bail!(
            "profile `{}` is read-only — refusing `{}` statement",
            profile.name,
            kw
        );
    }

    let destructive = matches!(
        kw.as_str(),
        "DELETE"
            | "DROP"
            | "UPDATE"
            | "TRUNCATE"
            | "ALTER"
            | "INSERT"
            | "REPLACE"
            | "CREATE"
            | "GRANT"
            | "REVOKE"
            | "MERGE"
    );
    if profile.prod && destructive {
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
        eprint!(
            "\u{26a0}\u{fe0f}  {} on PROD profile `{}`. Type 'yes' to proceed: ",
            kw, profile.name
        );
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "yes" {
            bail!("aborted");
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
            url: "sqlite::memory:".into(),
            prod,
            readonly,
            no_history: false,
        }
    }

    #[test]
    fn keyword_skips_comments() {
        assert_eq!(first_keyword("-- hi\n  select 1"), "SELECT");
        assert_eq!(first_keyword("DELETE FROM x"), "DELETE");
    }

    #[test]
    fn readonly_blocks_writes() {
        assert!(guard(&p(false, true), "delete from x", false, false).is_err());
        assert!(guard(&p(false, true), "select * from x", false, false).is_ok());
    }

    #[test]
    fn prod_destructive_needs_yes_when_noninteractive() {
        assert!(guard(&p(true, false), "drop table x", false, false).is_err());
        assert!(guard(&p(true, false), "drop table x", true, false).is_ok());
        assert!(guard(&p(true, false), "select 1", false, false).is_ok());
    }

    #[test]
    fn sqlite_select_roundtrip() {
        let prof = p(false, false);
        let r = run(&prof, "select 7 as answer, null as n", false).unwrap();
        match r {
            QueryResult::Rows { columns, rows, .. } => {
                assert_eq!(columns, vec!["answer", "n"]);
                assert_eq!(rows.len(), 1);
                assert!(matches!(rows[0][0], Cell::Int(7)));
                assert!(matches!(rows[0][1], Cell::Null));
            }
            _ => panic!("expected rows"),
        }
    }
}

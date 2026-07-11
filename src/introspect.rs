//! `nsql tables` / `describe` / `schemas` — psql's `\d` family as verbs.
//!
//! The simple query protocol has no bind parameters, so identifiers and
//! literals are quoted/escaped here (the #1 injection risk called out in the
//! design docs — never format a raw name into these strings).

use crate::config::Profile;
use crate::sql;
use anyhow::{bail, Result};

pub enum Verb<'a> {
    Tables { schema: Option<&'a str> },
    Describe { name: &'a str },
    Schemas,
}

/// Backend-appropriate SQL for the verb.
pub fn query(profile: &Profile, verb: &Verb) -> Result<String> {
    let scheme = profile.scheme();
    match scheme {
        "sqlite" => sqlite_query(verb),
        "postgres" | "postgresql" | "duckdb" => pg_query(verb),
        "mysql" | "mariadb" => mysql_query(verb),
        other => bail!("no introspection for scheme `{other}`"),
    }
}

fn sqlite_query(verb: &Verb) -> Result<String> {
    Ok(match verb {
        Verb::Tables { .. } => "SELECT name, type FROM sqlite_master \
             WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name"
            .to_string(),
        Verb::Describe { name } => {
            format!("PRAGMA table_info({})", sql::quote_ident(name))
        }
        Verb::Schemas => "PRAGMA database_list".to_string(),
    })
}

fn pg_query(verb: &Verb) -> Result<String> {
    Ok(match verb {
        Verb::Tables { schema } => {
            let filter = match schema {
                Some(s) => format!("table_schema = {}", sql::quote_literal(s)),
                None => "table_schema NOT IN ('pg_catalog','information_schema')".to_string(),
            };
            format!(
                "SELECT table_schema AS schema, table_name AS name, table_type AS type \
                 FROM information_schema.tables WHERE {filter} ORDER BY 1, 2"
            )
        }
        Verb::Describe { name } => {
            let (schema, table) = split_qualified(name);
            let mut filter = format!("table_name = {}", sql::quote_literal(table));
            if let Some(s) = schema {
                filter.push_str(&format!(" AND table_schema = {}", sql::quote_literal(s)));
            }
            format!(
                "SELECT column_name AS column, data_type AS type, is_nullable AS nullable, \
                        column_default AS default \
                 FROM information_schema.columns WHERE {filter} ORDER BY ordinal_position"
            )
        }
        Verb::Schemas => "SELECT schema_name AS schema FROM information_schema.schemata \
             WHERE schema_name NOT IN ('pg_catalog','information_schema') ORDER BY 1"
            .to_string(),
    })
}

fn mysql_query(verb: &Verb) -> Result<String> {
    Ok(match verb {
        Verb::Tables { schema } => {
            let filter = match schema {
                Some(s) => format!("table_schema = {}", sql::quote_literal(s)),
                None => "table_schema = DATABASE()".to_string(),
            };
            format!(
                "SELECT table_name AS name, table_type AS type \
                 FROM information_schema.tables WHERE {filter} ORDER BY 1"
            )
        }
        Verb::Describe { name } => {
            let (schema, table) = split_qualified(name);
            let mut filter = format!("table_name = {}", sql::quote_literal(table));
            match schema {
                Some(s) => {
                    filter.push_str(&format!(" AND table_schema = {}", sql::quote_literal(s)))
                }
                None => filter.push_str(" AND table_schema = DATABASE()"),
            }
            format!(
                "SELECT column_name AS `column`, column_type AS `type`, is_nullable AS nullable, \
                        column_default AS `default` \
                 FROM information_schema.columns WHERE {filter} ORDER BY ordinal_position"
            )
        }
        Verb::Schemas => "SHOW DATABASES".to_string(),
    })
}

fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.split_once('.') {
        Some((s, t)) if !s.is_empty() && !t.is_empty() => (Some(s), t),
        _ => (None, name),
    }
}

/// Table/column identifiers for the editor completion dictionary.
pub fn completion_query(scheme: &str) -> Option<&'static str> {
    match scheme {
        "sqlite" => Some(
            "SELECT m.name, p.name FROM sqlite_master m \
             JOIN pragma_table_info(m.name) p \
             WHERE m.type IN ('table','view') AND m.name NOT LIKE 'sqlite_%'",
        ),
        "postgres" | "postgresql" | "duckdb" => Some(
            "SELECT table_name, column_name FROM information_schema.columns \
             WHERE table_schema NOT IN ('pg_catalog','information_schema')",
        ),
        "mysql" | "mariadb" => Some(
            "SELECT table_name, column_name FROM information_schema.columns \
             WHERE table_schema = DATABASE()",
        ),
        _ => None,
    }
}

pub fn dict_path(paths: &crate::config::Paths, profile: &str) -> std::path::PathBuf {
    paths.state_dir.join(format!("dict-{profile}.txt"))
}

/// Refresh the completion dictionary for the classic editor in the
/// background: table and column names, one per line, 0600. The editor points
/// `dictionary+=` at the path up front; vim only reads it at completion time,
/// so the race with editor startup is harmless.
pub fn refresh_dictionary(paths: &crate::config::Paths, profile: &Profile) {
    let Some(q) = completion_query(profile.scheme()) else {
        return;
    };
    let path = dict_path(paths, &profile.name);
    let profile = profile.clone();
    let q = q.to_string();
    std::thread::spawn(move || {
        let Ok(crate::db::QueryResult::Rows { rows, .. }) = crate::db::run(&profile, &q, true)
        else {
            return;
        };
        let mut names: Vec<String> = rows
            .iter()
            .flatten()
            .filter_map(|c| match c {
                crate::db::Cell::Text(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            })
            .collect();
        names.sort();
        names.dedup();
        let _ = crate::util::write_private(&path, (names.join("\n") + "\n").as_bytes());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prof(url: &str) -> Profile {
        Profile {
            name: "t".into(),
            url: url.into(),
            prod: false,
            readonly: false,
            no_history: false,
            ssh: None,
        }
    }

    #[test]
    fn describe_quotes_hostile_names() {
        let q = query(
            &prof("postgres://u@h/db"),
            &Verb::Describe {
                name: "x'; drop table t; --",
            },
        )
        .unwrap();
        assert!(q.contains("'x''; drop table t; --'"));
    }

    #[test]
    fn sqlite_describe_quotes_ident() {
        let q = query(&prof("sqlite://x.db"), &Verb::Describe { name: "we\"ird" }).unwrap();
        assert!(q.contains("\"we\"\"ird\""));
    }

    #[test]
    fn schema_filter_is_literal() {
        let q = query(
            &prof("postgres://u@h/db"),
            &Verb::Tables {
                schema: Some("pub'lic"),
            },
        )
        .unwrap();
        assert!(q.contains("'pub''lic'"));
    }
}

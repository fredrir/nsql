//! Favorite-parameter substitution: `:name`, `:'name'`, `:"name"`.
//!
//! nsql owns the quoting — `:'name'` becomes a correctly escaped string
//! literal, `:"name"` a quoted identifier. Raw `:name` only accepts values
//! that cannot smuggle SQL (numbers, booleans, NULL) unless --unsafe-subst.

use crate::sql::{self, Dialect, ParamStyle};
use anyhow::{bail, Result};
use std::collections::HashMap;

pub fn parse_bindings(pairs: &[String]) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for pair in pairs {
        let Some((k, v)) = pair.split_once('=') else {
            bail!("bad --param `{pair}` — expected NAME=VALUE");
        };
        map.insert(k.trim().to_string(), v.to_string());
    }
    Ok(map)
}

fn raw_ok(v: &str) -> bool {
    let lower = v.to_ascii_lowercase();
    if matches!(lower.as_str(), "true" | "false" | "null") {
        return true;
    }
    let v = v.strip_prefix('-').unwrap_or(v);
    !v.is_empty()
        && v.chars().all(|c| c.is_ascii_digit() || c == '.')
        && v.chars().filter(|&c| c == '.').count() <= 1
}

pub fn substitute(
    template: &str,
    bindings: &HashMap<String, String>,
    unsafe_subst: bool,
    dialect: Dialect,
) -> Result<String> {
    let refs = sql::find_params(template, dialect);
    if refs.is_empty() {
        if !bindings.is_empty() {
            bail!(
                "this favorite has no :parameters, but --param was given \
                 (write `:name`, `:'name'`, or `:\"name\"` in the favorite)"
            );
        }
        return Ok(template.to_string());
    }

    let mut out = String::with_capacity(template.len());
    let mut pos = 0usize;
    for r in &refs {
        let Some(value) = bindings.get(&r.name) else {
            let wanted: Vec<&str> = refs.iter().map(|p| p.name.as_str()).collect();
            bail!(
                "favorite needs -P {}=… (parameters: {})",
                r.name,
                wanted.join(", ")
            );
        };
        out.push_str(&template[pos..r.start]);
        match r.style {
            ParamStyle::Literal => out.push_str(&sql::quote_literal(value)),
            ParamStyle::Ident => out.push_str(&sql::quote_ident(value)),
            ParamStyle::Raw => {
                if raw_ok(value) || unsafe_subst {
                    out.push_str(value);
                } else {
                    bail!(
                        "raw :{} only takes numbers/booleans/null (got `{}`) — \
                         use :'{}' for a string, or --unsafe-subst if you own the quoting",
                        r.name,
                        value,
                        r.name
                    );
                }
            }
        }
        pos = r.end;
    }
    out.push_str(&template[pos..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_all_three_styles() {
        let out = substitute(
            "select * from :\"tbl\" where id = :id and name = :'name'",
            &bind(&[("tbl", "users"), ("id", "42"), ("name", "it's bob")]),
            false,
            Dialect::default(),
        )
        .unwrap();
        assert_eq!(
            out,
            "select * from \"users\" where id = 42 and name = 'it''s bob'"
        );
    }

    #[test]
    fn raw_rejects_strings_without_flag() {
        let e = substitute(
            "select :x",
            &bind(&[("x", "1; drop table t")]),
            false,
            Dialect::default(),
        );
        assert!(e.is_err());
        let ok = substitute(
            "select :x",
            &bind(&[("x", "1; drop table t")]),
            true,
            Dialect::default(),
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn missing_param_lists_names() {
        let e = substitute("select :a, :'b'", &bind(&[]), false, Dialect::default())
            .unwrap_err()
            .to_string();
        assert!(e.contains("a"));
        assert!(e.contains("b"));
    }

    #[test]
    fn casts_are_not_params() {
        let out = substitute("select 1::int", &HashMap::new(), false, Dialect::default()).unwrap();
        assert_eq!(out, "select 1::int");
    }
}

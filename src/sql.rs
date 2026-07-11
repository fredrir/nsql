//! Token-level SQL scanning shared by the statement splitter, the safety
//! guard, comment stripping, and favorite-parameter substitution.
//!
//! This is not a parser: it only classifies bytes as code vs. string /
//! identifier / comment so that splitting on `;` and scanning keywords never
//! trips over quoted content. Dollar-quoting ($tag$…$tag$), nested block
//! comments, `E'…'` backslash escapes, and MySQL backslash strings are all
//! handled at this level.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dialect {
    /// MySQL treats backslash as an escape in ordinary strings; Postgres and
    /// SQLite only do so in `E'…'` strings.
    pub backslash_strings: bool,
    /// $tag$…$tag$ quoting exists in Postgres/DuckDB only; treating it as a
    /// string in MySQL/SQLite would swallow semicolons after a bare `$word$`.
    pub dollar_strings: bool,
}

impl Dialect {
    pub fn for_scheme(scheme: &str) -> Self {
        match scheme {
            "mysql" | "mariadb" => Dialect {
                backslash_strings: true,
                dollar_strings: false,
            },
            "sqlite" => Dialect {
                backslash_strings: false,
                dollar_strings: false,
            },
            _ => Dialect::default(),
        }
    }
}

impl Default for Dialect {
    fn default() -> Self {
        Dialect {
            backslash_strings: false,
            dollar_strings: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Code,
    LineComment,
    BlockComment,
    /// '…' (including the E'…' body)
    Str,
    /// "…" or `…` quoted identifier
    Ident,
    /// $tag$ … $tag$
    DollarStr,
}

/// One classified span of the input. Spans partition the input exactly.
#[derive(Debug)]
pub struct Span<'a> {
    pub kind: Kind,
    pub text: &'a str,
}

/// Classify `sql` into code / string / comment spans.
pub fn scan(sql: &str, dialect: Dialect) -> Vec<Span<'_>> {
    fn push<'a>(spans: &mut Vec<Span<'a>>, sql: &'a str, kind: Kind, from: usize, to: usize) {
        if to > from {
            spans.push(Span {
                kind,
                text: &sql[from..to],
            });
        }
    }

    let b = sql.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < b.len() {
        match b[i] {
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                push(&mut spans, sql, Kind::Code, start, i);
                let end = memchr_newline(b, i).unwrap_or(b.len());
                push(&mut spans, sql, Kind::LineComment, i, end);
                i = end;
                start = i;
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                push(&mut spans, sql, Kind::Code, start, i);
                let mut depth = 1usize;
                let mut j = i + 2;
                while j < b.len() && depth > 0 {
                    if j + 1 < b.len() && b[j] == b'/' && b[j + 1] == b'*' {
                        depth += 1;
                        j += 2;
                    } else if j + 1 < b.len() && b[j] == b'*' && b[j + 1] == b'/' {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                push(&mut spans, sql, Kind::BlockComment, i, j);
                i = j;
                start = i;
            }
            b'\'' => {
                push(&mut spans, sql, Kind::Code, start, i);
                let escaped = dialect.backslash_strings
                    || (i > 0 && (b[i - 1] == b'E' || b[i - 1] == b'e') && !ident_before(b, i - 1));
                let end = scan_quoted(b, i, b'\'', escaped);
                push(&mut spans, sql, Kind::Str, i, end);
                i = end;
                start = i;
            }
            b'"' => {
                push(&mut spans, sql, Kind::Code, start, i);
                let end = scan_quoted(b, i, b'"', false);
                push(&mut spans, sql, Kind::Ident, i, end);
                i = end;
                start = i;
            }
            b'`' => {
                push(&mut spans, sql, Kind::Code, start, i);
                let end = scan_quoted(b, i, b'`', false);
                push(&mut spans, sql, Kind::Ident, i, end);
                i = end;
                start = i;
            }
            b'$' if dialect.dollar_strings => {
                if let Some(tag_end) = dollar_tag(b, i) {
                    push(&mut spans, sql, Kind::Code, start, i);
                    let tag = &sql[i..tag_end];
                    let end = match find_sub(b, tag.as_bytes(), tag_end) {
                        Some(close) => close + tag.len(),
                        None => b.len(),
                    };
                    push(&mut spans, sql, Kind::DollarStr, i, end);
                    i = end;
                    start = i;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    push(&mut spans, sql, Kind::Code, start, b.len());
    spans
}

fn memchr_newline(b: &[u8], from: usize) -> Option<usize> {
    b[from..].iter().position(|&c| c == b'\n').map(|p| from + p)
}

/// True if the byte at `i` is preceded by an identifier character, meaning a
/// leading `E` belongs to a word like `TABLE` rather than an E'…' prefix.
fn ident_before(b: &[u8], i: usize) -> bool {
    i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')
}

/// Scan a quoted region starting at the opening quote `q` at `open`.
/// Doubling (`''`) always escapes; backslash escapes when `backslash` is set.
/// Returns the index just past the closing quote (or end of input).
fn scan_quoted(b: &[u8], open: usize, q: u8, backslash: bool) -> usize {
    let mut j = open + 1;
    while j < b.len() {
        if backslash && b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == q {
            if j + 1 < b.len() && b[j + 1] == q {
                j += 2; // doubled quote
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// If a valid dollar-quote tag starts at `i` (`$$` or `$word$`), return the
/// index just past the opening tag.
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'$' => return Some(j + 1),
            c if c.is_ascii_alphanumeric() || c == b'_' => j += 1,
            _ => return None,
        }
    }
    None
}

fn find_sub(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

/// Split `sql` into statements on `;` found in code spans. Statements keep
/// their comments; entries that are empty (or comment-only) are dropped.
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<String> {
    let spans = scan(sql, dialect);
    let mut stmts = Vec::new();
    let mut cur = String::new();

    for span in &spans {
        if span.kind != Kind::Code {
            cur.push_str(span.text);
            continue;
        }
        let mut rest = span.text;
        while let Some(pos) = rest.find(';') {
            cur.push_str(&rest[..pos]);
            flush(&mut stmts, &mut cur, dialect);
            rest = &rest[pos + 1..];
        }
        cur.push_str(rest);
    }
    flush(&mut stmts, &mut cur, dialect);
    stmts
}

fn flush(stmts: &mut Vec<String>, cur: &mut String, dialect: Dialect) {
    let stmt = std::mem::take(cur);
    if !strip_comments(&stmt, dialect).trim().is_empty() {
        stmts.push(stmt.trim().to_string());
    }
}

/// Remove comments (string- and dollar-quote-aware). Comments become a single
/// space so adjacent tokens don't fuse.
pub fn strip_comments(sql: &str, dialect: Dialect) -> String {
    let mut out = String::with_capacity(sql.len());
    for span in scan(sql, dialect) {
        match span.kind {
            Kind::LineComment | Kind::BlockComment => out.push(' '),
            _ => out.push_str(span.text),
        }
    }
    out
}

/// The first bare keyword of a statement, uppercased ("" if none).
pub fn first_keyword(stmt: &str, dialect: Dialect) -> String {
    keywords(stmt, dialect)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// Every bare keyword-like word in code spans, uppercased, in order.
/// Quoted identifiers, strings, and comments never contribute.
pub fn keywords(stmt: &str, dialect: Dialect) -> Vec<String> {
    let mut out = Vec::new();
    for span in scan(stmt, dialect) {
        if span.kind != Kind::Code {
            continue;
        }
        let mut word = String::new();
        for c in span.text.chars() {
            if c.is_ascii_alphabetic() || c == '_' {
                word.push(c.to_ascii_uppercase());
            } else if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        }
        if !word.is_empty() {
            out.push(word);
        }
    }
    out
}

/// A `:name`, `:'name'`, or `:"name"` favorite parameter found in code spans.
#[derive(Debug, PartialEq, Eq)]
pub struct ParamRef {
    /// Byte range of the whole token (including the colon and any quotes).
    pub start: usize,
    pub end: usize,
    pub name: String,
    pub style: ParamStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamStyle {
    /// `:name` — raw substitution (numbers / booleans only unless overridden)
    Raw,
    /// `:'name'` — substituted as a quoted SQL string literal
    Literal,
    /// `:"name"` — substituted as a quoted identifier
    Ident,
}

/// Find favorite parameters. `::type` casts and colons inside strings,
/// comments, and quoted identifiers are ignored.
///
/// Note the scanner classifies the quoted part of `:'name'` / `:"name"` as a
/// Str/Ident span, so those forms appear as a code span ending in `:` followed
/// by a quoted span — handled across the span boundary.
pub fn find_params(sql: &str, dialect: Dialect) -> Vec<ParamRef> {
    let mut params = Vec::new();
    let spans = scan(sql, dialect);
    let mut offset = 0usize;

    for (si, span) in spans.iter().enumerate() {
        if span.kind != Kind::Code {
            offset += span.text.len();
            continue;
        }
        let b = span.text.as_bytes();
        let mut i = 0usize;
        while i < b.len() {
            if b[i] != b':' {
                i += 1;
                continue;
            }
            // `::cast` — skip both colons
            if i + 1 < b.len() && b[i + 1] == b':' {
                i += 2;
                continue;
            }
            match b.get(i + 1) {
                // `:name` — raw parameter fully inside this code span
                Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {
                    let name_start = i + 1;
                    let mut j = name_start;
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                        j += 1;
                    }
                    params.push(ParamRef {
                        start: offset + i,
                        end: offset + j,
                        name: span.text[name_start..j].to_string(),
                        style: ParamStyle::Raw,
                    });
                    i = j;
                }
                // `:'name'` / `:"name"` — the quote opens the NEXT span
                None => {
                    if let Some(next) = spans.get(si + 1) {
                        let style = match next.kind {
                            Kind::Str => Some(ParamStyle::Literal),
                            Kind::Ident => Some(ParamStyle::Ident),
                            _ => None,
                        };
                        if let Some(style) = style {
                            let inner = next
                                .text
                                .strip_prefix(['\'', '"'])
                                .and_then(|s| s.strip_suffix(['\'', '"']))
                                .unwrap_or("");
                            if !inner.is_empty()
                                && inner
                                    .chars()
                                    .next()
                                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                                && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                            {
                                params.push(ParamRef {
                                    start: offset + i,
                                    end: offset + span.text.len() + next.text.len(),
                                    name: inner.to_string(),
                                    style,
                                });
                            }
                        }
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        offset += span.text.len();
    }
    params
}

/// True when `buf` reads as a finished statement batch: no unterminated
/// quote/dollar-string at the end, and the last code character is `;`.
/// Trailing comments after the `;` are fine.
pub fn batch_complete(buf: &str, dialect: Dialect) -> bool {
    let spans = scan(buf, dialect);
    if let Some(last) = spans.last() {
        let t = last.text;
        let open = match last.kind {
            Kind::Str => !(t.len() >= 2 && t.ends_with('\'')),
            Kind::Ident => {
                let q = t.chars().next().unwrap_or('"');
                !(t.len() >= 2 && t.ends_with(q))
            }
            Kind::DollarStr => {
                let tag_len = t[1..].find('$').map(|p| p + 2).unwrap_or(t.len());
                !(t.len() >= 2 * tag_len && t.ends_with(&t[..tag_len]))
            }
            _ => false,
        };
        if open {
            return false;
        }
    }
    let mut last_code_char = None;
    for span in &spans {
        if span.kind == Kind::Code {
            if let Some(c) = span.text.trim_end().chars().last() {
                last_code_char = Some(c);
            }
        }
    }
    last_code_char == Some(';')
}

/// Quote a string as a SQL literal ('' doubling).
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Quote an identifier with double quotes ("" doubling).
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg() -> Dialect {
        Dialect::default()
    }

    #[test]
    fn splits_on_semicolons() {
        let v = split_statements("select 1; select 2 ; ", pg());
        assert_eq!(v, vec!["select 1", "select 2"]);
    }

    #[test]
    fn semicolon_in_string_does_not_split() {
        let v = split_statements("select 'a;b'; select 2", pg());
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], "select 'a;b'");
    }

    #[test]
    fn dollar_quoting_swallows_semicolons() {
        let v = split_statements(
            "create function f() returns void as $fn$ begin; end; $fn$ language plpgsql; select 1",
            pg(),
        );
        assert_eq!(v.len(), 2);
        assert!(v[0].contains("$fn$ begin; end; $fn$"));
    }

    #[test]
    fn comments_do_not_split_or_leak() {
        let v = split_statements(
            "select 1 -- trailing ; comment\n; /* block ; */ select 2",
            pg(),
        );
        assert_eq!(v.len(), 2);
        let stripped = strip_comments("select 'a--b' -- real comment", pg());
        assert!(stripped.contains("'a--b'"));
        assert!(!stripped.contains("real comment"));
    }

    #[test]
    fn nested_block_comments() {
        let s = strip_comments("select /* outer /* inner */ still */ 1", pg());
        assert_eq!(s.trim(), "select   1".trim());
        assert!(!s.contains("inner"));
    }

    #[test]
    fn dollar_quoting_is_postgres_only() {
        let lite = Dialect::for_scheme("sqlite");
        let v = split_statements("select 1 as a$x$; select 2 as b", lite);
        assert_eq!(v.len(), 2, "sqlite must not treat $x$ as a string opener");
        let my = Dialect::for_scheme("mysql");
        let v = split_statements("select 1 as a$x$; select 2 as b", my);
        assert_eq!(v.len(), 2);
        let pg_d = Dialect::for_scheme("postgres");
        let v = split_statements("select $x$ a; b $x$; select 2", pg_d);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn batch_completeness_is_string_aware() {
        let d = Dialect::default();
        assert!(batch_complete("select 1;", d));
        assert!(batch_complete("select 1; -- trailing comment", d));
        assert!(!batch_complete("select 1", d));
        assert!(!batch_complete("insert into t values (';", d));
        assert!(!batch_complete("select $tag$ open;", d));
        assert!(batch_complete("select 'a;b';", d));
    }

    #[test]
    fn escaped_strings() {
        // E'..' with a backslash-escaped quote
        let v = split_statements(r"select E'it\'s'; select 2", pg());
        assert_eq!(v.len(), 2);
        // plain string: backslash is NOT an escape in pg/sqlite
        let v = split_statements(r"select 'c:\'; select 2", pg());
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], r"select 'c:\'");
        // mysql dialect: backslash IS an escape
        let my = Dialect::for_scheme("mysql");
        let v = split_statements(r"select 'it\'s a; test'; select 2", my);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn doubled_quote_escape() {
        let v = split_statements("select 'it''s; fine'; select 2", pg());
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn keywords_skip_quoted_content() {
        let kws = keywords(
            "select \"drop\", 'delete me' as x from t -- update nothing",
            pg(),
        );
        assert!(kws.contains(&"SELECT".to_string()));
        assert!(kws.contains(&"FROM".to_string()));
        assert!(!kws.contains(&"DROP".to_string()));
        assert!(!kws.contains(&"DELETE".to_string()));
        assert!(!kws.contains(&"UPDATE".to_string()));
    }

    #[test]
    fn first_keyword_skips_comments() {
        assert_eq!(first_keyword("-- lead\n /* c */ SELECT 1", pg()), "SELECT");
        assert_eq!(first_keyword("", pg()), "");
    }

    #[test]
    fn params_found_and_casts_skipped() {
        let sql = "select :id, :'name', :\"col\" from t where a = 1::int and s = ':not_me'";
        let ps = find_params(sql, pg());
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[0].name, "id");
        assert_eq!(ps[0].style, ParamStyle::Raw);
        assert_eq!(ps[1].name, "name");
        assert_eq!(ps[1].style, ParamStyle::Literal);
        assert_eq!(ps[2].name, "col");
        assert_eq!(ps[2].style, ParamStyle::Ident);
        assert_eq!(&sql[ps[1].start..ps[1].end], ":'name'");
    }

    #[test]
    fn quoting_helpers() {
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }
}

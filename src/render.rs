use crate::cli::{Cli, FormatArg};
use crate::db::{Cell, QueryResult};
use crate::{pager, util};
use anyhow::Result;
use std::fmt::Write as _;

const DEFAULT_NULL_GLYPH: &str = "(null)";
const BLOB_PREVIEW: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Auto,
    Table,
    Expanded,
    Tsv,
    Csv,
    Json,
    Ndjson,
    Markdown,
}

pub struct Options {
    pub format: Format,
    pub is_tty: bool,
    pub echo: Option<String>,
    pub elapsed: Option<std::time::Duration>,
    pub null_glyph: String,
}

impl Options {
    pub fn from_cli(
        cli: &Cli,
        is_tty: bool,
        echo: Option<String>,
        elapsed: Option<std::time::Duration>,
    ) -> Self {
        let format = if cli.json {
            Format::Json
        } else if cli.expanded {
            Format::Expanded
        } else {
            match &cli.format {
                Some(FormatArg::Table) => Format::Table,
                Some(FormatArg::Tsv) => Format::Tsv,
                Some(FormatArg::Csv) => Format::Csv,
                Some(FormatArg::Json) => Format::Json,
                Some(FormatArg::Ndjson) => Format::Ndjson,
                Some(FormatArg::Markdown) => Format::Markdown,
                None => Format::Auto,
            }
        };
        Options {
            format,
            is_tty,
            echo,
            elapsed,
            null_glyph: cli
                .null
                .clone()
                .unwrap_or_else(|| DEFAULT_NULL_GLYPH.to_string()),
        }
    }

    fn resolved(&self) -> Format {
        match self.format {
            Format::Auto => {
                if self.is_tty {
                    Format::Table
                } else {
                    Format::Tsv
                }
            }
            other => other,
        }
    }
}

/// Render and print every result of a (possibly multi-statement) run.
pub fn print_all(results: &[QueryResult], opts: &Options) -> Result<()> {
    pager::emit(&format_all(results, opts), opts.is_tty)
}

pub fn format_all(results: &[QueryResult], opts: &Options) -> String {
    match opts.resolved() {
        Format::Json if results.len() > 1 => {
            let arr: Vec<serde_json::Value> = results
                .iter()
                .map(|r| match r {
                    QueryResult::Affected { changes } => {
                        serde_json::json!({ "affected": changes })
                    }
                    QueryResult::Rows { columns, rows, .. } => {
                        serde_json::json!({ "rows": rows_to_json(columns, rows) })
                    }
                })
                .collect();
            let mut out = serde_json::to_string_pretty(&serde_json::Value::Array(arr))
                .unwrap_or_else(|_| "[]".into());
            out.push('\n');
            out
        }
        _ => {
            let mut out = String::new();
            let last = results.len().saturating_sub(1);
            for (i, result) in results.iter().enumerate() {
                let sub = Options {
                    format: opts.format,
                    is_tty: opts.is_tty,
                    // echo the SQL once, timing on the final result only
                    echo: if i == 0 { opts.echo.clone() } else { None },
                    elapsed: if i == last { opts.elapsed } else { None },
                    null_glyph: opts.null_glyph.clone(),
                };
                out.push_str(&format(result, &sub));
            }
            out
        }
    }
}

pub fn format(result: &QueryResult, opts: &Options) -> String {
    let fmt = opts.resolved();
    let human = matches!(fmt, Format::Table | Format::Expanded);
    let timing = opts
        .elapsed
        .map(|d| format!(" in {}", fmt_elapsed(d)))
        .unwrap_or_default();
    let mut out = String::new();

    if human {
        if let Some(sql) = &opts.echo {
            for line in sql.trim().lines() {
                let _ = writeln!(out, "-- {}", sanitize(line));
            }
        }
    }

    match result {
        QueryResult::Affected { changes } => match fmt {
            Format::Json => {
                let _ = writeln!(out, "{{\"affected\": {changes}}}");
            }
            Format::Ndjson => {
                let _ = writeln!(out, "{{\"affected\":{changes}}}");
            }
            Format::Tsv | Format::Csv => {
                let _ = writeln!(out, "{changes}");
            }
            _ => {
                let _ = writeln!(out, "OK \u{2014} {changes} row(s) affected{timing}");
            }
        },
        QueryResult::Rows {
            columns,
            rows,
            truncated,
        } => {
            let fmt = if fmt == Format::Table
                && opts.format == Format::Auto
                && wider_than_terminal(columns, rows, &opts.null_glyph)
            {
                Format::Expanded // pgcli-style auto-vertical for wide rows
            } else {
                fmt
            };
            match fmt {
                Format::Table => render_table(&mut out, columns, rows, &opts.null_glyph),
                Format::Expanded => render_expanded(&mut out, columns, rows, &opts.null_glyph),
                Format::Tsv => render_sv(&mut out, columns, rows, '\t', &opts.null_glyph),
                Format::Csv => render_csv(&mut out, columns, rows, &opts.null_glyph),
                Format::Json => render_json(&mut out, columns, rows),
                Format::Ndjson => render_ndjson(&mut out, columns, rows),
                Format::Markdown => render_markdown(&mut out, columns, rows, &opts.null_glyph),
                Format::Auto => unreachable!(),
            }
            let n = rows.len();
            let human = matches!(fmt, Format::Table | Format::Expanded);
            if human {
                let suffix = if truncated.is_some() {
                    " (capped, ,a for all)"
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "({n} row{}{timing}{suffix})",
                    if n == 1 { "" } else { "s" }
                );
            }
        }
    }

    out
}

/// Natural (unwrapped) table width against the terminal: header/cell display
/// widths + comfy-table chrome (3 per column + 1).
fn wider_than_terminal(columns: &[String], rows: &[Vec<Cell>], null_glyph: &str) -> bool {
    let (term_w, _) = util::term_size();
    let mut total = 1usize;
    for (i, col) in columns.iter().enumerate() {
        let mut w = col.chars().count();
        for row in rows.iter().take(50) {
            if let Some(cell) = row.get(i) {
                w = w.max(display_cell(cell, null_glyph).chars().count());
            }
        }
        total += w + 3;
        if total > term_w as usize {
            return true;
        }
    }
    false
}

fn render_table(out: &mut String, columns: &[String], rows: &[Vec<Cell>], null_glyph: &str) {
    if columns.is_empty() {
        return;
    }
    use comfy_table::{ContentArrangement, Table};
    let (width, _) = util::term_size();
    let mut t = Table::new();
    t.load_preset(comfy_table::presets::UTF8_FULL);
    t.set_content_arrangement(ContentArrangement::Dynamic);
    t.set_width(width);
    t.set_header(columns.iter().map(|c| sanitize(c)));
    for row in rows {
        t.add_row(row.iter().map(|c| display_cell(c, null_glyph)));
    }
    let _ = writeln!(out, "{t}");
}

fn render_expanded(out: &mut String, columns: &[String], rows: &[Vec<Cell>], null_glyph: &str) {
    let label_w = columns.iter().map(|c| c.chars().count()).max().unwrap_or(0);
    for (i, row) in rows.iter().enumerate() {
        let _ = writeln!(out, "-[ row {} ]-", i + 1);
        for (col, cell) in columns.iter().zip(row) {
            let _ = writeln!(
                out,
                "{:>label_w$} | {}",
                sanitize(col),
                display_cell(cell, null_glyph),
                label_w = label_w
            );
        }
    }
}

fn render_sv(
    out: &mut String,
    columns: &[String],
    rows: &[Vec<Cell>],
    sep: char,
    null_glyph: &str,
) {
    let _ = writeln!(
        out,
        "{}",
        columns
            .iter()
            .map(|c| sanitize(c))
            .collect::<Vec<_>>()
            .join(&sep.to_string())
    );
    for row in rows {
        let _ = writeln!(
            out,
            "{}",
            row.iter()
                .map(|c| display_cell(c, null_glyph))
                .collect::<Vec<_>>()
                .join(&sep.to_string())
        );
    }
}

fn render_csv(out: &mut String, columns: &[String], rows: &[Vec<Cell>], null_glyph: &str) {
    let _ = writeln!(
        out,
        "{}",
        columns
            .iter()
            .map(|c| csv_field(&sanitize(c)))
            .collect::<Vec<_>>()
            .join(",")
    );
    for row in rows {
        let _ = writeln!(
            out,
            "{}",
            row.iter()
                .map(|c| csv_field(&display_cell(c, null_glyph)))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
}

fn render_markdown(out: &mut String, columns: &[String], rows: &[Vec<Cell>], null_glyph: &str) {
    let md = |s: &str| sanitize(s).replace('|', "\\|");
    let _ = writeln!(
        out,
        "| {} |",
        columns
            .iter()
            .map(|c| md(c))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let _ = writeln!(
        out,
        "|{}|",
        columns
            .iter()
            .map(|_| " --- ")
            .collect::<Vec<_>>()
            .join("|")
    );
    for row in rows {
        let _ = writeln!(
            out,
            "| {} |",
            row.iter()
                .map(|c| md(&display_cell(c, null_glyph)))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
}

fn rows_to_json(columns: &[String], rows: &[Vec<Cell>]) -> Vec<serde_json::Value> {
    rows.iter()
        .map(|row| {
            let mut map = serde_json::Map::new();
            for (col, cell) in columns.iter().zip(row) {
                map.insert(col.clone(), cell_to_json(cell));
            }
            serde_json::Value::Object(map)
        })
        .collect()
}

fn render_json(out: &mut String, columns: &[String], rows: &[Vec<Cell>]) {
    let arr = rows_to_json(columns, rows);
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&serde_json::Value::Array(arr))
            .unwrap_or_else(|_| "[]".into())
    );
}

fn render_ndjson(out: &mut String, columns: &[String], rows: &[Vec<Cell>]) {
    for v in rows_to_json(columns, rows) {
        let _ = writeln!(out, "{}", serde_json::to_string(&v).unwrap_or_default());
    }
}

fn cell_to_json(cell: &Cell) -> serde_json::Value {
    use serde_json::Value;
    match cell {
        Cell::Null => Value::Null,
        Cell::Bool(b) => Value::Bool(*b),
        Cell::Int(i) => Value::from(*i),
        Cell::Real(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Cell::Text(s) => Value::String(sanitize(s)),
        Cell::Bytes(b) => Value::String(blob_hex(b)),
        Cell::Json(v) => v.clone(),
    }
}

fn display_cell(cell: &Cell, null_glyph: &str) -> String {
    match cell {
        Cell::Null => null_glyph.to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::Real(f) => f.to_string(),
        Cell::Text(s) => sanitize(s),
        Cell::Bytes(b) => blob_hex(b),
        Cell::Json(v) => sanitize(&v.to_string()),
    }
}

fn blob_hex(b: &[u8]) -> String {
    let shown: String = b
        .iter()
        .take(BLOB_PREVIEW)
        .map(|x| format!("{x:02x}"))
        .collect();
    if b.len() > BLOB_PREVIEW {
        format!("\\x{shown}\u{2026} ({} bytes)", b.len())
    } else {
        format!("\\x{shown}")
    }
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn fmt_elapsed(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        format!("{}\u{b5}s", d.as_micros())
    } else if ms < 1000.0 {
        format!("{ms:.1}ms")
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(format: Format) -> Options {
        Options {
            format,
            is_tty: true,
            echo: None,
            elapsed: None,
            null_glyph: DEFAULT_NULL_GLYPH.to_string(),
        }
    }

    fn one_row() -> QueryResult {
        QueryResult::Rows {
            columns: vec!["n".into(), "g".into()],
            rows: vec![vec![Cell::Int(7), Cell::Text("hi".into())]],
            truncated: None,
        }
    }

    #[test]
    fn sanitize_neutralises_escape() {
        let evil = "\x1b[?1049h";
        let clean = sanitize(evil);
        assert!(!clean.contains('\x1b'));
        assert!(clean.starts_with("\\x1b"));
    }

    #[test]
    fn null_distinct_from_empty() {
        assert_eq!(display_cell(&Cell::Null, "(null)"), "(null)");
        assert_eq!(display_cell(&Cell::Null, "\u{2205}"), "\u{2205}");
        assert_eq!(display_cell(&Cell::Text(String::new()), "(null)"), "");
    }

    #[test]
    fn format_produces_table_text() {
        let s = format(&one_row(), &opts(Format::Table));
        assert!(s.contains('7') && s.contains("hi") && s.contains('n'));
        assert!(s.contains("(1 row"));
    }

    #[test]
    fn markdown_format() {
        let s = format(&one_row(), &opts(Format::Markdown));
        assert!(s.contains("| n | g |"));
        assert!(s.contains("| --- |"));
        assert!(s.contains("| 7 | hi |"));
    }

    #[test]
    fn ndjson_format_one_object_per_line() {
        let s = format(&one_row(), &opts(Format::Ndjson));
        assert_eq!(s.trim(), r#"{"n":7,"g":"hi"}"#);
    }

    #[test]
    fn json_multi_result_shape() {
        let results = vec![QueryResult::Affected { changes: 2 }, one_row()];
        let s = format_all(&results, &opts(Format::Json));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["affected"], 2);
        assert_eq!(v[1]["rows"][0]["n"], 7);
    }

    #[test]
    fn bool_and_json_cells() {
        assert_eq!(display_cell(&Cell::Bool(true), "(null)"), "true");
        assert_eq!(cell_to_json(&Cell::Bool(false)), serde_json::json!(false));
        let j = Cell::Json(serde_json::json!({"a": 1}));
        assert_eq!(cell_to_json(&j), serde_json::json!({"a": 1}));
    }
}

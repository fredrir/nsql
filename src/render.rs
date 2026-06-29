use crate::cli::{Cli, FormatArg};
use crate::db::{Cell, QueryResult};
use crate::{pager, util};
use anyhow::Result;
use std::fmt::Write as _;

const NULL_GLYPH: &str = "(null)";
const BLOB_PREVIEW: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Auto,
    Table,
    Expanded,
    Tsv,
    Csv,
    Json,
}

pub struct Options {
    pub format: Format,
    pub is_tty: bool,
    pub echo: Option<String>,
    pub elapsed: Option<std::time::Duration>,
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
                None => Format::Auto,
            }
        };
        Options {
            format,
            is_tty,
            echo,
            elapsed,
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

pub fn print(result: &QueryResult, opts: &Options) -> Result<()> {
    pager::emit(&format(result, opts), opts.is_tty)
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
            match fmt {
                Format::Table => render_table(&mut out, columns, rows),
                Format::Expanded => render_expanded(&mut out, columns, rows),
                Format::Tsv => render_sv(&mut out, columns, rows, '\t'),
                Format::Csv => render_csv(&mut out, columns, rows),
                Format::Json => render_json(&mut out, columns, rows),
                Format::Auto => unreachable!(),
            }
            let n = rows.len();
            if human {
                let suffix = if truncated.is_some() { " (capped, ,a for all)" } else { "" };
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

fn render_table(out: &mut String, columns: &[String], rows: &[Vec<Cell>]) {
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
        t.add_row(row.iter().map(display_cell));
    }
    let _ = writeln!(out, "{t}");
}

fn render_expanded(out: &mut String, columns: &[String], rows: &[Vec<Cell>]) {
    let label_w = columns.iter().map(|c| c.chars().count()).max().unwrap_or(0);
    for (i, row) in rows.iter().enumerate() {
        let _ = writeln!(out, "-[ row {} ]-", i + 1);
        for (col, cell) in columns.iter().zip(row) {
            let _ = writeln!(
                out,
                "{:>label_w$} | {}",
                sanitize(col),
                display_cell(cell),
                label_w = label_w
            );
        }
    }
}

fn render_sv(out: &mut String, columns: &[String], rows: &[Vec<Cell>], sep: char) {
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
                .map(display_cell)
                .collect::<Vec<_>>()
                .join(&sep.to_string())
        );
    }
}

fn render_csv(out: &mut String, columns: &[String], rows: &[Vec<Cell>]) {
    let _ = writeln!(
        out,
        "{}",
        columns.iter().map(|c| csv_field(&sanitize(c))).collect::<Vec<_>>().join(",")
    );
    for row in rows {
        let _ = writeln!(
            out,
            "{}",
            row.iter()
                .map(|c| csv_field(&display_cell(c)))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
}

fn render_json(out: &mut String, columns: &[String], rows: &[Vec<Cell>]) {
    let arr: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let mut map = serde_json::Map::new();
            for (col, cell) in columns.iter().zip(row) {
                map.insert(col.clone(), cell_to_json(cell));
            }
            serde_json::Value::Object(map)
        })
        .collect();
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&serde_json::Value::Array(arr))
            .unwrap_or_else(|_| "[]".into())
    );
}

fn cell_to_json(cell: &Cell) -> serde_json::Value {
    use serde_json::Value;
    match cell {
        Cell::Null => Value::Null,
        Cell::Int(i) => Value::from(*i),
        Cell::Real(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Cell::Text(s) => Value::String(sanitize(s)),
        Cell::Bytes(b) => Value::String(blob_hex(b)),
    }
}

fn display_cell(cell: &Cell) -> String {
    match cell {
        Cell::Null => NULL_GLYPH.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::Real(f) => f.to_string(),
        Cell::Text(s) => sanitize(s),
        Cell::Bytes(b) => blob_hex(b),
    }
}

fn blob_hex(b: &[u8]) -> String {
    let shown: String = b.iter().take(BLOB_PREVIEW).map(|x| format!("{x:02x}")).collect();
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

    #[test]
    fn sanitize_neutralises_escape() {
        let evil = "\x1b[?1049h";
        let clean = sanitize(evil);
        assert!(!clean.contains('\x1b'));
        assert!(clean.starts_with("\\x1b"));
    }

    #[test]
    fn null_distinct_from_empty() {
        assert_eq!(display_cell(&Cell::Null), "(null)");
        assert_eq!(display_cell(&Cell::Text(String::new())), "");
    }

    #[test]
    fn format_produces_table_text() {
        let result = crate::db::QueryResult::Rows {
            columns: vec!["n".into(), "g".into()],
            rows: vec![vec![Cell::Int(7), Cell::Text("hi".into())]],
            truncated: None,
        };
        let opts = Options {
            format: Format::Table,
            is_tty: true,
            echo: None,
            elapsed: None,
        };
        let s = format(&result, &opts);
        assert!(s.contains('7') && s.contains("hi") && s.contains('n'));
        assert!(s.contains("(1 row"));
    }
}

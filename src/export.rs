//! `--out FILE`: stream the full result to a file. Bypasses the row cap and
//! the pager. Postgres single-SELECT CSV goes through COPY (server-side
//! streaming); SQLite streams row by row; everything else falls back to a
//! buffered write.

use crate::cli::FormatArg;
use crate::db::{self, Cell, Conn, QueryResult, RunOpts};
use crate::sql;
use anyhow::{bail, Context, Result};
use std::io::Write;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
    Ndjson,
}

pub fn resolve_format(cli_format: Option<&FormatArg>, path: &str) -> Result<ExportFormat> {
    if let Some(f) = cli_format {
        return match f {
            FormatArg::Csv => Ok(ExportFormat::Csv),
            FormatArg::Tsv => Ok(ExportFormat::Tsv),
            FormatArg::Json => Ok(ExportFormat::Json),
            FormatArg::Ndjson => Ok(ExportFormat::Ndjson),
            FormatArg::Table | FormatArg::Markdown => {
                bail!("--out supports csv, tsv, json, ndjson")
            }
        };
    }
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("csv") => Ok(ExportFormat::Csv),
        Some("tsv") | Some("tab") => Ok(ExportFormat::Tsv),
        Some("json") => Ok(ExportFormat::Json),
        Some("ndjson") | Some("jsonl") => Ok(ExportFormat::Ndjson),
        _ => Ok(ExportFormat::Csv),
    }
}

pub fn run(
    profile: &crate::config::Profile,
    sql_text: &str,
    path: &str,
    fmt: ExportFormat,
    null_glyph: &str,
) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    let mut w = std::io::BufWriter::new(file);

    let mut conn = db::connect(profile)?;
    crate::cancel::reset();
    let _cancel = conn.cancel_closure().map(crate::cancel::arm);

    let stmts = sql::split_statements(sql_text, conn.dialect());
    let single_query = stmts.len() == 1
        && matches!(
            sql::first_keyword(&stmts[0], conn.dialect()).as_str(),
            "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW"
        );

    match &mut conn {
        // Server-side streaming: never buffers, row cap irrelevant.
        Conn::Pg(pg) if fmt == ExportFormat::Csv && single_query => {
            let copy = format!("COPY ({}) TO STDOUT (FORMAT csv, HEADER true)", stmts[0]);
            let mut rdr = pg
                .client
                .copy_out(copy.as_str())
                .context("streaming with COPY")?;
            std::io::copy(&mut rdr, &mut w).with_context(|| format!("writing {path}"))?;
        }
        // Client-side streaming for sqlite: rows never accumulate in memory.
        Conn::Sqlite(c) if single_query => {
            stream_sqlite(c, &stmts[0], &mut w, fmt, null_glyph)?;
        }
        conn => {
            let out = db::run_on(
                conn,
                sql_text,
                &RunOpts {
                    cap: usize::MAX,
                    typed: matches!(fmt, ExportFormat::Json | ExportFormat::Ndjson),
                },
            )?;
            write_buffered(&out.results, &mut w, fmt, null_glyph)?;
        }
    }

    w.flush().with_context(|| format!("writing {path}"))?;
    eprintln!("nsql: wrote {path}");
    Ok(())
}

fn stream_sqlite(
    conn: &rusqlite::Connection,
    stmt_text: &str,
    w: &mut impl Write,
    fmt: ExportFormat,
    null_glyph: &str,
) -> Result<()> {
    use rusqlite::types::ValueRef;

    let mut stmt = conn.prepare(stmt_text).context("preparing SQL")?;
    let columns: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect();

    write_header(w, &columns, fmt)?;
    let ncol = columns.len();
    let mut first = true;
    let mut q = stmt.query([])?;
    while let Some(r) = q.next()? {
        let mut cells = Vec::with_capacity(ncol);
        for i in 0..ncol {
            cells.push(match r.get_ref(i)? {
                ValueRef::Null => Cell::Null,
                ValueRef::Integer(i) => Cell::Int(i),
                ValueRef::Real(f) => Cell::Real(f),
                ValueRef::Text(b) => Cell::Text(String::from_utf8_lossy(b).into_owned()),
                ValueRef::Blob(b) => Cell::Bytes(b.to_vec()),
            });
        }
        write_row(w, &columns, &cells, fmt, null_glyph, first)?;
        first = false;
    }
    write_footer(w, fmt, first)?;
    Ok(())
}

fn write_buffered(
    results: &[QueryResult],
    w: &mut impl Write,
    fmt: ExportFormat,
    null_glyph: &str,
) -> Result<()> {
    // One valid document per file: JSON gets a single array across every
    // row-returning result (a write-only run exports `[]`); CSV/TSV separate
    // result sets with a blank line.
    if fmt == ExportFormat::Json {
        write!(w, "[")?;
        let mut first = true;
        for result in results {
            if let QueryResult::Rows { columns, rows, .. } = result {
                for row in rows {
                    write_row(w, columns, row, fmt, null_glyph, first)?;
                    first = false;
                }
            }
        }
        write_footer(w, fmt, first)?;
        return Ok(());
    }

    let mut first_set = true;
    for result in results {
        let QueryResult::Rows { columns, rows, .. } = result else {
            continue;
        };
        if !first_set && matches!(fmt, ExportFormat::Csv | ExportFormat::Tsv) {
            writeln!(w)?;
        }
        first_set = false;
        write_header(w, columns, fmt)?;
        let mut first = true;
        for row in rows {
            write_row(w, columns, row, fmt, null_glyph, first)?;
            first = false;
        }
        write_footer(w, fmt, first)?;
    }
    Ok(())
}

fn write_header(w: &mut impl Write, columns: &[String], fmt: ExportFormat) -> Result<()> {
    match fmt {
        ExportFormat::Csv => writeln!(
            w,
            "{}",
            columns.iter().map(|c| csv(c)).collect::<Vec<_>>().join(",")
        )?,
        ExportFormat::Tsv => writeln!(w, "{}", columns.join("\t"))?,
        ExportFormat::Json => write!(w, "[")?,
        ExportFormat::Ndjson => {}
    }
    Ok(())
}

fn write_row(
    w: &mut impl Write,
    columns: &[String],
    cells: &[Cell],
    fmt: ExportFormat,
    null_glyph: &str,
    first: bool,
) -> Result<()> {
    match fmt {
        ExportFormat::Csv => writeln!(
            w,
            "{}",
            cells
                .iter()
                .map(|c| csv(&display(c, null_glyph)))
                .collect::<Vec<_>>()
                .join(",")
        )?,
        ExportFormat::Tsv => writeln!(
            w,
            "{}",
            cells
                .iter()
                .map(|c| display(c, null_glyph))
                .collect::<Vec<_>>()
                .join("\t")
        )?,
        ExportFormat::Json | ExportFormat::Ndjson => {
            let mut map = serde_json::Map::new();
            for (col, cell) in columns.iter().zip(cells) {
                map.insert(col.clone(), cell_json(cell));
            }
            let line = serde_json::to_string(&serde_json::Value::Object(map))?;
            match fmt {
                ExportFormat::Json => {
                    if first {
                        write!(w, "\n{line}")?;
                    } else {
                        write!(w, ",\n{line}")?;
                    }
                }
                _ => writeln!(w, "{line}")?,
            }
        }
    }
    Ok(())
}

fn write_footer(w: &mut impl Write, fmt: ExportFormat, empty: bool) -> Result<()> {
    if fmt == ExportFormat::Json {
        if empty {
            writeln!(w, "]")?;
        } else {
            writeln!(w, "\n]")?;
        }
    }
    Ok(())
}

fn display(cell: &Cell, null_glyph: &str) -> String {
    match cell {
        Cell::Null => null_glyph.to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::Real(f) => f.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Bytes(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
        Cell::Json(v) => v.to_string(),
    }
}

fn cell_json(cell: &Cell) -> serde_json::Value {
    use serde_json::Value;
    match cell {
        Cell::Null => Value::Null,
        Cell::Bool(b) => Value::Bool(*b),
        Cell::Int(i) => Value::from(*i),
        Cell::Real(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Cell::Text(s) => Value::String(s.clone()),
        Cell::Bytes(b) => Value::String(b.iter().map(|x| format!("{x:02x}")).collect()),
        Cell::Json(v) => v.clone(),
    }
}

// Exports write raw values (no terminal escape-sanitising): files are data,
// not terminal output.
fn csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

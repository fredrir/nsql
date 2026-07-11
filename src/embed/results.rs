use super::NvimWriter;
use crate::{db, render};
use anyhow::Result;
use nvim_rs::{Neovim, Value};

pub(super) struct Outcome {
    pub(super) json: Option<String>,
    pub(super) csv: Option<String>,
    pub(super) persist: Option<String>,
}

pub(super) fn render_outcome(res: &Result<db::QueryResult>, sql: &str) -> Outcome {
    let fmt = |result, f| {
        render::format(
            result,
            &render::Options {
                format: f,
                is_tty: false,
                echo: None,
                elapsed: None,
                null_glyph: "(null)".to_string(),
            },
        )
    };
    match res {
        Ok(result) => Outcome {
            json: Some(fmt(result, render::Format::Json)),
            csv: Some(fmt(result, render::Format::Csv)),
            persist: Some(render_persist(result, sql)),
        },
        Err(_) => Outcome {
            json: None,
            csv: None,
            persist: None,
        },
    }
}

fn render_persist(result: &db::QueryResult, sql: &str) -> String {
    use std::fmt::Write;
    const SHOW: usize = 10;
    let mut out = String::new();
    for l in sql.lines() {
        let t = l.trim();
        if !t.is_empty() {
            let _ = writeln!(out, "-- {t}");
        }
    }
    match result {
        db::QueryResult::Affected { changes } => {
            let _ = writeln!(out, "-- ✓ {changes} row(s) affected");
        }
        db::QueryResult::Rows {
            columns,
            rows,
            truncated,
        } => {
            if columns.is_empty() {
                let _ = writeln!(out, "-- (0 rows)");
                return out;
            }
            const MAXW: usize = 40;
            let ncol = columns.len();
            let total = rows.len();
            let show = &rows[..total.min(SHOW)];
            let disp: Vec<Vec<String>> = show
                .iter()
                .map(|r| (0..ncol).map(|i| buf_cell(r.get(i))).collect())
                .collect();
            let mut widths: Vec<usize> = columns
                .iter()
                .map(|c| c.chars().count().min(MAXW))
                .collect();
            for row in &disp {
                for (i, c) in row.iter().enumerate() {
                    if i < ncol {
                        widths[i] = widths[i].max(c.chars().count()).min(MAXW);
                    }
                }
            }
            let sep = "  ";
            let row_line = |cells: &dyn Fn(usize) -> String| {
                let mut line = String::new();
                for (i, w) in widths.iter().enumerate().take(ncol) {
                    let s = truncate_disp(&cells(i), *w);
                    line.push_str(&s);
                    push_pad(&mut line, w.saturating_sub(s.chars().count()));
                    if i + 1 < ncol {
                        line.push_str(sep);
                    }
                }
                line.trim_end().to_string()
            };
            let _ = writeln!(out, "{}", row_line(&|i| columns[i].clone()));
            for r in &disp {
                let _ = writeln!(out, "{}", row_line(&|i| r[i].clone()));
            }
            if truncated.is_some() {
                let _ = writeln!(
                    out,
                    "-- first {total} rows (capped) · ,a or `nsql -e` for all"
                );
            } else if total > show.len() {
                let _ = writeln!(
                    out,
                    "-- {total} rows ({} shown) · `nsql -e` for all",
                    show.len()
                );
            } else {
                let _ = writeln!(out, "-- {total} row{}", if total == 1 { "" } else { "s" });
            }
        }
    }
    out
}

pub(super) struct CellMark {
    pub(super) line: usize,
    pub(super) col: usize,
    pub(super) end: usize,
    pub(super) hl: &'static str,
}

pub(super) fn format_for_buffer(
    res: &Result<db::QueryResult>,
) -> (String, Vec<String>, Vec<CellMark>) {
    let result = match res {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("  error: {}", first_line(&format!("{e:#}")));
            let mark = CellMark {
                line: 0,
                col: 0,
                end: msg.len(),
                hl: "ErrorMsg",
            };
            return (String::new(), vec![msg], vec![mark]);
        }
    };
    let (columns, rows, truncated) = match result {
        db::QueryResult::Rows {
            columns,
            rows,
            truncated,
        } => (columns, rows, *truncated),
        db::QueryResult::Affected { changes } => {
            return (
                String::new(),
                vec![format!("  ✓ OK — {changes} row(s) affected")],
                Vec::new(),
            );
        }
    };
    if columns.is_empty() {
        return (String::new(), vec!["  (0 rows)".to_string()], Vec::new());
    }

    const MAXW: usize = 60;
    const MAX_BUF_ROWS: usize = 2000;
    let total = rows.len();
    let rows: &[Vec<db::Cell>] = if total > MAX_BUF_ROWS {
        &rows[..MAX_BUF_ROWS]
    } else {
        rows
    };
    let ncol = columns.len();
    let disp: Vec<Vec<String>> = rows
        .iter()
        .map(|r| (0..ncol).map(|i| buf_cell(r.get(i))).collect())
        .collect();
    let mut widths: Vec<usize> = columns
        .iter()
        .map(|c| c.chars().count().min(MAXW))
        .collect();
    for row in &disp {
        for (i, c) in row.iter().enumerate() {
            if i < ncol {
                widths[i] = widths[i].max(c.chars().count()).min(MAXW);
            }
        }
    }

    let sep = "  ";

    let mut header = String::new();
    for (i, c) in columns.iter().enumerate() {
        let shown = truncate_disp(c, widths[i]);
        header.push_str(&shown);
        push_pad(&mut header, widths[i].saturating_sub(shown.chars().count()));
        if i + 1 < ncol {
            header.push_str(sep);
        }
    }

    let mut lines: Vec<String> = Vec::with_capacity(rows.len() + 1);
    let mut marks: Vec<CellMark> = Vec::new();

    for (ri, row) in rows.iter().enumerate() {
        let mut line = String::new();
        for i in 0..ncol {
            let shown = truncate_disp(&disp[ri][i], widths[i]);
            let start = line.len();
            line.push_str(&shown);
            marks.push(CellMark {
                line: ri,
                col: start,
                end: line.len(),
                hl: classify(&shown, row.get(i)),
            });
            push_pad(&mut line, widths[i].saturating_sub(shown.chars().count()));
            if i + 1 < ncol {
                line.push_str(sep);
            }
        }
        lines.push(line);
    }

    let shown = rows.len();
    let more = truncated.is_some() || shown < total;
    let footer = if more {
        format!("{shown}+ rows")
    } else {
        format!("{total} row{}", if total == 1 { "" } else { "s" })
    };
    let hl = if more { "WarningMsg" } else { "Comment" };
    marks.push(CellMark {
        line: lines.len(),
        col: 0,
        end: footer.len(),
        hl,
    });
    lines.push(footer);

    (header, lines, marks)
}

fn push_pad(s: &mut String, n: usize) {
    for _ in 0..n {
        s.push(' ');
    }
}

fn buf_cell(c: Option<&db::Cell>) -> String {
    match c {
        None | Some(db::Cell::Null) => "∅".to_string(),
        Some(db::Cell::Bool(b)) => b.to_string(),
        Some(db::Cell::Int(i)) => i.to_string(),
        Some(db::Cell::Real(f)) => f.to_string(),
        Some(db::Cell::Text(s)) => render::sanitize(s),
        Some(db::Cell::Bytes(b)) => format!("\\x{}", hex_prefix(b)),
        Some(db::Cell::Json(v)) => render::sanitize(&v.to_string()),
    }
}

fn hex_prefix(b: &[u8]) -> String {
    let mut out = String::new();
    for byte in b.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    if b.len() > 8 {
        out.push('…');
    }
    out
}

fn truncate_disp(s: &str, w: usize) -> String {
    if s.chars().count() > w {
        let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

fn classify(shown: &str, c: Option<&db::Cell>) -> &'static str {
    match c {
        None | Some(db::Cell::Null) => "Comment",
        Some(db::Cell::Bool(_)) => "Boolean",
        Some(db::Cell::Int(_)) | Some(db::Cell::Real(_)) => "Number",
        Some(db::Cell::Bytes(_)) | Some(db::Cell::Json(_)) => "Special",
        Some(db::Cell::Text(_)) => classify_text(shown),
    }
}

fn classify_text(s: &str) -> &'static str {
    let t = s.trim();
    if t.is_empty() {
        return "String";
    }
    if matches!(t, "t" | "f" | "true" | "false" | "TRUE" | "FALSE") {
        return "Boolean";
    }
    if t.parse::<f64>().is_ok() {
        return "Number";
    }
    if looks_like_date(t) {
        return "Constant";
    }
    "String"
}

fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

pub(super) fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or("")
        .to_string();
    if line.chars().count() > 60 {
        format!("{}…", line.chars().take(60).collect::<String>())
    } else {
        line
    }
}

pub(super) async fn write_results(
    nvim: &Neovim<NvimWriter>,
    rbuf: i64,
    header: &str,
    lines: &[String],
    marks: &[CellMark],
) {
    if rbuf < 0 {
        return;
    }
    let lines_v = Value::Array(lines.iter().map(|l| Value::from(l.as_str())).collect());
    let marks_v = Value::Array(
        marks
            .iter()
            .map(|m| {
                Value::Array(vec![
                    Value::from(m.line as i64),
                    Value::from(m.col as i64),
                    Value::from(m.end as i64),
                    Value::from(m.hl),
                ])
            })
            .collect(),
    );
    let _ = nvim
        .exec_lua(
            WRITE_RESULTS_LUA,
            vec![Value::from(rbuf), lines_v, marks_v, Value::from(header)],
        )
        .await;
}

pub(super) const SETUP_RESULTS_LUA: &str = r#"
local results_rows = ...
local ok, rbuf = pcall(vim.api.nvim_create_buf, false, true)
if not ok then return -1 end
vim.bo[rbuf].buftype = 'nofile'
vim.bo[rbuf].bufhidden = 'hide'
vim.bo[rbuf].swapfile = false
vim.bo[rbuf].modifiable = false
pcall(function() vim.o.cmdheight = 0 end)  -- reclaim the bottom row (nvim 0.8+)
vim.g.nsql_ewin = vim.api.nvim_get_current_win()
vim.g.nsql_rbuf = rbuf
vim.g.nsql_rrows = results_rows

-- q / <Esc> in the results buffer hop back to the editor (the same toggle key).
local function back()
  if vim.g.nsql_ewin and vim.api.nvim_win_is_valid(vim.g.nsql_ewin) then
    pcall(vim.api.nvim_set_current_win, vim.g.nsql_ewin)
  end
end
local bo = { buffer = rbuf, silent = true }
pcall(vim.keymap.set, 'n', 'q', back, bo)
pcall(vim.keymap.set, 'n', '<Esc>', back, bo)

-- Any yank in the results buffer → system clipboard via nsql (OSC 52).
pcall(vim.api.nvim_create_autocmd, 'TextYankPost', {
  buffer = rbuf,
  callback = function()
    local ch = vim.g.nsql_chan
    local ev = vim.v.event
    local txt = table.concat((ev and ev.regcontents) or {}, '\n')
    if ch and txt ~= '' then pcall(vim.rpcnotify, ch, 'nsql_yank', txt) end
  end,
})

-- Open the results split below the editor on demand (first result). Idempotent.
function _G.nsql_ensure_rwin()
  local rw = vim.g.nsql_rwin
  if rw and vim.api.nvim_win_is_valid(rw) and vim.api.nvim_win_get_buf(rw) == rbuf then
    return rw
  end
  local ew = vim.api.nvim_get_current_win()
  vim.cmd('botright ' .. (vim.g.nsql_rrows or 10) .. 'split')
  rw = vim.api.nvim_get_current_win()
  vim.api.nvim_win_set_buf(rw, rbuf)
  vim.wo[rw].number = false
  vim.wo[rw].relativenumber = false
  vim.wo[rw].signcolumn = 'no'
  vim.wo[rw].foldcolumn = '0'
  vim.wo[rw].winfixheight = true
  vim.wo[rw].cursorline = true
  vim.wo[rw].wrap = false
  pcall(vim.api.nvim_set_current_win, ew)  -- focus stays in the editor
  vim.g.nsql_rwin = rw
  return rw
end
return rbuf
"#;

const WRITE_RESULTS_LUA: &str = r#"
local rbuf, lines, marks, header = ...
if not vim.api.nvim_buf_is_valid(rbuf) then return end
vim.bo[rbuf].modifiable = true
vim.api.nvim_buf_set_lines(rbuf, 0, -1, false, lines)
vim.bo[rbuf].modifiable = false
local ns = vim.api.nvim_create_namespace('nsql_types')
vim.api.nvim_buf_clear_namespace(rbuf, ns, 0, -1)
for _, m in ipairs(marks) do
  pcall(vim.api.nvim_buf_set_extmark, rbuf, ns, m[1], m[2], { end_col = m[3], hl_group = m[4] })
end
local rw = _G.nsql_ensure_rwin and _G.nsql_ensure_rwin() or nil
local ew = vim.g.nsql_ewin
local mainbar = vim.g.nsql_mainbar or ''
if rw and vim.api.nvim_win_is_valid(rw) then
  pcall(vim.api.nvim_win_set_cursor, rw, { 1, 0 })
  local function esc(s) return (s:gsub('%%', '%%%%')) end
  -- Editor statusline: the column HEADER on a table result, else the main header.
  if ew and vim.api.nvim_win_is_valid(ew) then
    vim.wo[ew].statusline = (header ~= '') and ('%<' .. esc(header)) or mainbar
  end
  -- Bottom statusline: the MAIN HEADER (moved down once a table shows).
  vim.wo[rw].statusline = mainbar
end
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn render_outcome_exports_and_persist() {
        let prof = crate::config::Profile {
            name: "t".into(),
            url: "sqlite::memory:".into(),
            prod: false,
            readonly: false,
            no_history: false,
            ssh: None,
        };
        let sql = "select 7 as answer, null as n";
        let res = db::run(&prof, sql, false);
        let o = render_outcome(&res, sql);

        let json = o.json.expect("json");
        assert!(json.contains("answer") && json.contains('7'));
        let csv = o.csv.expect("csv");
        assert!(csv.contains("answer"));
        let persist = o.persist.expect("persist");
        assert!(
            persist.contains("-- select 7 as answer"),
            "persist must echo the query"
        );
        assert!(persist.contains('7') && persist.contains("answer"));
        assert!(
            persist.contains("-- 1 row"),
            "persist needs a concise summary line"
        );
    }

    #[test]
    fn render_outcome_error_does_not_persist() {
        let err: Result<db::QueryResult> = Err(anyhow!("kaboom"));
        let o = render_outcome(&err, "select 1");
        assert!(
            o.persist.is_none(),
            "an error must not overwrite the persisted result"
        );
        assert!(o.json.is_none() && o.csv.is_none());
    }

    #[test]
    fn buffer_format_is_borderless_and_type_coloured() {
        let prof = crate::config::Profile {
            name: "t".into(),
            url: "sqlite::memory:".into(),
            prod: false,
            readonly: false,
            no_history: false,
            ssh: None,
        };
        let sql = "select 42 as qty, 'widget' as name, '2026-06-10' as day, null as note";
        let res = db::run(&prof, sql, false);
        let (header, lines, marks) = format_for_buffer(&res);

        assert!(header.contains("qty") && header.contains("name") && header.contains("note"));

        for l in std::iter::once(&header).chain(lines.iter()) {
            assert!(
                !l.contains('│') && !l.contains('─') && !l.contains('┌') && !l.contains('|'),
                "results must be borderless for clean copy, got: {l:?}"
            );
        }
        assert!(lines[0].contains("42") && lines[0].contains("widget"));
        assert!(
            lines.iter().any(|l| l.contains('∅')),
            "NULL needs a distinct glyph"
        );

        assert_eq!(classify("42", Some(&db::Cell::Int(42))), "Number");
        assert_eq!(
            classify("widget", Some(&db::Cell::Text("widget".into()))),
            "String"
        );
        assert_eq!(
            classify("2026-06-10", Some(&db::Cell::Text("2026-06-10".into()))),
            "Constant"
        );
        assert_eq!(classify("99", Some(&db::Cell::Text("99".into()))), "Number");
        assert_eq!(classify("", Some(&db::Cell::Null)), "Comment");
        assert!(marks.iter().any(|m| m.hl == "Number" && m.line == 0));
    }

    #[test]
    fn format_for_buffer_error_shows_message() {
        let err: Result<db::QueryResult> = Err(anyhow!("kaboom"));
        let (header, lines, marks) = format_for_buffer(&err);
        assert!(header.is_empty(), "error has no column header");
        assert!(lines[0].contains("error") && lines[0].contains("kaboom"));
        assert_eq!(marks[0].hl, "ErrorMsg");
    }
}

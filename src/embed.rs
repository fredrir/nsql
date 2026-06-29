use crate::config::{Paths, Profile};
use crate::{db, editor, history, render, util};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use nvim_rs::compat::tokio::Compat;
use nvim_rs::{Handler, Neovim, UiAttachOptions, Value};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
};
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use std::collections::HashMap;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;

type NvimWriter = Compat<ChildStdin>;

pub fn compose(paths: &Paths, profile: &Profile, portable: bool) -> Result<()> {
    editor::write_inject(paths)?;
    let scratch = paths.scratch_for(&profile.name);
    let prior = std::fs::read_to_string(&scratch).unwrap_or_default();
    let initial = editor::strip_header(&prior);

    let tmp = util::secure_tempfile("nsql", "sql")?;
    std::fs::write(&tmp, &initial).with_context(|| format!("writing {}", tmp.display()))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting embed runtime")?;

    let res = rt.block_on(run_session(paths, &tmp, profile, portable));
    rt.shutdown_background();
    let _ = disable_raw_mode();
    res?;

    let edited = std::fs::read_to_string(&tmp).unwrap_or_default();
    std::fs::remove_file(&tmp).ok();
    let body = editor::strip_header(&edited);
    if let Err(e) = editor::persist_scratch(&scratch, &body) {
        eprintln!("nsql: warning: could not save scratch: {e:#}");
    }
    Ok(())
}

enum RunMsg {
    Run { sql: String, force: bool, all: bool },
    Export(String),
    Yank(String),
}

async fn run_session(
    paths: &Paths,
    tmp: &std::path::Path,
    profile: &Profile,
    portable: bool,
) -> Result<()> {
    let (cols, rows) = util::term_size();
    let width = cols.max(20);
    let cfg_pane = crate::config::Config::load_or_init(paths)
        .map(|c| c.pane_height())
        .unwrap_or(12);
    let max_fit = (rows.saturating_sub(3) / 2).max(4);
    let pane = cfg_pane.min(max_fit).clamp(4, 24);
    let editor_rows = pane;
    let results_rows = pane;
    let view_h = editor_rows + results_rows + 2;

    let mut cmd = Command::new("nvim");
    cmd.env("NSQL_DB", &profile.name)
        .env("NSQL_URL", util::redact_url(&profile.url))
        .env("NSQL_PROD", if profile.prod { "1" } else { "0" })
        .env("NSQL_SAFE", if profile.readonly { "1" } else { "0" })
        .env_remove("PGPASSWORD")
        .arg("--embed")
        .arg("-n")
        .arg("-i")
        .arg("NONE");
    if portable {
        cmd.arg("-u").arg(editor::portable_init_path(paths));
    }
    cmd.arg(tmp)
        .arg("-c")
        .arg("setfiletype sql")
        .arg("-c")
        .arg(format!("luafile {}", paths.inject_lua.display()));

    let (redraw_tx, mut redraw_rx) = mpsc::unbounded_channel::<Vec<Value>>();
    let (run_tx, mut run_rx) = mpsc::unbounded_channel::<RunMsg>();
    let handler = RedrawHandler {
        tx: redraw_tx,
        run_tx,
    };

    let (nvim, _io, mut child) = nvim_rs::create::tokio::new_child_cmd(&mut cmd, handler)
        .await
        .context("spawning nvim --embed")?;

    let mut opts = UiAttachOptions::new();
    opts.set_linegrid_external(true);
    opts.set_rgb(true);
    nvim.ui_attach(width as i64, view_h as i64, &opts)
        .await
        .map_err(|e| anyhow!("nvim ui_attach failed: {e}"))?;

    if let Ok(info) = nvim.get_api_info().await {
        if let Some(ch) = info.first().and_then(|v| v.as_i64()) {
            let _ = nvim.set_var("nsql_chan", Value::from(ch)).await;
        }
    }

    let rbuf: i64 = nvim
        .exec_lua(SETUP_RESULTS_LUA, vec![Value::from(results_rows as i64)])
        .await
        .ok()
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);

    enable_raw_mode().context("enabling raw mode")?;
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let _term_guard = TermGuard;
    let shutdown = Arc::new(AtomicBool::new(false));
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Event>();
    {
        let shutdown = shutdown.clone();
        std::thread::spawn(move || loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match event::poll(std::time::Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if key_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        });
    }

    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(view_h),
        },
    )
    .context("creating inline terminal")?;

    let mut grid = Grid::new(width as usize, view_h as usize);
    let mut last_shape = Shape::Block;
    let mut result_json: Option<String> = None;
    let mut result_csv: Option<String> = None;
    let mut last_persist: Option<String> = None;
    let (qdone_tx, mut qdone_rx) =
        mpsc::unbounded_channel::<(Result<db::QueryResult>, std::time::Instant, String)>();
    let mut in_flight: Option<std::time::Instant> = None;
    let mut spinner = tokio::time::interval(std::time::Duration::from_millis(120));
    let mut spin_i = 0usize;
    const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    let (schema_tx, mut schema_rx) = mpsc::unbounded_channel::<Value>();
    {
        let p = profile.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(s) = introspect_schema(&p) {
                let _ = schema_tx.send(s);
            }
        });
    }

    loop {
        let mut dirty = false;
        while let Ok(batch) = redraw_rx.try_recv() {
            apply_redraw(&mut grid, &batch);
            dirty = true;
        }
        if dirty {
            draw_grid(&mut terminal, &grid);
        }
        if grid.shape != last_shape {
            last_shape = grid.shape;
            let style = match grid.shape {
                Shape::Bar => SetCursorStyle::SteadyBar,
                Shape::Underline => SetCursorStyle::SteadyUnderScore,
                Shape::Block => SetCursorStyle::SteadyBlock,
            };
            let _ = execute!(std::io::stdout(), style);
        }

        tokio::select! {
            biased;
            _ = child.wait() => break,
            Some(msg) = run_rx.recv() => {
                match msg {
                    RunMsg::Run { sql, force, all } => {
                        if in_flight.is_some() {
                            write_results(&nvim, rbuf, "", &["  a query is already running…".to_string()], &[]).await;
                        } else if db::strip_sql_comments(&sql).trim().is_empty() {
                            write_results(&nvim, rbuf, "", &["  (nothing to run)".to_string()], &[]).await;
                        } else {
                            match db::guard(profile, &sql, force, false) {
                                Err(e) => {
                                    let msg = format!("  error: {}", first_line(&format!("{e:#}")));
                                    let mark = CellMark { line: 0, col: 0, end: msg.len(), hl: "ErrorMsg" };
                                    write_results(&nvim, rbuf, "", &[msg], &[mark]).await;
                                }
                                Ok(()) => {
                                    let started = std::time::Instant::now();
                                    in_flight = Some(started);
                                    write_results(&nvim, rbuf, "", &[format!("  running: {}…", first_line(&sql))], &[]).await;
                                    let p = profile.clone();
                                    let tx = qdone_tx.clone();
                                    tokio::task::spawn_blocking(move || {
                                        let r = db::run(&p, &sql, all);
                                        let _ = tx.send((r, started, sql));
                                    });
                                }
                            }
                        }
                    }
                    RunMsg::Export(fmt) => {
                        let data = if fmt == "csv" { result_csv.as_deref() } else { result_json.as_deref() };
                        if let Some(d) = data {
                            osc52_copy(d);
                        }
                    }
                    RunMsg::Yank(text) => osc52_copy(&text),
                }
            }
            Some((res, _started, sql)) = qdone_rx.recv() => {
                in_flight = None;
                let (header, lines, marks) = format_for_buffer(&res);
                write_results(&nvim, rbuf, &header, &lines, &marks).await;
                let outcome = render_outcome(&res, &sql);
                if outcome.json.is_some() {
                    result_json = outcome.json;
                    result_csv = outcome.csv;
                }
                if outcome.persist.is_some() {
                    last_persist = outcome.persist;
                }
                if res.is_ok() && !profile.no_history {
                    let _ = history::record(paths, &profile.name, &sql);
                }
            }
            _ = spinner.tick() => {
                if let Some(started) = in_flight {
                    let f = SPIN[spin_i % SPIN.len()];
                    spin_i = spin_i.wrapping_add(1);
                    let line = format!(
                        "  {f} running… {:.1}s   (:q to abandon and quit)",
                        started.elapsed().as_secs_f64()
                    );
                    write_results(&nvim, rbuf, "", &[line], &[]).await;
                }
            }
            Some(ev) = key_rx.recv() => {
                match ev {
                    Event::Key(k) => {
                        if let Some(input) = translate_key(k) {
                            let _ = nvim.input(&input).await;
                        }
                    }
                    Event::Paste(text) => {
                        let _ = nvim.paste(&text, false, -1).await;
                    }
                    Event::Resize(w, _h) => {
                        grid.resize(w as usize, view_h as usize);
                        let _ = nvim.ui_try_resize(w as i64, view_h as i64).await;
                        let _ = terminal.autoresize();
                        let _ = terminal.clear();
                    }
                    _ => {}
                }
            }
            Some(schema) = schema_rx.recv() => {
                let _ = nvim.exec_lua(SET_SCHEMA_LUA, vec![schema]).await;
            }
            Some(batch) = redraw_rx.recv() => {
                apply_redraw(&mut grid, &batch);
                draw_grid(&mut terminal, &grid);
            }
        }
    }

    shutdown.store(true, Ordering::Relaxed);
    if let Some(block) = &last_persist {
        let mut src: Vec<String> = block.lines().map(|l| l.to_string()).collect();
        const MAX_PERSIST: usize = 30;
        src.truncate(MAX_PERSIST);
        let lines: Vec<Line> = src.into_iter().map(Line::from).collect();
        let h = lines.len() as u16;
        let _ = terminal.insert_before(h, move |buf| {
            use ratatui::widgets::Widget;
            Paragraph::new(Text::from(lines)).render(buf.area, buf);
        });
    }
    let _ = terminal.clear();
    Ok(())
}

struct Outcome {
    json: Option<String>,
    csv: Option<String>,
    persist: Option<String>,
}

fn render_outcome(res: &Result<db::QueryResult>, sql: &str) -> Outcome {
    let fmt = |result, f| {
        render::format(
            result,
            &render::Options { format: f, is_tty: false, echo: None, elapsed: None },
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
        db::QueryResult::Rows { columns, rows, truncated } => {
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
            let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count().min(MAXW)).collect();
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
                for i in 0..ncol {
                    let s = truncate_disp(&cells(i), widths[i]);
                    line.push_str(&s);
                    push_pad(&mut line, widths[i].saturating_sub(s.chars().count()));
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
                let _ = writeln!(out, "-- first {total} rows (capped) · ,a or `nsql -e` for all");
            } else if total > show.len() {
                let _ = writeln!(out, "-- {total} rows ({} shown) · `nsql -e` for all", show.len());
            } else {
                let _ = writeln!(out, "-- {total} row{}", if total == 1 { "" } else { "s" });
            }
        }
    }
    out
}

struct CellMark {
    line: usize,
    col: usize,
    end: usize,
    hl: &'static str,
}

fn format_for_buffer(res: &Result<db::QueryResult>) -> (String, Vec<String>, Vec<CellMark>) {
    let result = match res {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("  error: {}", first_line(&format!("{e:#}")));
            let mark = CellMark { line: 0, col: 0, end: msg.len(), hl: "ErrorMsg" };
            return (String::new(), vec![msg], vec![mark]);
        }
    };
    let (columns, rows, truncated) = match result {
        db::QueryResult::Rows { columns, rows, truncated } => (columns, rows, *truncated),
        db::QueryResult::Affected { changes } => {
            return (String::new(), vec![format!("  ✓ OK — {changes} row(s) affected")], Vec::new());
        }
    };
    if columns.is_empty() {
        return (String::new(), vec!["  (0 rows)".to_string()], Vec::new());
    }

    const MAXW: usize = 60;
    const MAX_BUF_ROWS: usize = 2000;
    let total = rows.len();
    let rows: &[Vec<db::Cell>] = if total > MAX_BUF_ROWS { &rows[..MAX_BUF_ROWS] } else { rows };
    let ncol = columns.len();
    let disp: Vec<Vec<String>> = rows
        .iter()
        .map(|r| (0..ncol).map(|i| buf_cell(r.get(i))).collect())
        .collect();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count().min(MAXW)).collect();
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
    marks.push(CellMark { line: lines.len(), col: 0, end: footer.len(), hl });
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
        Some(db::Cell::Int(i)) => i.to_string(),
        Some(db::Cell::Real(f)) => f.to_string(),
        Some(db::Cell::Text(s)) => render::sanitize(s),
        Some(db::Cell::Bytes(b)) => format!("\\x{}", hex_prefix(b)),
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
        Some(db::Cell::Int(_)) | Some(db::Cell::Real(_)) => "Number",
        Some(db::Cell::Bytes(_)) => "Special",
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

async fn write_results(
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

const SETUP_RESULTS_LUA: &str = r#"
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

const SQLITE_SCHEMA_Q: &str = "SELECT m.name, p.name FROM sqlite_master m \
     JOIN pragma_table_info(m.name) p \
     WHERE m.type IN ('table','view') AND m.name NOT LIKE 'sqlite_%' \
     ORDER BY m.name, p.cid";
const PG_SCHEMA_Q: &str = "SELECT table_name, column_name FROM information_schema.columns \
     WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
     ORDER BY table_name, ordinal_position";

const SET_SCHEMA_LUA: &str = r#"
local s = ...
_G.nsql_schema = s
pcall(function()
  local ew = vim.g.nsql_ewin
  local buf = vim.g.nsql_ebuf
  if not (ew and vim.api.nvim_win_is_valid(ew)) then return end
  -- Overridable defaults; link to theme groups so tables vs columns vs keywords
  -- are all distinguishable.
  vim.api.nvim_set_hl(0, 'NsqlSchemaTable', { link = 'Type', default = true })
  vim.api.nvim_set_hl(0, 'NsqlSchemaColumn', { link = 'Identifier', default = true })

  local tset, cset = {}, {}
  for _, t in ipairs(s.tables or {}) do tset[t:lower()] = true end
  for _, c in ipairs(s.columns or {}) do cset[c:lower()] = true end
  local ns = vim.api.nvim_create_namespace('nsql_schema_hl')

  -- TREESITTER: walk the parse tree, colour leaf identifier nodes that match the
  -- schema, skipping string / comment / literal subtrees. Returns true if it ran.
  local skip = { string = true, comment = true, literal = true, string_literal = true,
                 marginalia = true, dollar_quote = true, ['string_content'] = true }
  local function ts_paint()
    if not buf or not vim.api.nvim_buf_is_valid(buf) then return false end
    local ok, parser = pcall(vim.treesitter.get_parser, buf, 'sql')
    if not ok or not parser then return false end
    local okp, trees = pcall(function() return parser:parse() end)
    if not okp or not trees[1] then return false end
    vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
    local function walk(node, in_skip)
      local sk = in_skip or skip[node:type()] or false
      local has_named = false
      for child in node:iter_children() do
        if child:named() then has_named = true; walk(child, sk) end
      end
      if not has_named and not sk then
        local txt = vim.treesitter.get_node_text(node, buf)
        if txt and txt:match('^[%w_]+$') then
          local low = txt:lower()
          local hl = tset[low] and 'NsqlSchemaTable' or (cset[low] and 'NsqlSchemaColumn' or nil)
          if hl then
            local sr, sc, er, ec = node:range()
            pcall(vim.api.nvim_buf_set_extmark, buf, ns, sr, sc,
              { end_row = er, end_col = ec, hl_group = hl, priority = 150 })
          end
        end
      end
    end
    walk(trees[1]:root(), false)
    return true
  end

  -- FALLBACK: matchadd (whole-word, case-insensitive; colours everywhere including
  -- strings/comments — acceptable for a scratch buffer with no sql parser).
  local function matchadd_paint()
    local function pat(list, cap)
      local parts = {}
      for i = 1, math.min(#list, cap) do parts[#parts + 1] = (list[i]:gsub('\\', '\\\\')) end
      if #parts == 0 then return nil end
      return '\\c\\V\\<\\%(' .. table.concat(parts, '\\|') .. '\\)\\>'
    end
    local tp, cp = pat(s.tables or {}, 1000), pat(s.columns or {}, 2000)
    vim.api.nvim_win_call(ew, function()
      for _, m in ipairs(vim.fn.getmatches()) do
        if m.group == 'NsqlSchemaTable' or m.group == 'NsqlSchemaColumn' then
          pcall(vim.fn.matchdelete, m.id)
        end
      end
      if cp then pcall(vim.fn.matchadd, 'NsqlSchemaColumn', cp, 10) end
      if tp then pcall(vim.fn.matchadd, 'NsqlSchemaTable', tp, 11) end
    end)
  end

  if ts_paint() then
    -- keep it fresh on edits, debounced (coalesce to one repaint per 150ms).
    local pending = false
    vim.api.nvim_create_autocmd({ 'TextChanged', 'TextChangedI' }, {
      buffer = buf,
      callback = function()
        if pending then return end
        pending = true
        vim.defer_fn(function() pending = false; pcall(ts_paint) end, 150)
      end,
    })
  else
    matchadd_paint()
  end
end)
"#;

fn introspect_schema(profile: &Profile) -> Option<Value> {
    let q = match profile.scheme() {
        "sqlite" => SQLITE_SCHEMA_Q,
        "postgres" | "postgresql" => PG_SCHEMA_Q,
        _ => return None,
    };
    let rows = match db::run(profile, q, true).ok()? {
        db::QueryResult::Rows { rows, .. } => rows,
        _ => return None,
    };
    let mut tables: Vec<String> = Vec::new();
    let mut by_table: Vec<(String, Vec<String>)> = Vec::new();
    let mut all_cols: Vec<String> = Vec::new();
    let mut seen_col = std::collections::HashSet::new();
    for row in &rows {
        let t = cell_str(row.first());
        let c = cell_str(row.get(1));
        if t.is_empty() {
            continue;
        }
        if by_table.last().map(|(n, _)| n != &t).unwrap_or(true) {
            tables.push(t.clone());
            by_table.push((t.clone(), Vec::new()));
        }
        if !c.is_empty() {
            by_table.last_mut().unwrap().1.push(c.clone());
            if seen_col.insert(c.clone()) {
                all_cols.push(c);
            }
        }
    }
    if tables.is_empty() {
        return None;
    }
    let arr = |v: &[String]| Value::Array(v.iter().map(|s| Value::from(s.as_str())).collect());
    let by_table_v = Value::Map(
        by_table
            .iter()
            .map(|(t, cols)| (Value::from(t.as_str()), arr(cols)))
            .collect(),
    );
    Some(Value::Map(vec![
        (Value::from("tables"), arr(&tables)),
        (Value::from("columns"), arr(&all_cols)),
        (Value::from("by_table"), by_table_v),
    ]))
}

fn cell_str(c: Option<&db::Cell>) -> String {
    match c {
        Some(db::Cell::Text(s)) => s.clone(),
        Some(db::Cell::Int(i)) => i.to_string(),
        Some(db::Cell::Real(f)) => f.to_string(),
        _ => String::new(),
    }
}

struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            SetCursorStyle::DefaultUserShape
        );
        let _ = disable_raw_mode();
    }
}

fn first_line(s: &str) -> String {
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

fn osc52_copy(text: &str) {
    use std::io::Write;
    let seq = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

fn base64_encode(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn translate_key(k: KeyEvent) -> Option<String> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let wrap = |s: String| -> String {
        let mut inner = s;
        if alt {
            inner = format!("M-{inner}");
        }
        if inner.len() > 1 || alt || ctrl {
            format!("<{inner}>")
        } else {
            inner
        }
    };
    let named = |name: &str| -> Option<String> {
        let mut inner = String::new();
        if ctrl {
            inner.push_str("C-");
        }
        if alt {
            inner.push_str("M-");
        }
        inner.push_str(name);
        Some(format!("<{inner}>"))
    };

    match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                return Some(format!("<C-{c}>"));
            }
            if c == '<' {
                return Some("<lt>".to_string());
            }
            Some(wrap(c.to_string()))
        }
        KeyCode::Enter => named("CR"),
        KeyCode::Esc => named("Esc"),
        KeyCode::Backspace => named("BS"),
        KeyCode::Tab => named("Tab"),
        KeyCode::BackTab => Some("<S-Tab>".to_string()),
        KeyCode::Delete => named("Del"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::Insert => named("Insert"),
        KeyCode::F(n) => Some(format!("<F{n}>")),
        _ => None,
    }
}

fn draw_grid(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    grid: &Grid,
) {
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let w = area.width as usize;
        let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
        for row in grid.cells.iter().take(area.height as usize) {
            lines.push(render_row(grid, row, w));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        let cx = (grid.cursor.0 as u16).min(area.width.saturating_sub(1));
        let cy = (grid.cursor.1 as u16).min(area.height.saturating_sub(1));
        frame.set_cursor_position(Position::new(area.x + cx, area.y + cy));
    });
}

fn render_row(grid: &Grid, row: &[GCell], width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for cell in row.iter().take(width) {
        let style = resolve_style(grid, cell.hl);
        if cur != Some(style) {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), cur.unwrap_or_default()));
            }
            cur = Some(style);
        }
        buf.push(cell.ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur.unwrap_or_default()));
    }
    Line::from(spans)
}

fn resolve_style(grid: &Grid, hl: u16) -> Style {
    let a = grid.hl.get(&hl).copied().unwrap_or_default();
    let mut s = Style::default();
    if let Some(fg) = a.fg.or(grid.def_fg) {
        s = s.fg(rgb(fg));
    }
    if let Some(bg) = a.bg {
        if Some(bg) != grid.def_bg {
            s = s.bg(rgb(bg));
        }
    }
    if a.bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    if a.italic {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if a.underline {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if a.reverse {
        s = s.add_modifier(Modifier::REVERSED);
    }
    s
}

fn rgb(v: u32) -> Color {
    Color::Rgb(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8)
}

#[derive(Clone, Copy)]
struct GCell {
    ch: char,
    hl: u16,
}

impl Default for GCell {
    fn default() -> Self {
        GCell { ch: ' ', hl: 0 }
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Attr {
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Block,
    Bar,
    Underline,
}

struct Grid {
    w: usize,
    h: usize,
    cells: Vec<Vec<GCell>>,
    cursor: (usize, usize),
    hl: HashMap<u16, Attr>,
    def_fg: Option<u32>,
    def_bg: Option<u32>,
    shape: Shape,
}

impl Grid {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            cells: vec![vec![GCell::default(); w]; h],
            cursor: (0, 0),
            hl: HashMap::new(),
            def_fg: None,
            def_bg: None,
            shape: Shape::Block,
        }
    }
    fn resize(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        self.cells = vec![vec![GCell::default(); w]; h];
        self.cursor = (0, 0);
    }
    fn clear(&mut self) {
        for row in &mut self.cells {
            for c in row.iter_mut() {
                *c = GCell::default();
            }
        }
    }
}

fn apply_redraw(grid: &mut Grid, batch: &[Value]) {
    for group in batch {
        let Some(items) = group.as_array() else { continue };
        let Some(name) = items.first().and_then(|v| v.as_str()) else {
            continue;
        };
        for params in &items[1..] {
            let Some(p) = params.as_array() else { continue };
            match name {
                "grid_resize" => {
                    if let (Some(w), Some(h)) = (uget(p, 1), uget(p, 2)) {
                        grid.resize(w as usize, h as usize);
                    }
                }
                "grid_clear" => grid.clear(),
                "grid_cursor_goto" => {
                    if let (Some(r), Some(c)) = (uget(p, 1), uget(p, 2)) {
                        grid.cursor = (c as usize, r as usize);
                    }
                }
                "grid_line" => apply_grid_line(grid, p),
                "grid_scroll" => apply_grid_scroll(grid, p),
                "default_colors_set" => {
                    grid.def_fg = uget(p, 0).map(|v| v as u32);
                    grid.def_bg = uget(p, 1).map(|v| v as u32);
                }
                "hl_attr_define" => {
                    if let Some((id, attr)) = parse_hl(p) {
                        grid.hl.insert(id, attr);
                    }
                }
                "mode_change" => {
                    if let Some(mode) = p.first().and_then(|v| v.as_str()) {
                        grid.shape = if mode.contains("insert") {
                            Shape::Bar
                        } else if mode.contains("replace") {
                            Shape::Underline
                        } else {
                            Shape::Block
                        };
                    }
                }
                _ => {}
            }
        }
    }
}

fn parse_hl(p: &[Value]) -> Option<(u16, Attr)> {
    let id = uget(p, 0)? as u16;
    let m = p.get(1)?;
    let b = |key: &str| map_get(m, key).and_then(|v| v.as_bool()).unwrap_or(false);
    let attr = Attr {
        fg: map_get(m, "foreground").and_then(|v| v.as_u64()).map(|v| v as u32),
        bg: map_get(m, "background").and_then(|v| v.as_u64()).map(|v| v as u32),
        bold: b("bold"),
        italic: b("italic"),
        underline: b("underline") || b("undercurl") || b("underdouble"),
        reverse: b("reverse"),
    };
    Some((id, attr))
}

fn map_get<'a>(m: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Map(entries) = m {
        for (k, v) in entries {
            if k.as_str() == Some(key) {
                return Some(v);
            }
        }
    }
    None
}

fn apply_grid_line(grid: &mut Grid, p: &[Value]) {
    let (Some(row), Some(col_start)) = (uget(p, 1), uget(p, 2)) else {
        return;
    };
    let (row, mut col) = (row as usize, col_start as usize);
    if row >= grid.h {
        return;
    }
    let Some(cells) = p.get(3).and_then(|v| v.as_array()) else {
        return;
    };
    let mut last_hl: u16 = 0;
    for cell in cells {
        let Some(c) = cell.as_array() else { continue };
        let text = c.first().and_then(|v| v.as_str()).unwrap_or(" ");
        if let Some(h) = c.get(1).and_then(|v| v.as_u64()) {
            last_hl = h as u16;
        }
        let remaining = grid.w.saturating_sub(col) as u64;
        let repeat = c.get(2).and_then(|v| v.as_u64()).unwrap_or(1).max(1).min(remaining);
        let ch = text.chars().next().unwrap_or(' ');
        for _ in 0..repeat {
            grid.cells[row][col] = GCell { ch, hl: last_hl };
            col += 1;
        }
    }
}

fn apply_grid_scroll(grid: &mut Grid, p: &[Value]) {
    let (Some(top), Some(bot), Some(left), Some(right)) =
        (uget(p, 1), uget(p, 2), uget(p, 3), uget(p, 4))
    else {
        return;
    };
    let rows = p.get(5).and_then(|v| v.as_i64()).unwrap_or(0);
    let (top, bot, left, right) = (
        (top as usize).min(grid.h),
        (bot as usize).min(grid.h),
        (left as usize).min(grid.w),
        (right as usize).min(grid.w),
    );
    if rows > 0 {
        let r = rows.unsigned_abs() as usize;
        for dst in top..bot.saturating_sub(r) {
            if dst + r >= grid.h {
                break;
            }
            for col in left..right {
                grid.cells[dst][col] = grid.cells[dst + r][col];
            }
        }
        for dst in bot.saturating_sub(r)..bot {
            for col in left..right {
                grid.cells[dst][col] = GCell::default();
            }
        }
    } else if rows < 0 {
        let r = rows.unsigned_abs() as usize;
        for dst in (top + r..bot).rev() {
            if dst < r {
                continue;
            }
            for col in left..right {
                grid.cells[dst][col] = grid.cells[dst - r][col];
            }
        }
        for dst in top..(top + r).min(grid.h) {
            for col in left..right {
                grid.cells[dst][col] = GCell::default();
            }
        }
    }
}

fn uget(p: &[Value], i: usize) -> Option<u64> {
    p.get(i).and_then(|v| v.as_u64())
}

#[derive(Clone)]
struct RedrawHandler {
    tx: mpsc::UnboundedSender<Vec<Value>>,
    run_tx: mpsc::UnboundedSender<RunMsg>,
}

#[async_trait]
impl Handler for RedrawHandler {
    type Writer = NvimWriter;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Neovim<NvimWriter>) {
        match name.as_str() {
            "redraw" => {
                let _ = self.tx.send(args);
            }
            "nsql_run" => {
                let p = args.first();
                let sql = p
                    .and_then(|m| map_get(m, "sql"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let force = p
                    .and_then(|m| map_get(m, "force"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let all = p
                    .and_then(|m| map_get(m, "all"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let _ = self.run_tx.send(RunMsg::Run { sql, force, all });
            }
            "nsql_export" => {
                let fmt = args.first().and_then(|v| v.as_str()).unwrap_or("json").to_string();
                let _ = self.run_tx.send(RunMsg::Export(fmt));
            }
            "nsql_yank" => {
                if let Some(text) = args.first().and_then(|v| v.as_str()) {
                    let _ = self.run_tx.send(RunMsg::Yank(text.to_string()));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_translation() {
        let plain = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(translate_key(plain).as_deref(), Some("a"));
        let cr = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(translate_key(cr).as_deref(), Some("<CR>"));
        let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(translate_key(ctrl_w).as_deref(), Some("<C-w>"));
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(translate_key(esc).as_deref(), Some("<Esc>"));
        let lt = KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE);
        assert_eq!(translate_key(lt).as_deref(), Some("<lt>"));
    }

    #[test]
    fn grid_line_writes_text() {
        let mut g = Grid::new(10, 3);
        let p = vec![
            Value::from(1),
            Value::from(0u64),
            Value::from(0u64),
            Value::Array(vec![
                Value::Array(vec![Value::from("H")]),
                Value::Array(vec![Value::from("i")]),
            ]),
        ];
        apply_grid_line(&mut g, &p);
        assert_eq!(g.cells[0][0].ch, 'H');
        assert_eq!(g.cells[0][1].ch, 'i');
    }

    #[test]
    fn hl_attr_and_default_colors_parse() {
        let mut g = Grid::new(4, 1);
        apply_redraw(
            &mut g,
            &[Value::Array(vec![
                Value::from("default_colors_set"),
                Value::Array(vec![Value::from(0xeeeeeeu64), Value::from(0x111111u64), Value::from(0)]),
            ])],
        );
        assert_eq!(g.def_fg, Some(0xeeeeee));
        assert_eq!(g.def_bg, Some(0x111111));
        let attrmap = Value::Map(vec![
            (Value::from("foreground"), Value::from(0xff0000u64)),
            (Value::from("bold"), Value::from(true)),
        ]);
        apply_redraw(
            &mut g,
            &[Value::Array(vec![
                Value::from("hl_attr_define"),
                Value::Array(vec![Value::from(7u64), attrmap, Value::Map(vec![]), Value::Array(vec![])]),
            ])],
        );
        let a = g.hl.get(&7).copied().unwrap();
        assert_eq!(a.fg, Some(0xff0000));
        assert!(a.bold);
    }

    #[test]
    fn embed_drives_real_nvim_and_reads_back() {
        if crate::util::find_on_path("nvim").is_none() {
            eprintln!("skip: nvim not on PATH");
            return;
        }
        use std::time::Duration;
        let tmp = crate::util::secure_tempfile("nsql-embedtest", "sql").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let grid_text = rt.block_on(async {
            let mut cmd = Command::new("nvim");
            cmd.arg("--embed").arg("--clean").arg(&tmp);
            let (redraw_tx, mut redraw_rx) = mpsc::unbounded_channel::<Vec<Value>>();
            let (nvim, _io, mut child) =
                nvim_rs::create::tokio::new_child_cmd(
                    &mut cmd,
                    RedrawHandler { tx: redraw_tx, run_tx: mpsc::unbounded_channel().0 },
                )
                    .await
                    .expect("spawn nvim --embed");

            let mut opts = UiAttachOptions::new();
            opts.set_linegrid_external(true);
            opts.set_rgb(true);
            nvim.ui_attach(80, 10, &opts).await.expect("ui_attach");

            nvim.input("iSELECT 42;").await.expect("input insert");
            nvim.input("<Esc>").await.expect("input esc");

            let mut grid = Grid::new(80, 10);
            while let Ok(Some(batch)) =
                tokio::time::timeout(Duration::from_millis(400), redraw_rx.recv()).await
            {
                apply_redraw(&mut grid, &batch);
                if grid_has(&grid, "SELECT 42") {
                    break;
                }
            }

            nvim.input(":wq<CR>").await.ok();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;

            grid.cells
                .iter()
                .map(|r| r.iter().map(|c| c.ch).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        });

        let file = std::fs::read_to_string(&tmp).unwrap_or_default();
        std::fs::remove_file(&tmp).ok();

        assert!(
            grid_text.contains("SELECT 42"),
            "typed text never rendered into the grid:\n{grid_text}"
        );
        assert!(
            file.contains("SELECT 42;"),
            "buffer was not written back on :wq: {file:?}"
        );
    }

    #[test]
    fn mode_change_sets_cursor_shape() {
        let mut g = Grid::new(4, 1);
        apply_redraw(
            &mut g,
            &[Value::Array(vec![
                Value::from("mode_change"),
                Value::Array(vec![Value::from("insert"), Value::from(1)]),
            ])],
        );
        assert!(matches!(g.shape, Shape::Bar));
        apply_redraw(
            &mut g,
            &[Value::Array(vec![
                Value::from("mode_change"),
                Value::Array(vec![Value::from("normal"), Value::from(0)]),
            ])],
        );
        assert!(matches!(g.shape, Shape::Block));
    }

    #[test]
    fn embed_captures_highlight_colors() {
        if crate::util::find_on_path("nvim").is_none() {
            eprintln!("skip: nvim not on PATH");
            return;
        }
        use std::time::Duration;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let captured = rt.block_on(async {
            let mut cmd = Command::new("nvim");
            cmd.arg("--embed").arg("--clean");
            let (redraw_tx, mut redraw_rx) = mpsc::unbounded_channel::<Vec<Value>>();
            let (nvim, _io, mut child) =
                nvim_rs::create::tokio::new_child_cmd(
                    &mut cmd,
                    RedrawHandler { tx: redraw_tx, run_tx: mpsc::unbounded_channel().0 },
                )
                    .await
                    .expect("spawn");
            let mut opts = UiAttachOptions::new();
            opts.set_linegrid_external(true);
            opts.set_rgb(true);
            nvim.ui_attach(80, 6, &opts).await.expect("attach");
            nvim.command("set termguicolors").await.ok();
            nvim.command("highlight Normal guifg=#abcdef guibg=#123456")
                .await
                .ok();

            let mut grid = Grid::new(80, 6);
            let mut found = false;
            while let Ok(Some(batch)) =
                tokio::time::timeout(Duration::from_millis(500), redraw_rx.recv()).await
            {
                apply_redraw(&mut grid, &batch);
                if grid.def_fg.is_some() || grid.hl.values().any(|a| a.fg.is_some()) {
                    found = true;
                    break;
                }
            }
            nvim.input(":qa!<CR>").await.ok();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            (found, grid.def_fg)
        });

        assert!(
            captured.0,
            "no rgb foreground decoded from redraw stream (def_fg={:?})",
            captured.1
        );
    }

    #[test]
    fn postgres_in_session_does_not_nested_runtime_panic() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let prof = crate::config::Profile {
            name: "t".into(),
            url: "postgres://u@127.0.0.1:1/nope".into(),
            prod: false,
            readonly: false,
            no_history: false,
        };
        let r = rt.block_on(async move {
            tokio::task::spawn_blocking(move || db::run(&prof, "select 1", false))
                .await
                .unwrap()
        });
        assert!(r.is_err(), "expected a connection error, not a panic/Ok");
    }

    #[test]
    fn render_outcome_exports_and_persist() {
        let prof = crate::config::Profile {
            name: "t".into(),
            url: "sqlite::memory:".into(),
            prod: false,
            readonly: false,
            no_history: false,
        };
        let sql = "select 7 as answer, null as n";
        let res = db::run(&prof, sql, false);
        let o = render_outcome(&res, sql);

        let json = o.json.expect("json");
        assert!(json.contains("answer") && json.contains('7'));
        let csv = o.csv.expect("csv");
        assert!(csv.contains("answer"));
        let persist = o.persist.expect("persist");
        assert!(persist.contains("-- select 7 as answer"), "persist must echo the query");
        assert!(persist.contains('7') && persist.contains("answer"));
        assert!(persist.contains("-- 1 row"), "persist needs a concise summary line");
    }

    #[test]
    fn render_outcome_error_does_not_persist() {
        let err: Result<db::QueryResult> = Err(anyhow!("kaboom"));
        let o = render_outcome(&err, "select 1");
        assert!(o.persist.is_none(), "an error must not overwrite the persisted result");
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
        assert!(lines.iter().any(|l| l.contains('∅')), "NULL needs a distinct glyph");

        assert_eq!(classify("42", Some(&db::Cell::Int(42))), "Number");
        assert_eq!(classify("widget", Some(&db::Cell::Text("widget".into()))), "String");
        assert_eq!(classify("2026-06-10", Some(&db::Cell::Text("2026-06-10".into()))), "Constant");
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

    #[test]
    fn introspect_schema_lists_tables_and_columns() {
        let path = std::env::temp_dir().join(format!("nsql-schema-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let prof = crate::config::Profile {
            name: "t".into(),
            url: format!("sqlite://{}", path.display()),
            prod: false,
            readonly: false,
            no_history: false,
        };
        db::run(&prof, "create table cat(name text, age int)", true).unwrap();
        db::run(&prof, "create table dog(id int, label text)", true).unwrap();

        let schema = introspect_schema(&prof).expect("schema");
        let get = |v: &Value, k: &str| -> Option<Value> {
            if let Value::Map(m) = v {
                m.iter().find(|(kk, _)| kk.as_str() == Some(k)).map(|(_, vv)| vv.clone())
            } else {
                None
            }
        };
        let tables = get(&schema, "tables").unwrap();
        let tnames: Vec<&str> = tables.as_array().unwrap().iter().filter_map(|t| t.as_str()).collect();
        assert!(tnames.contains(&"cat") && tnames.contains(&"dog"), "tables: {tnames:?}");

        let by_table = get(&schema, "by_table").unwrap();
        let cat_cols = get(&by_table, "cat").unwrap();
        let cnames: Vec<&str> = cat_cols.as_array().unwrap().iter().filter_map(|c| c.as_str()).collect();
        assert!(cnames.contains(&"name") && cnames.contains(&"age"), "cat cols: {cnames:?}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_background_is_transparent() {
        let mut g = Grid::new(2, 1);
        g.def_bg = Some(0x112233);
        assert!(resolve_style(&g, 0).bg.is_none(), "default bg must be transparent");
        g.hl.insert(
            1,
            Attr {
                bg: Some(0x112233),
                ..Default::default()
            },
        );
        assert!(
            resolve_style(&g, 1).bg.is_none(),
            "a bg equal to the default must stay transparent"
        );
        g.hl.insert(
            2,
            Attr {
                bg: Some(0xff0000),
                ..Default::default()
            },
        );
        assert!(
            resolve_style(&g, 2).bg.is_some(),
            "a distinct highlight bg must be painted"
        );
    }

    fn grid_has(g: &Grid, needle: &str) -> bool {
        g.cells
            .iter()
            .any(|r| r.iter().map(|c| c.ch).collect::<String>().contains(needle))
    }

    #[test]
    fn run_keymap_round_trips_via_rpcnotify() {
        if crate::util::find_on_path("nvim").is_none() {
            eprintln!("skip: nvim not on PATH");
            return;
        }
        use std::time::Duration;
        let inject = crate::util::secure_tempfile("nsql-inj", "lua").unwrap();
        std::fs::write(&inject, include_str!("../assets/inject.lua")).unwrap();
        let sqlf = crate::util::secure_tempfile("nsql-rt", "sql").unwrap();
        std::fs::write(&sqlf, "select 7 as v;\n").unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (got_run, got_copy) = rt.block_on(async {
            let mut cmd = Command::new("nvim");
            cmd.env("NSQL_STATUS", "test")
                .arg("--embed")
                .arg("--clean")
                .arg(&sqlf)
                .arg("-c")
                .arg("setfiletype sql")
                .arg("-c")
                .arg(format!("luafile {}", inject.display()));
            let (redraw_tx, mut redraw_rx) = mpsc::unbounded_channel::<Vec<Value>>();
            let (run_tx, mut run_rx) = mpsc::unbounded_channel::<RunMsg>();
            let (nvim, _io, mut child) =
                nvim_rs::create::tokio::new_child_cmd(&mut cmd, RedrawHandler { tx: redraw_tx, run_tx })
                    .await
                    .expect("spawn");
            let mut o = UiAttachOptions::new();
            o.set_linegrid_external(true);
            o.set_rgb(true);
            nvim.ui_attach(80, 6, &o).await.expect("attach");
            if let Ok(info) = nvim.get_api_info().await {
                if let Some(ch) = info.first().and_then(|v| v.as_i64()) {
                    let _ = nvim.set_var("nsql_chan", Value::from(ch)).await;
                }
            }

            async fn next(
                run_rx: &mut mpsc::UnboundedReceiver<RunMsg>,
                redraw_rx: &mut mpsc::UnboundedReceiver<Vec<Value>>,
            ) -> Option<RunMsg> {
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        tokio::select! {
                            Some(m) = run_rx.recv() => return Some(m),
                            Some(_) = redraw_rx.recv() => continue,
                            else => return None,
                        }
                    }
                })
                .await
                .ok()
                .flatten()
            }

            nvim.input(":w<CR>").await.ok();
            let run = next(&mut run_rx, &mut redraw_rx).await;
            nvim.input(",j").await.ok();
            let export = next(&mut run_rx, &mut redraw_rx).await;

            nvim.input(":qa!<CR>").await.ok();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            (run, export)
        });
        std::fs::remove_file(&inject).ok();
        std::fs::remove_file(&sqlf).ok();

        match got_run {
            Some(RunMsg::Run { sql, .. }) => {
                assert!(sql.contains("select 7"), "unexpected sql: {sql:?}")
            }
            _ => panic!("`:w` did not deliver a RunMsg::Run"),
        }
        match got_copy {
            Some(RunMsg::Export(fmt)) => assert_eq!(fmt, "json"),
            _ => panic!("`,j` did not deliver a RunMsg::Export"),
        }
    }
}

//! Zero-flash persistent inline session (behind the `embed-editor` feature).
//!
//! We spawn `nvim --embed` — a headless editing engine that draws NOTHING to the
//! terminal — drive it over msgpack-RPC, attach a UI, and render its
//! `ext_linegrid` redraw stream (with colors) into a ratatui **inline** region.
//! That region is split: the editor on top, a results pane on the bottom. The
//! user's scrollback ABOVE is never touched — running a query updates the bottom
//! pane in place (no `insert_before`, no scroll), so you keep an eye on your main
//! work. nvim never emits smcup, so there is no alt-screen flash, ever.
//!
//! Queries run IN-SESSION: a buffer-local keymap (`,r`) or `:w` fires an
//! `rpcnotify('nsql_run', {sql})` (the channel is handed to inject.lua as
//! `g:nsql_chan` after attach); the handler runs it via `db::run` and renders the
//! result into the pane. `,y` copies the last result as TSV via OSC 52. `,,`/`,q`
//! quit (persisting the scratch). Errors render in the pane and never end the
//! session.
//!
//! The async machinery (tokio + nvim-rs + ratatui) is contained entirely within
//! this module and a current-thread runtime scoped inside `compose()`; the rest
//! of nsql stays sync.

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

/// Open the persistent inline session (zero-flash). Queries run IN-SESSION (on
/// `,r`/`:w`); results render in a bottom pane WITHOUT disturbing the scrollback
/// above. Returns when the user quits (`,,`/`,q`); the scratch is persisted.
pub fn compose(paths: &Paths, profile: &Profile) -> Result<()> {
    editor::write_inject(paths)?;
    let scratch = paths.scratch_for(&profile.name);
    let prior = std::fs::read_to_string(&scratch).unwrap_or_default();
    // Clean buffer: just the prior scratch (strip_header scrubs any legacy header).
    let initial = editor::strip_header(&prior);

    let tmp = util::secure_tempfile("nsql", "sql")?;
    std::fs::write(&tmp, &initial).with_context(|| format!("writing {}", tmp.display()))?;

    // Scoped current-thread runtime — no async escapes this function.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting embed runtime")?;

    let res = rt.block_on(run_session(paths, &tmp, profile));
    // Always restore the terminal, whatever happened.
    let _ = disable_raw_mode();
    res?;

    // Persist the buffer for next-time resume (queries already ran in-session).
    let edited = std::fs::read_to_string(&tmp).unwrap_or_default();
    std::fs::remove_file(&tmp).ok();
    let body = editor::strip_header(&edited);
    if let Err(e) = editor::persist_scratch(&scratch, &body) {
        eprintln!("nsql: warning: could not save scratch: {e:#}");
    }
    Ok(())
}

/// A request from inside nvim (via rpcnotify) to act on the session.
enum RunMsg {
    Run { sql: String, force: bool, all: bool },
    Copy,
}

/// Drive the embedded nvim session: editor on top, results pane on the bottom.
async fn run_session(paths: &Paths, tmp: &std::path::Path, profile: &Profile) -> Result<()> {
    let (cols, rows) = util::term_size();
    let width = cols.max(20);
    // Split the inline region: editor on top, a results pane on the bottom,
    // with a 1-row separator. The scrollback ABOVE is never touched.
    let total = rows.saturating_sub(2).clamp(8, 30);
    let results_rows = (total / 2).clamp(4, 12);
    let editor_rows = total.saturating_sub(results_rows + 1).max(3);
    let view_h = editor_rows + 1 + results_rows;

    let mut cmd = Command::new("nvim");
    cmd.env("NSQL_STATUS", editor::status_line(profile))
        .arg("--embed")
        .arg("-n")
        .arg("-i")
        .arg("NONE")
        .arg(tmp)
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
    nvim.ui_attach(width as i64, editor_rows as i64, &opts)
        .await
        .map_err(|e| anyhow!("nvim ui_attach failed: {e}"))?;

    // Tell inject.lua which channel to rpcnotify on. nvim_get_api_info() called
    // from a startup luafile returns the internal channel, so we resolve it here
    // (this RPC call resolves to our channel) and hand it over as g:nsql_chan.
    if let Ok(info) = nvim.get_api_info().await {
        if let Some(ch) = info.first().and_then(|v| v.as_i64()) {
            let _ = nvim.set_var("nsql_chan", Value::from(ch)).await;
        }
    }

    enable_raw_mode().context("enabling raw mode")?;
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
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

    let mut grid = Grid::new(width as usize, editor_rows as usize);
    let mut last_shape = Shape::Block;
    let mut results: Vec<String> =
        vec!["  (no results yet · ,r run · ,y copy · ,, quit)".to_string()];
    let mut result_tsv: Option<String> = None;

    loop {
        let mut dirty = false;
        while let Ok(batch) = redraw_rx.try_recv() {
            apply_redraw(&mut grid, &batch);
            dirty = true;
        }
        if dirty {
            draw_session(&mut terminal, &grid, editor_rows, results_rows, &results);
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
                        run_query(
                            paths, profile, &sql, force, all,
                            &mut results, &mut result_tsv,
                            &mut terminal, &grid, editor_rows, results_rows,
                        );
                    }
                    RunMsg::Copy => {
                        if let Some(tsv) = &result_tsv {
                            osc52_copy(tsv);
                            results = vec!["  ✓ copied last result to clipboard".to_string()];
                            draw_session(&mut terminal, &grid, editor_rows, results_rows, &results);
                        }
                    }
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
                        grid.resize(w as usize, editor_rows as usize);
                        let _ = nvim.ui_try_resize(w as i64, editor_rows as i64).await;
                        let _ = terminal.autoresize();
                        let _ = terminal.clear();
                    }
                    _ => {}
                }
            }
            Some(batch) = redraw_rx.recv() => {
                apply_redraw(&mut grid, &batch);
                draw_session(&mut terminal, &grid, editor_rows, results_rows, &results);
            }
        }
    }

    shutdown.store(true, Ordering::Relaxed);
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        SetCursorStyle::DefaultUserShape
    );
    let _ = terminal.clear(); // wipe nsql's region; main scrollback is untouched
    Ok(())
}

/// Execute one query and update the results pane. Never propagates an error out
/// of the session: guard/DB failures render as an in-pane message.
#[allow(clippy::too_many_arguments)]
fn run_query(
    paths: &Paths,
    profile: &Profile,
    sql: &str,
    force: bool,
    all: bool,
    results: &mut Vec<String>,
    result_tsv: &mut Option<String>,
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    grid: &Grid,
    editor_rows: u16,
    results_rows: u16,
) {
    if db::strip_sql_comments(sql).trim().is_empty() {
        *results = vec!["  (nothing to run)".to_string()];
        draw_session(terminal, grid, editor_rows, results_rows, results);
        return;
    }
    // Immediate feedback before the (blocking) query.
    *results = vec![format!("  running: {}…", first_line(sql))];
    draw_session(terminal, grid, editor_rows, results_rows, results);

    let started = std::time::Instant::now();
    let outcome = db::guard(profile, sql, force, false).and_then(|_| db::run(profile, sql, all));
    match outcome {
        Ok(result) => {
            let table_opts = render::Options {
                format: render::Format::Table,
                is_tty: true,
                echo: None,
                elapsed: Some(started.elapsed()),
            };
            *results = render::format(&result, &table_opts)
                .lines()
                .map(|l| l.to_string())
                .collect();
            let tsv_opts = render::Options {
                format: render::Format::Tsv,
                is_tty: false,
                echo: None,
                elapsed: None,
            };
            *result_tsv = Some(render::format(&result, &tsv_opts));
            if !profile.no_history {
                let _ = history::record(paths, &profile.name, sql);
            }
        }
        Err(e) => {
            // Single-line, already-redacted error (db.rs masks the DSN).
            *results = vec![format!("  error: {}", first_line(&format!("{e:#}")))];
        }
    }
    draw_session(terminal, grid, editor_rows, results_rows, results);
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

/// Copy text to the system clipboard via OSC 52 (works over SSH; no xclip/pbcopy).
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

/// Translate a crossterm key into nvim input notation. M1 covers the common keys
/// needed to type SQL and trigger `,,` / `:wq` / `,q`.
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

/// Render the split inline region: nvim editor (top `editor_rows`), a separator,
/// then the results pane (bottom `results_rows`). The scrollback above is never
/// touched.
fn draw_session(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    grid: &Grid,
    editor_rows: u16,
    results_rows: u16,
    results: &[String],
) {
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let w = area.width as usize;
        let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);

        // Editor: the nvim grid (sized to editor_rows via ui_attach).
        for row in grid.cells.iter().take(editor_rows as usize) {
            lines.push(render_row(grid, row, w));
        }
        // Separator.
        lines.push(Line::from(Span::styled(
            "─".repeat(w),
            Style::default().fg(Color::DarkGray),
        )));
        // Results pane (plain text, truncated to width; non-navigable preview).
        let shown = results_rows.saturating_sub(0) as usize;
        for (i, r) in results.iter().enumerate() {
            if i >= shown {
                let more = results.len() - shown + 1;
                if let Some(last) = lines.last_mut() {
                    *last = Line::from(Span::styled(
                        format!("  … +{more} more lines · ,a all · ,y copy"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                break;
            }
            let truncated: String = r.chars().take(w).collect();
            lines.push(Line::from(truncated));
        }

        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        // Cursor lives in the editor pane only.
        let cx = (grid.cursor.0 as u16).min(area.width.saturating_sub(1));
        let cy = (grid.cursor.1 as u16).min(editor_rows.saturating_sub(1));
        frame.set_cursor_position(Position::new(area.x + cx, area.y + cy));
    });
}

/// Build one styled line, coalescing runs of cells that share a highlight.
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
    if let Some(bg) = a.bg.or(grid.def_bg) {
        s = s.bg(rgb(bg));
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

// ---- grid model ----------------------------------------------------------

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

/// A resolved highlight (subset of nvim's `hl_attr_define` rgb attributes).
#[derive(Clone, Copy, Default, PartialEq)]
struct Attr {
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
}

/// Terminal cursor shape, mirrored from nvim's current mode.
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
    cursor: (usize, usize), // (col, row)
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
                    // [grid, width, height]
                    if let (Some(w), Some(h)) = (uget(p, 1), uget(p, 2)) {
                        grid.resize(w as usize, h as usize);
                    }
                }
                "grid_clear" => grid.clear(),
                "grid_cursor_goto" => {
                    // [grid, row, col]
                    if let (Some(r), Some(c)) = (uget(p, 1), uget(p, 2)) {
                        grid.cursor = (c as usize, r as usize);
                    }
                }
                "grid_line" => apply_grid_line(grid, p),
                "grid_scroll" => apply_grid_scroll(grid, p),
                "default_colors_set" => {
                    // [rgb_fg, rgb_bg, rgb_sp, cterm_fg, cterm_bg]
                    grid.def_fg = uget(p, 0).map(|v| v as u32);
                    grid.def_bg = uget(p, 1).map(|v| v as u32);
                }
                "hl_attr_define" => {
                    if let Some((id, attr)) = parse_hl(p) {
                        grid.hl.insert(id, attr);
                    }
                }
                "mode_change" => {
                    // [mode (str), mode_idx]
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
    // [id, rgb_attr (map), cterm_attr (map), info]
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
    // [grid, row, col_start, cells, wrap]
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
    // hl id carries over to following cells that omit it (nvim ui spec).
    let mut last_hl: u16 = 0;
    for cell in cells {
        let Some(c) = cell.as_array() else { continue };
        let text = c.first().and_then(|v| v.as_str()).unwrap_or(" ");
        if let Some(h) = c.get(1).and_then(|v| v.as_u64()) {
            last_hl = h as u16;
        }
        let repeat = c.get(2).and_then(|v| v.as_u64()).unwrap_or(1).max(1);
        let ch = text.chars().next().unwrap_or(' ');
        for _ in 0..repeat {
            if col < grid.w {
                grid.cells[row][col] = GCell { ch, hl: last_hl };
                col += 1;
            }
        }
    }
}

fn apply_grid_scroll(grid: &mut Grid, p: &[Value]) {
    // [grid, top, bot, left, right, rows, cols]
    let (Some(top), Some(bot), Some(left), Some(right)) =
        (uget(p, 1), uget(p, 2), uget(p, 3), uget(p, 4))
    else {
        return;
    };
    let rows = p.get(5).and_then(|v| v.as_i64()).unwrap_or(0);
    let (top, bot, left, right) = (top as usize, bot as usize, left as usize, right as usize);
    if rows > 0 {
        let r = rows as usize;
        for dst in top..bot.saturating_sub(r) {
            for col in left..right.min(grid.w) {
                grid.cells[dst][col] = grid.cells[dst + r][col];
            }
        }
        for dst in bot.saturating_sub(r)..bot.min(grid.h) {
            for col in left..right.min(grid.w) {
                grid.cells[dst][col] = GCell::default();
            }
        }
    } else if rows < 0 {
        let r = (-rows) as usize;
        for dst in (top + r..bot).rev() {
            for col in left..right.min(grid.w) {
                grid.cells[dst][col] = grid.cells[dst - r][col];
            }
        }
        for dst in top..(top + r).min(grid.h) {
            for col in left..right.min(grid.w) {
                grid.cells[dst][col] = GCell::default();
            }
        }
    }
}

fn uget(p: &[Value], i: usize) -> Option<u64> {
    p.get(i).and_then(|v| v.as_u64())
}

// ---- RPC handler ---------------------------------------------------------

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
            // rpcnotify(ch, 'nsql_run', { sql=…, force=bool, all=bool }) from inject.lua.
            // The SQL is treated as untrusted text: the session re-derives guard/run.
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
            "nsql_copy" => {
                let _ = self.run_tx.send(RunMsg::Copy);
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
        // grid_line params: [grid, row, col_start, [[ "H" ],[ "i" ]]]
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
        // default_colors_set [fg, bg, sp, ...]
        apply_redraw(
            &mut g,
            &[Value::Array(vec![
                Value::from("default_colors_set"),
                Value::Array(vec![Value::from(0xeeeeeeu64), Value::from(0x111111u64), Value::from(0)]),
            ])],
        );
        assert_eq!(g.def_fg, Some(0xeeeeee));
        assert_eq!(g.def_bg, Some(0x111111));
        // hl_attr_define id=7 with a foreground + bold
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

    /// End-to-end against REAL nvim (no tty needed): spawn `nvim --embed`, attach
    /// a UI, type text, confirm it renders into our grid via the redraw stream,
    /// then `:wq` and confirm the buffer is written back. This is the M1 contract.
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

    /// Prove the M2 color pipeline against real nvim: force a concrete Normal
    /// highlight and confirm we decode its rgb foreground from the redraw stream.
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

    fn grid_has(g: &Grid, needle: &str) -> bool {
        g.cells
            .iter()
            .any(|r| r.iter().map(|c| c.ch).collect::<String>().contains(needle))
    }

    /// Deterministically (nvim --clean, no user config to race) verify the
    /// run-without-quit plumbing: inject.lua's `,r` and `,y` keymaps fire
    /// rpcnotify that the handler turns into RunMsg::{Run, Copy}.
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
            // Hand the channel to inject.lua, exactly as run_session does.
            if let Ok(info) = nvim.get_api_info().await {
                if let Some(ch) = info.first().and_then(|v| v.as_i64()) {
                    let _ = nvim.set_var("nsql_chan", Value::from(ch)).await;
                }
            }

            // Drain redraws while waiting for the next RunMsg (keeps the io task fed).
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

            nvim.input(",r").await.ok();
            let run = next(&mut run_rx, &mut redraw_rx).await;
            nvim.input(",y").await.ok();
            let copy = next(&mut run_rx, &mut redraw_rx).await;

            nvim.input(",,").await.ok();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            (run, copy)
        });
        std::fs::remove_file(&inject).ok();
        std::fs::remove_file(&sqlf).ok();

        match got_run {
            Some(RunMsg::Run { sql, .. }) => {
                assert!(sql.contains("select 7"), "unexpected sql: {sql:?}")
            }
            _ => panic!("`,r` did not deliver a RunMsg::Run"),
        }
        assert!(matches!(got_copy, Some(RunMsg::Copy)), "`,y` did not deliver RunMsg::Copy");
    }
}

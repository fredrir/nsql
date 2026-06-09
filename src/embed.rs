//! Zero-flash inline editor (Phase 3, M1 — behind the `embed-editor` feature).
//!
//! Instead of spawning nvim as a full-screen child (Mode 1, which flashes the
//! alternate screen during the edit), we spawn `nvim --embed` — a headless
//! editing engine that draws NOTHING to the terminal — drive it over msgpack-RPC,
//! attach a UI, and render its `ext_linegrid` redraw stream into a ratatui
//! **inline** viewport. nvim never emits smcup, so there is no alt-screen flash
//! at all; results still print to the normal screen above the viewport.
//!
//! The async machinery (tokio + nvim-rs + ratatui) is contained entirely within
//! this module and a current-thread runtime scoped inside `compose()`. The rest
//! of nsql stays sync. Run/cancel reuses the identical exit-code + temp-file
//! contract as Mode 1 (`,,` -> :wq -> exit 0 -> run; `,q` -> :cquit -> cancel).
//!
//! M1 is a deliberately monochrome renderer (text + cursor; highlights/colors are
//! M2). It proves the loop, the input path, and the exit/readback contract.

use crate::config::{Paths, Profile};
use crate::{db, editor, util};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use nvim_rs::compat::tokio::Compat;
use nvim_rs::{Handler, Neovim, UiAttachOptions, Value};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::layout::Position;
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;

type NvimWriter = Compat<ChildStdin>;

/// Compose a query in the embedded (zero-flash) editor. Same contract as
/// `editor::compose`: `Some(sql)` to run, `None` to cancel / no-op.
pub fn compose(paths: &Paths, profile: &Profile) -> Result<Option<String>> {
    editor::write_inject(paths)?;
    let scratch = paths.scratch_for(&profile.name);
    let prior = std::fs::read_to_string(&scratch).unwrap_or_default();
    let initial = format!("{}{}", editor::header(profile), editor::strip_header(&prior));

    let tmp = util::secure_tempfile("nsql", "sql")?;
    std::fs::write(&tmp, &initial).with_context(|| format!("writing {}", tmp.display()))?;

    // Scoped current-thread runtime — no async escapes this function.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting embed runtime")?;

    let exit_code = rt.block_on(run_session(paths, &tmp));
    // Always restore the terminal, whatever happened.
    let _ = disable_raw_mode();
    let exit_code = exit_code?;

    if exit_code != 0 {
        std::fs::remove_file(&tmp).ok();
        return Ok(None);
    }
    let edited = std::fs::read_to_string(&tmp).unwrap_or_default();
    std::fs::remove_file(&tmp).ok();
    let body = editor::strip_header(&edited);
    if let Err(e) = editor::persist_scratch(&scratch, &body) {
        eprintln!("nsql: warning: could not save scratch: {e:#}");
    }
    if db::strip_sql_comments(&body).trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(body))
}

/// Drive the embedded nvim and return its process exit code.
async fn run_session(paths: &Paths, tmp: &std::path::Path) -> Result<i32> {
    let (cols, rows) = util::term_size();
    let height = rows.saturating_sub(2).clamp(3, 24);
    let width = cols.max(20);

    // Spawn `nvim --embed` on the temp file, with our buffer-local keymaps.
    let mut cmd = Command::new("nvim");
    cmd.arg("--embed")
        .arg("-n")
        .arg("-i")
        .arg("NONE")
        .arg(tmp)
        .arg("-c")
        .arg("setfiletype sql")
        .arg("-c")
        .arg(format!("luafile {}", paths.inject_lua.display()));

    let (redraw_tx, mut redraw_rx) = mpsc::unbounded_channel::<Vec<Value>>();
    let handler = RedrawHandler { tx: redraw_tx };

    let (nvim, _io, mut child) = nvim_rs::create::tokio::new_child_cmd(&mut cmd, handler)
        .await
        .context("spawning nvim --embed")?;

    // Attach a UI so nvim sources the user's config and starts emitting redraws.
    let mut opts = UiAttachOptions::new();
    opts.set_linegrid_external(true);
    opts.set_rgb(true);
    nvim.ui_attach(width as i64, height as i64, &opts)
        .await
        .map_err(|e| anyhow!("nvim ui_attach failed: {e}"))?;

    // Raw-mode key reader on a dedicated thread (poll so it can shut down).
    enable_raw_mode().context("enabling raw mode")?;
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
            viewport: Viewport::Inline(height),
        },
    )
    .context("creating inline terminal")?;

    let mut grid = Grid::new(width as usize, height as usize);

    let code: i32 = loop {
        // Drain all pending redraws (non-blocking) so the grid is current, then
        // render once. This keeps a chatty config (statusline/cursor/plugins)
        // from monopolising the loop.
        let mut dirty = false;
        while let Ok(batch) = redraw_rx.try_recv() {
            apply_redraw(&mut grid, &batch);
            dirty = true;
        }
        if dirty {
            draw(&mut terminal, &grid);
        }

        // Keys are listed BEFORE redraws in this biased select so a continuous
        // redraw stream can never starve user input.
        tokio::select! {
            biased;
            status = child.wait() => {
                break status.ok().and_then(|s| s.code()).unwrap_or(1);
            }
            Some(ev) = key_rx.recv() => {
                match ev {
                    Event::Key(k) => {
                        if let Some(input) = translate_key(k) {
                            let _ = nvim.input(&input).await;
                        }
                    }
                    Event::Resize(w, h) => {
                        let h = (h.saturating_sub(2)).clamp(3, 24);
                        grid.resize(w as usize, h as usize);
                        let _ = nvim.ui_try_resize(w as i64, h as i64).await;
                        let _ = terminal.clear();
                    }
                    _ => {}
                }
            }
            Some(batch) = redraw_rx.recv() => {
                apply_redraw(&mut grid, &batch);
                draw(&mut terminal, &grid);
            }
        }
    };

    shutdown.store(true, Ordering::Relaxed);
    let _ = terminal.clear(); // wipe the inline viewport; results print below
    Ok(code)
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

fn draw(terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>, grid: &Grid) {
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let lines: Vec<Line> = grid
            .cells
            .iter()
            .take(area.height as usize)
            .map(|row| Line::from(row.iter().collect::<String>()))
            .collect();
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        let cx = (grid.cursor.0 as u16).min(area.width.saturating_sub(1));
        let cy = (grid.cursor.1 as u16).min(area.height.saturating_sub(1));
        frame.set_cursor_position(Position::new(area.x + cx, area.y + cy));
    });
}

// ---- grid model ----------------------------------------------------------

struct Grid {
    w: usize,
    h: usize,
    cells: Vec<Vec<char>>,
    cursor: (usize, usize), // (col, row)
}

impl Grid {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            cells: vec![vec![' '; w]; h],
            cursor: (0, 0),
        }
    }
    fn resize(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        self.cells = vec![vec![' '; w]; h];
        self.cursor = (0, 0);
    }
    fn clear(&mut self) {
        for row in &mut self.cells {
            for c in row.iter_mut() {
                *c = ' ';
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
                _ => {}
            }
        }
    }
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
    for cell in cells {
        let Some(c) = cell.as_array() else { continue };
        let text = c.first().and_then(|v| v.as_str()).unwrap_or(" ");
        let repeat = c.get(2).and_then(|v| v.as_u64()).unwrap_or(1).max(1);
        let ch = text.chars().next().unwrap_or(' ');
        for _ in 0..repeat {
            if col < grid.w {
                grid.cells[row][col] = ch;
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
                grid.cells[dst][col] = ' ';
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
                grid.cells[dst][col] = ' ';
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
}

#[async_trait]
impl Handler for RedrawHandler {
    type Writer = NvimWriter;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Neovim<NvimWriter>) {
        if name == "redraw" {
            let _ = self.tx.send(args);
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
        assert_eq!(g.cells[0][0], 'H');
        assert_eq!(g.cells[0][1], 'i');
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
                nvim_rs::create::tokio::new_child_cmd(&mut cmd, RedrawHandler { tx: redraw_tx })
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
                .map(|r| r.iter().collect::<String>())
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

    fn grid_has(g: &Grid, needle: &str) -> bool {
        g.cells
            .iter()
            .any(|r| r.iter().collect::<String>().contains(needle))
    }
}

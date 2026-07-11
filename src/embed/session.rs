use super::grid::{apply_redraw, draw_grid, map_get, Grid, Shape};
use super::keys::{translate_key, VISUAL_RUN_LUA};
use super::results::{
    first_line, format_for_buffer, render_outcome, write_results, CellMark, SETUP_RESULTS_LUA,
};
use super::ring;
use super::schema::{introspect_schema, OMNI_LUA, SET_SCHEMA_LUA};
use super::NvimWriter;
use crate::config::{Paths, Profile};
use crate::{db, editor, history, util};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use nvim_rs::{Handler, Neovim, UiAttachOptions, Value};
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::mpsc;

pub(super) enum RunMsg {
    Run { sql: String, force: bool, all: bool },
    Export(String),
    Yank(String),
}

pub(super) async fn run_session(
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

    let _ = nvim.exec_lua(OMNI_LUA, vec![]).await;
    let _ = nvim.exec_lua(VISUAL_RUN_LUA, vec![]).await;
    let _ = nvim.exec_lua(ring::SETUP_HISTORY_LUA, vec![]).await;
    let _ = nvim
        .exec_lua(
            ring::SET_HISTORY_LUA,
            vec![ring::entries_value(paths, &profile.name)],
        )
        .await;

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
                    let _ = nvim
                        .exec_lua(
                            ring::SET_HISTORY_LUA,
                            vec![ring::entries_value(paths, &profile.name)],
                        )
                        .await;
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

#[derive(Clone)]
pub(super) struct RedrawHandler {
    pub(super) tx: mpsc::UnboundedSender<Vec<Value>>,
    pub(super) run_tx: mpsc::UnboundedSender<RunMsg>,
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
                let fmt = args
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or("json")
                    .to_string();
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
            let (nvim, _io, mut child) = nvim_rs::create::tokio::new_child_cmd(
                &mut cmd,
                RedrawHandler {
                    tx: redraw_tx,
                    run_tx: mpsc::unbounded_channel().0,
                },
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
            let (nvim, _io, mut child) = nvim_rs::create::tokio::new_child_cmd(
                &mut cmd,
                RedrawHandler {
                    tx: redraw_tx,
                    run_tx: mpsc::unbounded_channel().0,
                },
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
            ssh: None,
        };
        let r = rt.block_on(async move {
            tokio::task::spawn_blocking(move || db::run(&prof, "select 1", false))
                .await
                .unwrap()
        });
        assert!(r.is_err(), "expected a connection error, not a panic/Ok");
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
        std::fs::write(&inject, include_str!("../../assets/inject.lua")).unwrap();
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
            let (nvim, _io, mut child) = nvim_rs::create::tokio::new_child_cmd(
                &mut cmd,
                RedrawHandler {
                    tx: redraw_tx,
                    run_tx,
                },
            )
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

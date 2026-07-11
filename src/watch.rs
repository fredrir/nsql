//! `--watch N`: psql's beloved `\watch`, adapted to nsql's no-altscreen rule.
//! On a tty the previous frame is erased with cursor-up + clear-below (plain
//! ANSI in the main screen — scrollback above the frame survives); if a frame
//! is taller than the terminal we stop erasing and append instead. Ctrl-C
//! ends the loop cleanly.

use crate::cli::Cli;
use crate::config::Profile;
use crate::db::{self, RunOpts};
use crate::{cancel, render, util};
use anyhow::Result;
use std::io::Write;
use std::time::{Duration, Instant};

pub fn run(profile: &Profile, sql_text: &str, secs: f64, cli: &Cli, is_tty: bool) -> Result<()> {
    let interval = Duration::from_secs_f64(secs.max(0.2));
    let mut conn = db::connect(profile)?;
    let opts = RunOpts::new(cli.all);

    let mut prev_lines: Option<usize> = None;
    let mut runs = 0u64;

    cancel::reset();
    loop {
        if cancel::interrupted() {
            break;
        }
        let started = Instant::now();
        let guard = conn.cancel_closure().map(cancel::arm);
        let out = db::run_on(&mut conn, sql_text, &opts);
        drop(guard);
        if cancel::interrupted() {
            break;
        }
        let out = out?; // a real error (not a cancel) aborts the watch
        runs += 1;

        let ropts = render::Options::from_cli(cli, is_tty, None, Some(started.elapsed()));
        let stamp = chrono::Local::now().format("%H:%M:%S");
        let mut text = format!("-- watch every {secs}s \u{b7} run {runs} \u{b7} {stamp}\n");
        text.push_str(&render::format_all(&out.results, &ropts));
        for n in &out.notices {
            text.push_str(&format!("-- {n}\n"));
        }

        let mut stdout = std::io::stdout();
        if is_tty {
            if let Some(n) = prev_lines {
                // erase the previous frame in place (no alternate screen)
                let _ = write!(stdout, "\x1b[{n}A\x1b[0J");
            }
            let lines = text.lines().count();
            let (_, rows) = util::term_size();
            prev_lines = (lines + 1 < rows as usize).then_some(lines);
        }
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();

        // Sleep armed with a no-op cancel so Ctrl-C ends the loop instead of
        // killing the process mid-frame.
        let _sleep_guard = cancel::arm(|| {});
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline {
            if cancel::interrupted() {
                eprintln!("nsql: watch stopped after {runs} run(s)");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    eprintln!("nsql: watch stopped after {runs} run(s)");
    Ok(())
}

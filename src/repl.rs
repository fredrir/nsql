//! `--repeat`: a line-oriented session on one pinned connection, so BEGIN /
//! COMMIT, temp tables, and SET survive across statements — the one thing a
//! verb-per-invocation tool can't otherwise do.
//!
//! Deliberately tiny (the design docs cap the meta-command set so this never
//! becomes a second product): `\q` quit, `\e` edit the buffer in $EDITOR,
//! `\d [table]` describe, `\g` run the buffer as-is. A trailing `;` runs.

use crate::config::{Paths, Profile};
use crate::db::{self, RunOpts};
use crate::sql::{self, Dialect};
use crate::{cancel, cli, history, introspect, render, util};
use anyhow::{Context, Result};
use std::io::{BufRead, IsTerminal, Write};

pub fn run(cli: &cli::Cli, paths: &Paths, profile: &Profile) -> Result<()> {
    let mut conn = db::connect(profile)?;
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let dialect = conn.dialect();

    eprintln!(
        "nsql session on `{}` \u{2014} \\q quit \u{b7} \\e edit \u{b7} \\d [table] \u{b7} \\g run \u{b7} `;` runs",
        profile.name
    );

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut buf = String::new();

    loop {
        prompt(profile, buf.is_empty(), is_tty);
        let Some(line) = lines.next() else {
            break; // EOF
        };
        let line = line.context("reading stdin")?;
        let trimmed = line.trim();

        match trimmed {
            "\\q" => break,
            "\\e" => {
                match edit_in_editor(&buf) {
                    Ok(Some(new_buf)) => {
                        buf = new_buf;
                        for l in buf.lines() {
                            eprintln!("  {l}");
                        }
                        if complete(&buf, dialect) {
                            execute(&mut conn, &mut buf, cli, paths, profile, is_tty);
                        }
                    }
                    Ok(None) => eprintln!("(edit cancelled)"),
                    Err(e) => eprintln!("nsql: {e:#}"),
                }
                continue;
            }
            "\\g" => {
                if sql::split_statements(&buf, dialect).is_empty() {
                    eprintln!("(buffer is empty)");
                } else {
                    execute(&mut conn, &mut buf, cli, paths, profile, is_tty);
                }
                continue;
            }
            "\\d" => {
                introspect_run(
                    &mut conn,
                    profile,
                    &introspect::Verb::Tables { schema: None },
                );
                continue;
            }
            t if t.starts_with("\\d ") => {
                let name = t[3..].trim();
                introspect_run(&mut conn, profile, &introspect::Verb::Describe { name });
                continue;
            }
            t if t.starts_with('\\') => {
                eprintln!("unknown command `{t}` (\\q \\e \\d \\g)");
                continue;
            }
            _ => {}
        }

        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(&line);

        if complete(&buf, dialect) {
            execute(&mut conn, &mut buf, cli, paths, profile, is_tty);
        }
    }
    Ok(())
}

fn prompt(profile: &Profile, fresh: bool, is_tty: bool) {
    if !is_tty {
        return;
    }
    let mut err = std::io::stderr();
    let p = if fresh {
        let name = &profile.name;
        if profile.prod {
            format!("\x1b[1;31m{name}!\x1b[0m> ")
        } else if profile.readonly {
            format!("\x1b[32m{name}\x1b[0m> ")
        } else {
            format!("{name}> ")
        }
    } else {
        "  ...> ".to_string()
    };
    let _ = err.write_all(p.as_bytes());
    let _ = err.flush();
}

fn complete(buf: &str, dialect: Dialect) -> bool {
    !sql::split_statements(buf, dialect).is_empty() && sql::batch_complete(buf, dialect)
}

fn execute(
    conn: &mut db::Conn,
    buf: &mut String,
    cli: &cli::Cli,
    paths: &Paths,
    profile: &Profile,
    is_tty: bool,
) {
    let sql_text = std::mem::take(buf);
    if let Err(e) = db::guard(profile, &sql_text, cli.yes, is_tty) {
        eprintln!("nsql: {e:#}");
        return;
    }
    let started = std::time::Instant::now();
    cancel::reset();
    let guard = conn.cancel_closure().map(cancel::arm);
    let out = db::run_on(conn, &sql_text, &RunOpts::new(cli.all));
    drop(guard);
    match out {
        Ok(out) => {
            for n in &out.notices {
                eprintln!("nsql: {n}");
            }
            if !profile.no_history {
                let _ = history::record(paths, &profile.name, &sql_text);
            }
            let opts = render::Options::from_cli(cli, is_tty, None, Some(started.elapsed()));
            if let Err(e) = render::print_all(&out.results, &opts) {
                eprintln!("nsql: {e:#}");
            }
        }
        Err(e) => eprintln!("nsql: {e:#}"), // session survives statement errors
    }
}

fn introspect_run(conn: &mut db::Conn, profile: &Profile, verb: &introspect::Verb) {
    match introspect::query(profile, verb) {
        Ok(q) => {
            cancel::reset();
            let guard = conn.cancel_closure().map(cancel::arm);
            match db::run_on(conn, &q, &RunOpts::new(false)) {
                Ok(out) => {
                    let opts = render::Options {
                        format: render::Format::Table,
                        is_tty: true,
                        echo: None,
                        elapsed: None,
                        null_glyph: "(null)".to_string(),
                    };
                    if let Err(e) = render::print_all(&out.results, &opts) {
                        eprintln!("nsql: {e:#}");
                    }
                }
                Err(e) => eprintln!("nsql: {e:#}"),
            }
            drop(guard);
        }
        Err(e) => eprintln!("nsql: {e:#}"),
    }
}

fn edit_in_editor(current: &str) -> Result<Option<String>> {
    let tmp = util::secure_tempfile("nsql-repl", "sql")?;
    std::fs::write(&tmp, current)?;
    let (program, _) = util::resolve_editor()?;
    let status = std::process::Command::new(&program)
        .arg(&tmp)
        .status()
        .with_context(|| format!("spawning editor `{program}`"))?;
    let text = std::fs::read_to_string(&tmp).unwrap_or_default();
    std::fs::remove_file(&tmp).ok();
    if !status.success() {
        return Ok(None);
    }
    Ok(Some(text.trim_end().to_string()))
}

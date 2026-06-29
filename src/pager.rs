use crate::util;
use anyhow::Result;
use std::io::Write;
use std::process::{Command, Stdio};

pub fn emit(output: &str, is_tty: bool) -> Result<()> {
    let (_, rows) = util::term_size();
    let line_count = output.lines().count();

    if is_tty && line_count > rows as usize {
        if let Some(less) = pick_pager() {
            if page_with(&less, output).is_ok() {
                return Ok(());
            }
        }
    }

    let mut stdout = std::io::stdout();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn pick_pager() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("PAGER") {
        let prog = p.split_whitespace().next().unwrap_or("");
        let base = std::path::Path::new(prog)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(prog);
        if base == "less" {
            return util::find_on_path(prog);
        }
    }
    util::find_on_path("less")
}

fn page_with(less: &std::path::Path, output: &str) -> Result<()> {
    let mut child = Command::new(less)
        .args(["-RFX"])
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(output.as_bytes());
    }
    child.wait()?;
    Ok(())
}

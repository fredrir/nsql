//! Output emission with a scrollback-safe pager policy.
//!
//! The cardinal rule: never enter the alternate screen. So we only page when
//! output actually overflows AND a *known-safe* pager (`less`) is present, and
//! we invoke it with `-X` (no alt screen), `-F` (quit if it fits), `-R` (pass
//! colors). If only `more` (or nothing) is available we print directly —
//! `more` strips color and on many builds uses the alternate screen, which
//! would silently break the guarantee. (Verified: this machine has `more`, not
//! `less`.)

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
            // fall through to direct print if the pager failed to spawn
        }
    }

    let mut stdout = std::io::stdout();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// Resolve a known-alt-screen-safe pager. We honour $PAGER only when it is
/// `less`; anything else (notably `more`) is rejected to protect scrollback.
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
        .args(["-RFX"]) // -R colors, -F quit-if-fits, -X no alternate screen
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // Ignore broken-pipe if the user quits the pager early.
        let _ = stdin.write_all(output.as_bytes());
    }
    child.wait()?;
    Ok(())
}

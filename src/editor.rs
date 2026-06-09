//! The heart of the primary goal: compose a query in the user's REAL neovim as
//! a transient, blocking child process (the `git commit` / `psql \e` model).
//!
//! While nvim is up it owns the alternate screen; the instant it exits, the
//! normal screen + all prior scrollback are restored, and nsql prints the
//! result into that normal buffer. nsql itself NEVER emits the alternate-screen
//! escape — it is a guest-spawner, not a screen-taker.
//!
//! Run/cancel is decided by the child's EXIT CODE (robust, editor-agnostic):
//!   `,,` -> :write + :quit  (exit 0) -> run
//!   `,q` -> :cquit          (exit ≠0) -> cancel, run nothing
//! Plain `:wq` runs, `:cq`/`:q!` cancel. Discarding (`:q!`) leaves the temp
//! file unwritten, so we never run a half-typed query by accident.

use crate::config::{Paths, Profile};
use crate::util::{self, EditorKind};
use anyhow::{Context, Result};
use std::process::Command;

/// Header lines (as SQL comments) shown at the top of the scratch buffer. They
/// are stripped before the query runs and before it is echoed.
pub(crate) fn header(profile: &Profile) -> String {
    format!(
        "-- nsql \u{b7} profile: {} \u{b7} {}\n\
         -- ,, = run     ,q = cancel     (or :wq to run, :cq to cancel)\n",
        profile.name,
        util::redact_url(&profile.url)
    )
}

fn is_header_line(line: &str) -> bool {
    line.starts_with("-- nsql \u{b7}") || line.starts_with("-- ,, = run")
}

pub(crate) fn strip_header(text: &str) -> String {
    text.lines()
        .filter(|l| !is_header_line(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Open the editor, returning `Some(sql)` to run or `None` to cancel / no-op.
pub fn compose(paths: &Paths, profile: &Profile) -> Result<Option<String>> {
    write_inject(paths)?;

    let scratch = paths.scratch_for(&profile.name);
    let prior = std::fs::read_to_string(&scratch).unwrap_or_default();
    let initial = format!("{}{}", header(profile), strip_header(&prior));

    let tmp = util::secure_tempfile("nsql", "sql")?;
    std::fs::write(&tmp, &initial).with_context(|| format!("writing {}", tmp.display()))?;

    let (program, kind) = util::resolve_editor()?;
    let mut cmd = Command::new(&program);
    match kind {
        EditorKind::Nvim => {
            cmd.arg("-i")
                .arg("NONE") // disable shada (NOT `--cmd 'set shada=NONE'`, which throws E539)
                .arg(&tmp)
                .arg("-c")
                .arg("setfiletype sql")
                .arg("-c")
                .arg(format!("luafile {}", paths.inject_lua.display()));
        }
        EditorKind::Vimlike => {
            cmd.arg(&tmp).arg("-c").arg("setfiletype sql");
        }
        EditorKind::Other => {
            cmd.arg(&tmp);
        }
    }

    // Inherit the real tty so the editor draws normally; block until exit.
    let status = cmd
        .status()
        .with_context(|| format!("spawning editor `{program}`"))?;

    if !status.success() {
        // Cancel (`:cq`/`,q`/non-zero). Leave the prior scratch untouched.
        std::fs::remove_file(&tmp).ok();
        return Ok(None);
    }

    let edited = std::fs::read_to_string(&tmp).unwrap_or_default();
    std::fs::remove_file(&tmp).ok();

    let body = strip_header(&edited);
    // Persist the durable scratch (without the header) so the next launch
    // resumes exactly where the user left off.
    if let Err(e) = persist_scratch(&scratch, &body) {
        eprintln!("nsql: warning: could not save scratch: {e:#}");
    }

    if crate::db::strip_sql_comments(&body).trim().is_empty() {
        return Ok(None); // nothing meaningful to run
    }
    Ok(Some(body))
}

pub(crate) fn persist_scratch(path: &std::path::Path, body: &str) -> Result<()> {
    std::fs::write(path, body)?;
    set_0600(path);
    Ok(())
}

fn set_0600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

const INJECT_LUA: &str = include_str!("../assets/inject.lua");

pub(crate) fn write_inject(paths: &Paths) -> Result<()> {
    std::fs::write(&paths.inject_lua, INJECT_LUA)
        .with_context(|| format!("writing {}", paths.inject_lua.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_stripped() {
        let prof = Profile {
            name: "local".into(),
            url: "sqlite::memory:".into(),
            prod: false,
            readonly: false,
            no_history: false,
        };
        let buf = format!("{}select 1;\n", header(&prof));
        let body = strip_header(&buf);
        assert!(!body.contains("nsql \u{b7} profile"));
        assert!(body.contains("select 1;"));
    }
}

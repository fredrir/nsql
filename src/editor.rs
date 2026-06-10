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
/// `portable` (over SSH / `--clean`) launches nvim with nsql's bundled minimal
/// config instead of the user's.
pub fn compose(paths: &Paths, profile: &Profile, portable: bool) -> Result<Option<String>> {
    write_inject(paths)?;

    let scratch = paths.scratch_for(&profile.name);
    let prior = std::fs::read_to_string(&scratch).unwrap_or_default();
    // Clean buffer: just the prior scratch (strip_header also scrubs any legacy
    // header lines from scratch files written by older versions).
    let initial = strip_header(&prior);

    let tmp = util::secure_tempfile("nsql", "sql")?;
    std::fs::write(&tmp, &initial).with_context(|| format!("writing {}", tmp.display()))?;

    let (program, kind) = util::resolve_editor()?;
    let mut cmd = Command::new(&program);
    // Don't leak a connection secret into the editor's env (its plugins could
    // read it); nsql passes connections to any LSP explicitly, not via PGPASSWORD.
    cmd.env("NSQL_DB", &profile.name)
        .env("NSQL_URL", util::redact_url(&profile.url))
        .env("NSQL_PROD", if profile.prod { "1" } else { "0" })
        .env("NSQL_SAFE", if profile.readonly { "1" } else { "0" })
        .env_remove("PGPASSWORD");
    match kind {
        EditorKind::Nvim => {
            cmd.arg("-i").arg("NONE"); // disable shada (NOT `--cmd 'set shada=NONE'`, throws E539)
            if portable {
                // nsql's own minimal config instead of the user's (identical anywhere).
                cmd.arg("-u").arg(portable_init_path(paths));
            }
            cmd.arg(&tmp)
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
    // O_EXCL+0600 temp + rename — no write-then-chmod TOCTOU. The scratch may
    // hold a partially-typed query, so keep it owner-only.
    crate::util::write_private(path, body.as_bytes())
}

const INJECT_LUA: &str = include_str!("../assets/inject.lua");
const INIT_LUA: &str = include_str!("../assets/nsql_init.lua");

pub(crate) fn write_inject(paths: &Paths) -> Result<()> {
    std::fs::write(&paths.inject_lua, INJECT_LUA)
        .with_context(|| format!("writing {}", paths.inject_lua.display()))?;
    std::fs::write(portable_init_path(paths), INIT_LUA)
        .with_context(|| "writing nsql_init.lua")?;
    Ok(())
}

/// Path to nsql's bundled minimal nvim config (used in `--clean` / over-SSH mode),
/// written next to inject.lua.
pub(crate) fn portable_init_path(paths: &Paths) -> std::path::PathBuf {
    paths.inject_lua.with_file_name("nsql_init.lua")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_header_is_stripped() {
        // Scratch files written by older versions may still carry the header;
        // strip_header must scrub it so the buffer opens clean.
        let legacy = "-- nsql \u{b7} profile: x \u{b7} url\n\
                      -- ,, = run     ,q = cancel\n\
                      select 1;\n";
        let body = strip_header(legacy);
        assert!(!body.contains("nsql"));
        assert!(body.contains("select 1;"));
    }
}

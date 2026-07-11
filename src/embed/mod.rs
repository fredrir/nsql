mod grid;
mod keys;
mod results;
mod ring;
mod schema;
mod session;

use crate::config::{Paths, Profile};
use crate::{editor, util};
use anyhow::{Context, Result};
use nvim_rs::compat::tokio::Compat;
use ratatui::crossterm::terminal::disable_raw_mode;
use tokio::process::ChildStdin;

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

    let res = rt.block_on(session::run_session(paths, &tmp, profile, portable));
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

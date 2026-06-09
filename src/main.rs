//! nsql — run SQL from your terminal, composed in your real neovim, without
//! taking over the screen. See DESIGN.md for the architecture and rationale.

mod backends;
mod cli;
mod config;
mod db;
mod editor;
#[cfg(feature = "embed-editor")]
mod embed;
mod favorites;
mod history;
mod pager;
mod render;
mod secrets;
mod util;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands};
use config::{Config, Paths, Profile};
use std::io::{IsTerminal, Read};

fn main() {
    if let Err(e) = real_main() {
        eprintln!("nsql: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    // Allow a leading `@name` anywhere in argv as a shorthand for --profile.
    let mut argv: Vec<String> = std::env::args().collect();
    let mut profile_at: Option<String> = None;
    argv.retain(|a| match a.strip_prefix('@') {
        Some(rest) if !rest.is_empty() && !a.starts_with("@@") => {
            profile_at = Some(rest.to_string());
            false
        }
        _ => true,
    });

    let cli = Cli::parse_from(argv);
    let profile_override = profile_at.or_else(|| cli.profile.clone());

    let paths = Paths::resolve()?;
    let mut cfg = Config::load_or_init(&paths)?;

    if let Some(cmd) = cli.command {
        return run_subcommand(cmd, &paths, &mut cfg, profile_override.as_deref());
    }

    let profile = cfg.select(profile_override.as_deref())?;
    let out_tty = std::io::stdout().is_terminal();

    // Acquire the SQL to run and whether to echo it above the result.
    let (sql, echo) = acquire_sql(&cli, &paths, &profile)?;
    let Some(sql) = sql else {
        eprintln!("nsql: cancelled (nothing run)");
        return Ok(());
    };

    if db::strip_sql_comments(&sql).trim().is_empty() {
        eprintln!("nsql: nothing to run");
        return Ok(());
    }

    db::guard(&profile, &sql, cli.yes, out_tty)?;
    let started = std::time::Instant::now();
    let result = db::run(&profile, &sql, cli.all)?;
    let elapsed = started.elapsed();

    if !profile.no_history {
        if let Err(e) = history::record(&paths, &profile.name, &sql) {
            eprintln!("nsql: warning: could not record history: {e:#}");
        }
    }

    let echo_text = if echo && out_tty { Some(sql.clone()) } else { None };
    let opts = render::Options::from_cli(&cli, out_tty, echo_text, Some(elapsed));
    render::print(&result, &opts)
}

/// Returns (Some(sql) to run | None to cancel, echo?).
fn acquire_sql(cli: &Cli, paths: &Paths, profile: &Profile) -> Result<(Option<String>, bool)> {
    if cli.edit || cli.embed {
        return Ok((compose(paths, profile, cli.embed)?, true));
    }
    if let Some(e) = &cli.execute {
        return Ok((Some(e.clone()), false));
    }
    if let Some(f) = &cli.file {
        let s = std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?;
        return Ok((Some(s), false));
    }
    if let Some(name) = &cli.favorite {
        return Ok((Some(favorites::load(paths, name)?), true));
    }
    // No explicit source: read a pipe if present, otherwise open the editor.
    if !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok((Some(s), false));
    }
    Ok((compose(paths, profile, false)?, true))
}

/// Open the compose editor — the zero-flash embedded renderer when requested and
/// available, otherwise the proven transient-child (Mode 1) editor.
fn compose(paths: &Paths, profile: &Profile, want_embed: bool) -> Result<Option<String>> {
    if want_embed {
        #[cfg(feature = "embed-editor")]
        {
            return embed::compose(paths, profile);
        }
        #[cfg(not(feature = "embed-editor"))]
        {
            anyhow::bail!("--embed requires building with `--features embed-editor`");
        }
    }
    editor::compose(paths, profile)
}

fn run_subcommand(
    cmd: Commands,
    paths: &Paths,
    cfg: &mut Config,
    profile_override: Option<&str>,
) -> Result<()> {
    match cmd {
        Commands::Profiles => {
            if cfg.profiles.is_empty() {
                println!("(no profiles — add one with `nsql connect <name> --url ...`)");
            }
            for p in &cfg.profiles {
                let marker = if cfg.default.as_deref() == Some(&p.name) {
                    "*"
                } else {
                    " "
                };
                let mut tags = Vec::new();
                if p.prod {
                    tags.push("prod");
                }
                if p.readonly {
                    tags.push("readonly");
                }
                let tags = if tags.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", tags.join(","))
                };
                println!(
                    "{marker} {:<16} {}{}",
                    p.name,
                    util::redact_url(&p.url),
                    tags
                );
            }
            Ok(())
        }

        Commands::Connect {
            name,
            url,
            set_password,
            prod,
            readonly,
        } => {
            let existing = cfg.profiles.iter().find(|p| p.name == name).cloned();
            let url = url
                .or_else(|| existing.as_ref().map(|p| p.url.clone()))
                .context("a new profile needs --url (e.g. --url sqlite:///path/db.sqlite)")?;
            let profile = Profile {
                name: name.clone(),
                url,
                prod: prod || existing.as_ref().map(|p| p.prod).unwrap_or(false),
                readonly: readonly || existing.as_ref().map(|p| p.readonly).unwrap_or(false),
                no_history: existing.as_ref().map(|p| p.no_history).unwrap_or(false),
            };
            cfg.upsert(profile);
            cfg.save(paths)?;
            println!("saved profile `{name}`");

            if set_password {
                let pw = rpassword::prompt_password(format!("Password for `{name}`: "))?;
                secrets::set(&name, &pw)?;
                println!("stored password for `{name}` in the OS keyring");
            }
            Ok(())
        }

        Commands::Save { name } => {
            let profile = cfg.select(profile_override)?;
            let scratch = paths.scratch_for(&profile.name);
            let content = std::fs::read_to_string(&scratch).unwrap_or_default();
            if content.trim().is_empty() {
                anyhow::bail!(
                    "scratch for profile `{}` is empty — nothing to save",
                    profile.name
                );
            }
            let p = favorites::save(paths, &name, &content)?;
            println!("saved favorite `{name}` -> {}", p.display());
            Ok(())
        }

        Commands::Favorites => {
            let names = favorites::list(paths)?;
            if names.is_empty() {
                println!("(no favorites yet — `nsql save <name>`)");
            }
            for n in names {
                println!("{n}");
            }
            Ok(())
        }

        Commands::History { limit } => history::list(paths, limit),

        Commands::Discover => {
            println!(
                "discovery is Phase 2/3: docker/podman container inspection first, \
                 then an opt-in, rate-limited local-/24 TCP scan (mDNS dropped). Not yet implemented."
            );
            Ok(())
        }
    }
}

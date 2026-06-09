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

    // A bare connection URL (`nsql postgres://…`) is an ad-hoc, unsaved
    // connection — pull it out before clap so it isn't read as a subcommand.
    let adhoc_url = extract_adhoc_url(&mut argv);

    let cli = Cli::parse_from(argv);
    let profile_override = profile_at.or_else(|| cli.profile.clone());

    let paths = Paths::resolve()?;
    let mut cfg = Config::load_or_init(&paths)?;

    if let Some(cmd) = cli.command {
        return run_subcommand(cmd, &paths, &mut cfg, profile_override.as_deref());
    }

    let profile = match &adhoc_url {
        Some(url) => Profile {
            name: adhoc_name(url),
            url: url.clone(),
            prod: false,
            readonly: false,
            no_history: false,
        },
        None => cfg.select(profile_override.as_deref())?,
    };
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
        return Ok((compose(paths, profile, cli)?, true));
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
    Ok((compose(paths, profile, cli)?, true))
}

/// True if `s` looks like a database connection URL we can dispatch.
fn is_db_url(s: &str) -> bool {
    matches!(
        s.split_once("://").map(|(scheme, _)| scheme),
        Some("postgres" | "postgresql" | "mysql" | "mariadb" | "sqlite")
    )
}

/// Pull a leading bare connection URL out of argv (the first positional token, so
/// it never swallows an option value like `connect --url …`). Returns the URL.
fn extract_adhoc_url(argv: &mut Vec<String>) -> Option<String> {
    const VALUE_FLAGS: &[&str] = &[
        "-e", "--execute", "-f", "--file", "-F", "--favorite", "-p", "--profile", "--format",
    ];
    let mut i = 1;
    let mut skip_value = false;
    while i < argv.len() {
        let a = &argv[i];
        if skip_value {
            skip_value = false;
            i += 1;
            continue;
        }
        if a == "--" {
            break;
        }
        if a.starts_with('-') {
            if VALUE_FLAGS.contains(&a.as_str()) {
                skip_value = true;
            }
            i += 1;
            continue;
        }
        // First positional token: take it only if it's a URL, else leave it
        // (it's a subcommand or other arg) and stop scanning.
        return if is_db_url(a) {
            Some(argv.remove(i))
        } else {
            None
        };
    }
    None
}

/// A short, secret-free label for an ad-hoc connection (the database name).
fn adhoc_name(url: &str) -> String {
    url.rsplit('/')
        .next()
        .map(|s| s.split('?').next().unwrap_or(s))
        .filter(|s| !s.is_empty())
        .unwrap_or("ad-hoc")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("nsql")
            .chain(args.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn extracts_leading_url() {
        let mut a = argv(&["postgres://u:p@h/db", "-e", "select 1"]);
        assert_eq!(
            extract_adhoc_url(&mut a).as_deref(),
            Some("postgres://u:p@h/db")
        );
        assert_eq!(a, argv(&["-e", "select 1"]));
    }

    #[test]
    fn does_not_steal_connect_url() {
        // The URL here is the value of `--url`, not a bare positional.
        let mut a = argv(&["connect", "prod", "--url", "postgres://u@h/db"]);
        assert_eq!(extract_adhoc_url(&mut a), None);
    }

    #[test]
    fn ignores_non_url_positional() {
        let mut a = argv(&["profiles"]);
        assert_eq!(extract_adhoc_url(&mut a), None);
    }

    #[test]
    fn url_after_value_flag() {
        let mut a = argv(&["-p", "x", "sqlite:///t.db"]);
        assert_eq!(extract_adhoc_url(&mut a).as_deref(), Some("sqlite:///t.db"));
    }

    #[test]
    fn adhoc_name_is_dbname() {
        assert_eq!(
            adhoc_name("postgres://u:p@h:5433/pyparser_llunde"),
            "pyparser_llunde"
        );
        assert_eq!(adhoc_name("postgres://u@h/db?sslmode=require"), "db");
    }
}

/// Open the compose editor. The zero-flash inline editor is the default when
/// built with the `embed-editor` feature and stdin/stdout are a real terminal;
/// `--classic` (or a non-tty / a build without the feature) uses the proven
/// transient-child editor (Mode 1).
fn compose(paths: &Paths, profile: &Profile, cli: &Cli) -> Result<Option<String>> {
    #[cfg(feature = "embed-editor")]
    {
        let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        if cli.embed && !is_tty {
            anyhow::bail!("--embed needs an interactive terminal");
        }
        // Inline zero-flash editor: the default on a real terminal, unless
        // --classic. Non-tty (e.g. --edit while piping) falls back to Mode 1.
        if (cli.embed || !cli.classic) && is_tty {
            return embed::compose(paths, profile);
        }
    }
    #[cfg(not(feature = "embed-editor"))]
    if cli.embed {
        anyhow::bail!("--embed requires building with `--features embed-editor`");
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

//! nsql — run SQL from your terminal, composed in your real neovim, without
//! taking over the screen. See DESIGN.md for the architecture and rationale.

mod backends;
mod cli;
mod config;
mod creds;
mod db;
mod editor;
#[cfg(feature = "embed-editor")]
mod embed;
mod favorites;
mod history;
mod pager;
mod recents;
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

    // A leading positional (`nsql postgres://…`, `nsql staging`, `nsql 2`) is a
    // connection target — pull it out before clap so it isn't read as a subcommand.
    let target = extract_target(&mut argv);

    let cli = Cli::parse_from(argv);
    let profile_override = profile_at.or_else(|| cli.profile.clone());

    let paths = Paths::resolve()?;
    let mut cfg = Config::load_or_init(&paths)?;

    if let Some(cmd) = cli.command {
        return run_subcommand(cmd, &paths, &mut cfg, profile_override.as_deref());
    }

    // Resolve the connection: explicit target (URL / recents label / index / saved
    // profile) > -p/@name > resume the most-recent > the bootstrapped `local`.
    let profile = if let Some(t) = &target {
        if is_db_url(t) {
            // An inline password in an ad-hoc URL would be lost on resume: recents
            // stores the URL WITHOUT it (no plaintext on disk). Migrate it into the
            // OS keyring, keyed on the stable user@host:port/db identity, so bare
            // `nsql` reconnects. The full URL stays in this session's profile so the
            // current connection still works immediately.
            if let Some(pw) = util::url_password(t) {
                if let Some(key) = creds::pg_identity(t).map(|id| creds::identity_key(&id)) {
                    if let Err(e) = secrets::set(&key, &pw) {
                        eprintln!("nsql: note: couldn't save the password for resume: {e:#}");
                    }
                }
            }
            Profile {
                name: adhoc_name(t),
                url: t.clone(),
                prod: false,
                readonly: false,
                no_history: false,
            }
        } else if let Some(r) = recents::resolve(&paths, t) {
            r.to_profile(&cfg)
        } else if let Some(p) = cfg.profiles.iter().find(|p| &p.name == t).cloned() {
            p
        } else {
            anyhow::bail!(
                "unknown connection `{t}` — pass a URL, a saved profile (`nsql profiles`), \
                 or a recents label/number"
            );
        }
    } else if let Some(name) = &profile_override {
        cfg.select(Some(name))?
    } else {
        match recents::most_recent(&paths) {
            Some(r) => r.to_profile(&cfg),
            None => cfg.select(None)?,
        }
    };

    // Remember interactively-chosen connections so bare `nsql` resumes them. Skip
    // scripted one-shots (`-e`/`-f`/stdin pipes) so scripts don't pin connections.
    let scripted =
        cli.execute.is_some() || cli.file.is_some() || cli.favorite.is_some() || !std::io::stdin().is_terminal();
    if target.is_some() || !scripted {
        let saved = cfg.profiles.iter().any(|p| p.name == profile.name);
        recents::record(&paths, &profile, saved);
    }

    let out_tty = std::io::stdout().is_terminal();

    // Acquire what to do. The interactive embed session runs queries itself and
    // returns Handled — nothing for main to run or print.
    let (sql, echo) = match acquire_sql(&cli, &paths, &profile)? {
        Acquired::Handled => return Ok(()),
        Acquired::Cancelled => {
            eprintln!("nsql: cancelled (nothing run)");
            return Ok(());
        }
        Acquired::Run { sql, echo } => (sql, echo),
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

/// What `acquire_sql` resolved the invocation to.
enum Acquired {
    /// A query for main to run + print (one-shot / classic editor).
    Run { sql: String, echo: bool },
    /// The user cancelled the editor with nothing to run.
    Cancelled,
    /// The interactive embed session already ran everything in-session.
    #[cfg_attr(not(feature = "embed-editor"), allow(dead_code))]
    Handled,
}

fn acquire_sql(cli: &Cli, paths: &Paths, profile: &Profile) -> Result<Acquired> {
    if cli.edit || cli.embed {
        return compose(paths, profile, cli);
    }
    if let Some(e) = &cli.execute {
        return Ok(Acquired::Run {
            sql: e.clone(),
            echo: false,
        });
    }
    if let Some(f) = &cli.file {
        let sql = std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?;
        return Ok(Acquired::Run { sql, echo: false });
    }
    if let Some(name) = &cli.favorite {
        return Ok(Acquired::Run {
            sql: favorites::load(paths, name)?,
            echo: true,
        });
    }
    // No explicit source: read a pipe if present, otherwise open the editor.
    if !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(Acquired::Run { sql: s, echo: false });
    }
    compose(paths, profile, cli)
}

/// True if `s` looks like a database connection URL we can dispatch.
fn is_db_url(s: &str) -> bool {
    matches!(
        s.split_once("://").map(|(scheme, _)| scheme),
        Some("postgres" | "postgresql" | "mysql" | "mariadb" | "sqlite")
    )
}

/// Pull a leading connection target out of argv — the first positional token (so
/// it never swallows an option value like `connect --url …`). Returns it unless
/// it's a subcommand name, which is left for clap. The target may be a URL, a
/// recents label, a recents index, or a saved-profile name.
fn extract_target(argv: &mut Vec<String>) -> Option<String> {
    const VALUE_FLAGS: &[&str] = &[
        "-e", "--execute", "-f", "--file", "-F", "--favorite", "-p", "--profile", "--format",
    ];
    const SUBCOMMANDS: &[&str] = &[
        "profiles", "connect", "save", "favorites", "history", "discover", "help",
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
        // First positional: leave subcommands for clap; otherwise it's a target.
        return if SUBCOMMANDS.contains(&a.as_str()) {
            None
        } else {
            Some(argv.remove(i))
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
            extract_target(&mut a).as_deref(),
            Some("postgres://u:p@h/db")
        );
        assert_eq!(a, argv(&["-e", "select 1"]));
    }

    #[test]
    fn extracts_label_and_index_targets() {
        // A bare non-URL positional is a recents label / index, now extracted.
        let mut a = argv(&["staging"]);
        assert_eq!(extract_target(&mut a).as_deref(), Some("staging"));
        let mut b = argv(&["2"]);
        assert_eq!(extract_target(&mut b).as_deref(), Some("2"));
    }

    #[test]
    fn does_not_steal_connect_url_or_subcommands() {
        // The URL here is the value of `--url`, not a bare positional.
        let mut a = argv(&["connect", "prod", "--url", "postgres://u@h/db"]);
        assert_eq!(extract_target(&mut a), None);
        // Subcommands are left for clap.
        let mut b = argv(&["profiles"]);
        assert_eq!(extract_target(&mut b), None);
    }

    #[test]
    fn target_after_value_flag() {
        let mut a = argv(&["-p", "x", "sqlite:///t.db"]);
        assert_eq!(extract_target(&mut a).as_deref(), Some("sqlite:///t.db"));
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
fn compose(paths: &Paths, profile: &Profile, cli: &Cli) -> Result<Acquired> {
    #[cfg(feature = "embed-editor")]
    {
        let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        if cli.embed && !is_tty {
            anyhow::bail!("--embed needs an interactive terminal");
        }
        // Inline zero-flash session: the default on a real terminal, unless
        // --classic. The session runs queries itself and returns Handled.
        if (cli.embed || !cli.classic) && is_tty {
            embed::compose(paths, profile)?;
            return Ok(Acquired::Handled);
        }
    }
    #[cfg(not(feature = "embed-editor"))]
    if cli.embed {
        anyhow::bail!("--embed requires building with `--features embed-editor`");
    }

    Ok(match editor::compose(paths, profile)? {
        Some(sql) => Acquired::Run { sql, echo: true },
        None => Acquired::Cancelled,
    })
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
            let raw_url = url
                .or_else(|| existing.as_ref().map(|p| p.url.clone()))
                .context("a new profile needs --url (e.g. --url sqlite:///path/db.sqlite)")?;
            // NEVER persist a password to config.toml: strip it from the stored
            // url and migrate any embedded password into the OS keyring.
            let embedded_pw = util::url_password(&raw_url);
            let profile = Profile {
                name: name.clone(),
                url: util::strip_url_password(&raw_url),
                prod: prod || existing.as_ref().map(|p| p.prod).unwrap_or(false),
                readonly: readonly || existing.as_ref().map(|p| p.readonly).unwrap_or(false),
                no_history: existing.as_ref().map(|p| p.no_history).unwrap_or(false),
            };

            // Store the secret (prompted, or migrated from the URL) keyed on the
            // stable user@host:port/db identity — never the profile name — so a
            // re-typed URL resolves the same entry and two databases can't collide.
            let pw = if set_password {
                Some(rpassword::prompt_password(format!("Password for `{name}`: "))?)
            } else {
                embedded_pw
            };
            if let Some(pw) = pw {
                match creds::pg_identity(&profile.url).map(|id| creds::identity_key(&id)) {
                    Some(key) => {
                        secrets::set(&key, &pw)?;
                        println!("stored password for `{key}` in the OS keyring");
                    }
                    None => eprintln!("nsql: note: this URL has no host/db to key a secret on"),
                }
            }

            cfg.upsert(profile);
            cfg.save(paths)?;
            println!("saved profile `{name}`");
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

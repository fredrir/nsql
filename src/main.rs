mod backends;
mod cancel;
mod cli;
mod config;
mod creds;
mod db;
mod editor;
#[cfg(feature = "embed-editor")]
mod embed;
mod export;
mod favorites;
mod history;
mod introspect;
mod pager;
mod params;
mod recents;
mod render;
mod repl;
mod secrets;
mod sql;
mod tunnel;
mod util;
mod watch;

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
    let mut argv: Vec<String> = std::env::args().collect();
    let mut profile_at: Option<String> = None;
    argv.retain(|a| match a.strip_prefix('@') {
        Some(rest) if !rest.is_empty() && !a.starts_with("@@") => {
            profile_at = Some(rest.to_string());
            false
        }
        _ => true,
    });

    let target = extract_target(&mut argv);

    let mut cli = Cli::parse_from(argv);
    let profile_override = profile_at.or_else(|| cli.profile.clone());

    let paths = Paths::resolve()?;
    let mut cfg = Config::load_or_init(&paths)?;
    if cli.null.is_none() {
        cli.null = cfg.null_glyph.clone();
    }

    if let Some(cmd) = cli.command {
        return run_subcommand(
            cmd,
            &paths,
            &mut cfg,
            profile_override.as_deref(),
            target.as_deref(),
        );
    }

    if !cli.params.is_empty() && cli.favorite.is_none() {
        anyhow::bail!("-P/--param only applies to favorites (-F name)");
    }

    // `nsql @prod users` — a bare table name with an explicit profile becomes
    // SELECT * FROM users LIMIT 100.
    let mut table_shorthand: Option<String> = None;

    let profile = if let Some(t) = &target {
        if is_db_url(t) {
            if let Some(pw) = util::url_password(t) {
                if let Some(key) = creds::identity(t).map(|id| creds::identity_key(&id)) {
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
                ssh: None,
            }
        } else if let Some(r) = recents::resolve(&paths, t) {
            r.to_profile(&cfg)
        } else if let Some(p) = cfg.profiles.iter().find(|p| &p.name == t).cloned() {
            p
        } else if profile_override.is_some() && is_bare_table(t) {
            let quoted = t
                .split('.')
                .map(sql::quote_ident)
                .collect::<Vec<_>>()
                .join(".");
            table_shorthand = Some(format!("select * from {quoted} limit 100"));
            cfg.select(profile_override.as_deref())?
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

    let mut profile = profile;
    if cli.safe {
        profile.readonly = true;
    }

    let scripted = cli.execute.is_some()
        || cli.file.is_some()
        || cli.favorite.is_some()
        || table_shorthand.is_some()
        || !std::io::stdin().is_terminal();
    if (target.is_some() && table_shorthand.is_none()) || !scripted {
        let saved = cfg.profiles.iter().any(|p| p.name == profile.name);
        recents::record(&paths, &profile, saved);
    }

    if cli.repeat {
        return repl::run(&cli, &paths, &profile);
    }

    // --last: preseed the scratch buffer with the most recent query and edit it
    if cli.last {
        match history::last_for(&paths, &profile.name)? {
            Some(prev) => {
                editor::persist_scratch(&paths.scratch_for(&profile.name), &prev)?;
            }
            None => anyhow::bail!("no history yet for profile `{}`", profile.name),
        }
    }

    let out_tty = std::io::stdout().is_terminal();

    let (sql_text, echo) = if let Some(shorthand) = table_shorthand {
        (shorthand, true)
    } else {
        match acquire_sql(&cli, &paths, &profile)? {
            Acquired::Handled => return Ok(()),
            Acquired::Cancelled => {
                eprintln!("nsql: cancelled (nothing run)");
                return Ok(());
            }
            Acquired::Run { sql, echo } => (sql, echo),
        }
    };

    if db::strip_sql_comments(&sql_text).trim().is_empty() {
        eprintln!("nsql: nothing to run");
        return Ok(());
    }

    // History should show the favorite template, not the bound values.
    let history_text = match &cli.favorite {
        Some(name) => favorites::load(&paths, name).unwrap_or_else(|_| sql_text.clone()),
        None => sql_text.clone(),
    };

    db::guard(&profile, &sql_text, cli.yes, out_tty)?;

    if matches!(db::first_keyword(&sql_text).as_str(), "BEGIN" | "START") {
        eprintln!(
            "nsql: note: BEGIN has no effect here — the connection closes after this run \
             (use --repeat for a session with transactions)"
        );
    }

    if let Some(out_path) = &cli.out {
        let fmt = export::resolve_format(cli.format.as_ref(), out_path)?;
        let glyph = cli.null.clone().unwrap_or_default();
        return export::run(&profile, &sql_text, out_path, fmt, &glyph);
    }

    if let Some(secs) = cli.watch {
        if cli.execute.is_none() && cli.file.is_none() && cli.favorite.is_none() {
            anyhow::bail!("--watch needs a scripted query (-e, -f, or -F)");
        }
        return watch::run(&profile, &sql_text, secs, &cli, out_tty);
    }

    let mut conn = db::connect(&profile)?;
    cancel::reset();
    let cancel_guard = conn.cancel_closure().map(cancel::arm);
    let started = std::time::Instant::now();
    let run_opts = db::RunOpts {
        cap: if cli.all { usize::MAX } else { db::ROW_CAP },
        typed: wants_typed(&cli),
    };
    let out = db::run_on(&mut conn, &sql_text, &run_opts)?;
    let elapsed = started.elapsed();
    drop(cancel_guard);

    if !profile.no_history {
        if let Err(e) = history::record(&paths, &profile.name, &history_text) {
            eprintln!("nsql: warning: could not record history: {e:#}");
        }
    }

    for notice in &out.notices {
        eprintln!("nsql: {notice}");
    }

    let echo_text = if echo && out_tty {
        Some(sql_text.clone())
    } else {
        None
    };
    let opts = render::Options::from_cli(&cli, out_tty, echo_text, Some(elapsed));
    render::print_all(&out.results, &opts)
}

fn wants_typed(cli: &Cli) -> bool {
    cli.json
        || matches!(
            cli.format,
            Some(cli::FormatArg::Json) | Some(cli::FormatArg::Ndjson)
        )
}

enum Acquired {
    Run {
        sql: String,
        echo: bool,
    },
    Cancelled,
    #[cfg_attr(not(feature = "embed-editor"), allow(dead_code))]
    Handled,
}

fn acquire_sql(cli: &Cli, paths: &Paths, profile: &Profile) -> Result<Acquired> {
    if cli.edit || cli.embed || cli.last {
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
        let template = favorites::load(paths, name)?;
        let bindings = params::parse_bindings(&cli.params)?;
        let dialect = sql::Dialect::for_scheme(profile.scheme());
        let sql = params::substitute(&template, &bindings, cli.unsafe_subst, dialect)?;
        return Ok(Acquired::Run { sql, echo: true });
    }
    if !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(Acquired::Run {
            sql: s,
            echo: false,
        });
    }
    compose(paths, profile, cli)
}

fn is_db_url(s: &str) -> bool {
    matches!(
        s.split_once("://").map(|(scheme, _)| scheme),
        Some("postgres" | "postgresql" | "mysql" | "mariadb" | "sqlite" | "duckdb")
    )
}

fn is_bare_table(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        && s.chars().filter(|&c| c == '.').count() <= 1
        && !s.ends_with('.')
}

fn extract_target(argv: &mut Vec<String>) -> Option<String> {
    const VALUE_FLAGS: &[&str] = &[
        "-e",
        "--execute",
        "-f",
        "--file",
        "-F",
        "--favorite",
        "-p",
        "--profile",
        "-P",
        "--param",
        "--format",
        "--null",
        "--out",
        "--watch",
    ];
    const SUBCOMMANDS: &[&str] = &[
        "profiles",
        "connect",
        "save",
        "favorites",
        "history",
        "tables",
        "describe",
        "schemas",
        "completions",
        "discover",
        "help",
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
        return if SUBCOMMANDS.contains(&a.as_str()) {
            None
        } else {
            Some(argv.remove(i))
        };
    }
    None
}

fn adhoc_name(url: &str) -> String {
    url.rsplit('/')
        .next()
        .map(|s| s.split('?').next().unwrap_or(s))
        .filter(|s| !s.is_empty())
        .unwrap_or("ad-hoc")
        .to_string()
}

fn compose(paths: &Paths, profile: &Profile, cli: &Cli) -> Result<Acquired> {
    let portable = if cli.no_clean {
        false
    } else {
        cli.clean || is_ssh()
    };

    #[cfg(feature = "embed-editor")]
    {
        let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        if cli.embed && !is_tty {
            anyhow::bail!("--embed needs an interactive terminal");
        }
        if (cli.embed || !cli.classic) && is_tty {
            embed::compose(paths, profile, portable)?;
            return Ok(Acquired::Handled);
        }
    }
    #[cfg(not(feature = "embed-editor"))]
    if cli.embed {
        anyhow::bail!("--embed requires building with `--features embed-editor`");
    }

    Ok(match editor::compose(paths, profile, portable)? {
        Some(sql) => Acquired::Run { sql, echo: true },
        None => Acquired::Cancelled,
    })
}

fn is_ssh() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

fn run_subcommand(
    cmd: Commands,
    paths: &Paths,
    cfg: &mut Config,
    profile_override: Option<&str>,
    target: Option<&str>,
) -> Result<()> {
    match cmd {
        Commands::Profiles => {
            let color = std::io::stdout().is_terminal();
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
                    tags.push(if color {
                        "\x1b[1;31mprod\x1b[0m".to_string()
                    } else {
                        "prod".to_string()
                    });
                }
                if p.readonly {
                    tags.push(if color {
                        "\x1b[32mreadonly\x1b[0m".to_string()
                    } else {
                        "readonly".to_string()
                    });
                }
                if p.ssh.is_some() {
                    tags.push("ssh".to_string());
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
            ssh,
        } => {
            let existing = cfg.profiles.iter().find(|p| p.name == name).cloned();
            let raw_url = url
                .or_else(|| existing.as_ref().map(|p| p.url.clone()))
                .context("a new profile needs --url (e.g. --url sqlite:///path/db.sqlite)")?;
            let embedded_pw = util::url_password(&raw_url);
            let profile = Profile {
                name: name.clone(),
                url: util::strip_url_password(&raw_url),
                prod: prod || existing.as_ref().map(|p| p.prod).unwrap_or(false),
                readonly: readonly || existing.as_ref().map(|p| p.readonly).unwrap_or(false),
                no_history: existing.as_ref().map(|p| p.no_history).unwrap_or(false),
                ssh: ssh.or_else(|| existing.as_ref().and_then(|p| p.ssh.clone())),
            };

            let pw = if set_password {
                Some(rpassword::prompt_password(format!(
                    "Password for `{name}`: "
                ))?)
            } else {
                embedded_pw
            };
            if let Some(pw) = pw {
                match creds::identity(&profile.url).map(|id| creds::identity_key(&id)) {
                    // A missing keyring (headless box, no secret service) must
                    // not fail `connect` — the profile still works via
                    // PGPASSWORD/MYSQL_PWD, ~/.pgpass, or a password in the URL.
                    Some(key) => match secrets::set(&key, &pw) {
                        Ok(()) => println!("stored password for `{key}` in the OS keyring"),
                        Err(e) => eprintln!(
                            "nsql: warning: couldn't store the password in the OS keyring \
                             ({e:#}) — use PGPASSWORD/MYSQL_PWD or ~/.pgpass instead"
                        ),
                    },
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

        Commands::Tables { schema } => introspect_command(
            cfg,
            paths,
            profile_override,
            target,
            &introspect::Verb::Tables {
                schema: schema.as_deref(),
            },
        ),
        Commands::Describe { name } => introspect_command(
            cfg,
            paths,
            profile_override,
            target,
            &introspect::Verb::Describe { name: &name },
        ),
        Commands::Schemas => introspect_command(
            cfg,
            paths,
            profile_override,
            target,
            &introspect::Verb::Schemas,
        ),

        Commands::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "nsql", &mut std::io::stdout());
            Ok(())
        }

        Commands::Discover => {
            println!(
                "discovery is Phase 2/3: docker/podman container inspection first, \
                 then an opt-in, rate-limited local-/24 TCP scan (mDNS dropped). Not yet implemented."
            );
            Ok(())
        }
    }
}

fn introspect_command(
    cfg: &Config,
    paths: &Paths,
    profile_override: Option<&str>,
    target: Option<&str>,
    verb: &introspect::Verb,
) -> Result<()> {
    let profile = match target {
        Some(t) if is_db_url(t) => Profile {
            name: adhoc_name(t),
            url: t.to_string(),
            prod: false,
            readonly: false,
            no_history: false,
            ssh: None,
        },
        Some(t) => recents::resolve(paths, t)
            .map(|r| r.to_profile(cfg))
            .or_else(|| cfg.profiles.iter().find(|p| p.name == t).cloned())
            .with_context(|| format!("unknown connection `{t}`"))?,
        None => match profile_override {
            Some(_) => cfg.select(profile_override)?,
            None => match recents::most_recent(paths) {
                Some(r) => r.to_profile(cfg),
                None => cfg.select(None)?,
            },
        },
    };
    let q = introspect::query(&profile, verb)?;
    let out = db::run_all(&profile, &q, &db::RunOpts::new(false))?;
    let is_tty = std::io::stdout().is_terminal();
    let opts = render::Options {
        format: render::Format::Auto,
        is_tty,
        echo: None,
        elapsed: None,
        null_glyph: "(null)".to_string(),
    };
    render::print_all(&out.results, &opts)
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
        let mut a = argv(&["staging"]);
        assert_eq!(extract_target(&mut a).as_deref(), Some("staging"));
        let mut b = argv(&["2"]);
        assert_eq!(extract_target(&mut b).as_deref(), Some("2"));
    }

    #[test]
    fn does_not_steal_connect_url_or_subcommands() {
        let mut a = argv(&["connect", "prod", "--url", "postgres://u@h/db"]);
        assert_eq!(extract_target(&mut a), None);
        let mut b = argv(&["profiles"]);
        assert_eq!(extract_target(&mut b), None);
        let mut c = argv(&["tables", "--schema", "public"]);
        assert_eq!(extract_target(&mut c), None);
    }

    #[test]
    fn target_after_value_flag() {
        let mut a = argv(&["-p", "x", "sqlite:///t.db"]);
        assert_eq!(extract_target(&mut a).as_deref(), Some("sqlite:///t.db"));
        let mut b = argv(&["-P", "id=1", "-F", "top", "prod_db"]);
        assert_eq!(extract_target(&mut b).as_deref(), Some("prod_db"));
    }

    #[test]
    fn adhoc_name_is_dbname() {
        assert_eq!(
            adhoc_name("postgres://u:p@h:5433/pyparser_llunde"),
            "pyparser_llunde"
        );
        assert_eq!(adhoc_name("postgres://u@h/db?sslmode=require"), "db");
    }

    #[test]
    fn bare_table_names() {
        assert!(is_bare_table("users"));
        assert!(is_bare_table("public.users"));
        assert!(is_bare_table("_hidden"));
        assert!(!is_bare_table("1users"));
        assert!(!is_bare_table("a.b.c"));
        assert!(!is_bare_table("users;"));
        assert!(!is_bare_table("users."));
        assert!(!is_bare_table(""));
    }
}

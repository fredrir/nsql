//! Command-line surface. nsql is a *verb*, not an app you enter: the default
//! invocation opens your editor, runs the result, prints to normal scrollback,
//! and exits. Everything else is a flag or a small subcommand.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "nsql",
    version,
    about = "Run SQL from your terminal, composed in your real neovim — without taking over the screen.",
    long_about = "nsql opens your real neovim on a scratch .sql file, runs the query on save+quit, \
and prints the result into your normal terminal scrollback (never the alternate screen). \
Use `,,` to run and `,q` to cancel inside the editor (or :wq / :cq). \
Bare `nsql` opens the editor; `-e`/`-f`/`-F`/stdin skip it entirely."
)]
pub struct Cli {
    /// Execute SQL and exit (skips the editor)
    #[arg(short = 'e', long = "execute", value_name = "SQL")]
    pub execute: Option<String>,

    /// Run SQL from a file (skips the editor)
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    pub file: Option<String>,

    /// Run a saved favorite by name (skips the editor)
    #[arg(short = 'F', long = "favorite", value_name = "NAME")]
    pub favorite: Option<String>,

    /// Select connection profile (you can also pass a leading @name)
    #[arg(short = 'p', long = "profile", value_name = "NAME")]
    pub profile: Option<String>,

    /// Force the neovim compose loop even when reading from a pipe
    #[arg(long = "edit")]
    pub edit: bool,

    /// Expanded / vertical output (one field per line) — good for wide rows
    #[arg(short = 'x', long = "expanded")]
    pub expanded: bool,

    /// Output format (default: table on a tty, tsv when piped)
    #[arg(long = "format", value_enum)]
    pub format: Option<FormatArg>,

    /// Shortcut for --format json
    #[arg(long = "json")]
    pub json: bool,

    /// Do not cap the number of rows rendered
    #[arg(long = "all")]
    pub all: bool,

    /// Skip the confirmation for destructive statements on prod-tagged profiles
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum FormatArg {
    Table,
    Tsv,
    Csv,
    Json,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// List configured connection profiles
    Profiles,
    /// Add/update a profile and optionally store its password in the OS keyring
    Connect {
        name: String,
        /// Connection URL, e.g. sqlite:///path/db.sqlite or postgres://user@host/db
        #[arg(long)]
        url: Option<String>,
        /// Prompt for a password (hidden) and store it in the OS keyring
        #[arg(long = "set-password")]
        set_password: bool,
        /// Tag this profile as production (destructive statements need confirmation)
        #[arg(long)]
        prod: bool,
        /// Mark this profile read-only (refuse non-SELECT statements)
        #[arg(long)]
        readonly: bool,
    },
    /// Save the active profile's current scratch buffer as a named favorite (.sql)
    Save { name: String },
    /// List saved favorites
    Favorites,
    /// Show recent query history
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Discover local databases (docker/podman) — Phase 2 (stub)
    Discover,
}

# nsql

Run SQL from your terminal, composed in your **real neovim**, **without taking
over the screen**. Results print into your normal terminal scrollback — every
query lands in your shell history, interleaved with everything else, exactly
like `git commit`.

See **[DESIGN.md](./DESIGN.md)** for the full architecture, rationale, and the
answer to "can nvim run under the hood?" (yes — and the trick is *not* embedding
a TUI).

> Status: **Phase-1 + Postgres.** The primary goal (neovim edit → run → print
> with scrollback preserved) works end-to-end. **SQLite** (bundled) and
> **Postgres** (sync `postgres` crate) both execute live; MySQL is stubbed
> behind the same `Backend` extension point (`run` in `src/db.rs` →
> `src/backends/`).

## Quick start

```sh
cargo build
cargo test                       # includes the "never emit alt-screen" invariant test

# one-shot (no editor; great for scripts and pipes)
./target/debug/nsql -e "select 1 + 1 as two"
echo "select 'hi' as greeting" | ./target/debug/nsql
./target/debug/nsql --json -e "select 1 as a"   | jq

# the main event: compose in your real neovim, run on save+quit
./target/debug/nsql                # opens nvim on a scratch .sql file
#   ,,  -> run      ,q -> cancel       (or :wq to run, :cq to cancel)
```

On first run it bootstraps a `local` SQLite profile (a `dev.db` under your data
dir) so everything works immediately.

## How the editor loop works

`nsql` is the top-level process; your nvim is a **transient blocking child**:

1. `nsql` writes a scratch `.sql` temp file (`O_EXCL`, `0600`) and spawns
   `nvim -i NONE <file> -c 'setfiletype sql' -c 'luafile inject.lua'`.
2. nvim takes the alternate screen **only while you edit**; your `.sql` filetype
   triggers your own treesitter/LSP.
3. You hit `,,` (write + quit, exit 0 → run) or `,q` (`:cquit`, exit ≠0 →
   cancel). Run/cancel is decided by the **exit code**, so it works even in plain
   `vi`. `:q!` discards without running.
4. nvim exits → normal screen + scrollback restored byte-for-byte → `nsql` runs
   the query and prints the table to plain stdout (permanent scrollback).

`nsql` itself never emits the alternate-screen escape. There's a test that fails
if it ever does.

### Zero-flash mode (`--embed`, opt-in)

Mode 1 above flashes the alternate screen *during* the edit (then restores it).
If you want **zero** flash, build with the `embed-editor` feature and use
`--embed`:

```sh
cargo build --release --features embed-editor
nsql --embed        # nvim runs headless over RPC; its grid renders in an
                    # inline viewport — the alternate screen is NEVER entered
```

`nsql --embed` spawns `nvim --embed` (a headless editing engine that draws
nothing to the terminal), drives it over msgpack-RPC with your real config /
treesitter / LSP, and renders its `ext_linegrid` redraw stream into a ratatui
**inline** viewport. Same `,,`/`:wq` run contract; results still print to normal
scrollback. The async machinery (tokio/ratatui/nvim-rs) is contained entirely in
`src/embed.rs` behind the feature flag — the default build stays sync and lean
(+~0.7 MB with the feature).

**M1 status / limitations:** functional and verified end-to-end (no smcup, real
nvim, type → `:wq` → result in scrollback). The renderer is currently
**monochrome** — syntax-highlight colors (`hl_attr_define`/`default_colors_set`),
a popup-menu/cmdline overlay, dynamic resize and full input fidelity are **M2/M3**.
Mode 1 remains the default.

## Commands

| Command | What |
|---|---|
| `nsql` | open the editor, run on save, print |
| `nsql --edit` | force the editor even when piping |
| `nsql --embed` | zero-flash inline editor — nvim never touches the alt screen (needs `--features embed-editor`) |
| `nsql -e "<sql>"` / `-f file.sql` / `-F <favorite>` | run without the editor |
| `nsql @prod ...` / `-p prod` | pick a connection profile |
| `nsql -x` / `--json` / `--format csv\|tsv\|table` | output modes |
| `nsql --all` | don't cap rows (default cap: 1000) |
| `nsql -y` | skip the prod-destructive confirmation |
| `nsql connect <name> --url ... [--set-password] [--prod] [--readonly]` | add/update a profile |
| `nsql profiles` | list profiles |
| `nsql save <name>` / `nsql favorites` | favorites (plain `.sql` files) |
| `nsql history [--limit N]` | recent queries |
| `nsql discover` | local DB discovery (Phase 2 stub) |

## Safety (a fast SQL runner is a foot-gun by default)

- `--readonly` profiles refuse non-SELECT statements.
- `--prod` profiles require typing `yes` before a destructive statement
  (`DELETE`/`DROP`/`UPDATE`/…); non-interactively they need `--yes`.
- Passwords go to the **OS keyring** (`nsql connect --set-password`), never the
  config file. Connection URLs are **redacted** (`user:***@host`) in every
  message so a password can't leak into your scrollback.
- Result bytes are **sanitized** — a `bytea`/text cell containing escape codes
  can't break your terminal or defeat the no-alt-screen guarantee.
- Temp/scratch/history files are created `0600` with `O_EXCL`.

## Config

`~/.config/nsql/config.toml`:

```toml
default = "local"

[[profile]]
name = "local"
url = "sqlite:///home/you/.local/share/nsql/dev.db"

[[profile]]
name = "prod"
url = "postgres://app@db.internal/app"   # backend stubbed in Phase 1
prod = true
readonly = true
```

- Favorites: `~/.local/share/nsql/favorites/*.sql`
- History: `~/.local/share/nsql/history.sqlite` (`0600`)
- Scratch (per profile): `~/.local/share/nsql/state/scratch-<profile>.sql`

## Build notes

- Single sync binary. SQLite is live via `rusqlite` (bundled — no system lib).
- `cargo build --no-default-features` drops OS-keyring support (for environments
  without a Secret Service / for a leaner build).
- Phase-2 multi-engine (`sqlx`/`tokio-postgres`), warm-nvim `--listen` reuse,
  `--embed` zero-flash mode, visual-mode run-selection, and docker discovery are
  laid out in DESIGN.md §12.

## Backends

- **SQLite** — bundled (no system lib), full type fidelity.
- **Postgres** — via the sync `postgres` crate using the *simple-query protocol*,
  so values render exactly like `psql` and NULL stays distinct from empty.
  Password resolution when the URL omits it: `PGPASSWORD` env → OS keyring.
  Caveat: simple-query returns every value as **text**, so `--json` emits numbers
  as strings (`"42"`); typed/binary mode and TLS/SSL are Phase 3.
- **MySQL** — stubbed (next behind the dispatch in `src/db.rs`).

## Known limitations

- Statement splitting is naive (single row-returning statement, or a batch of
  non-row statements). Proper splitting + visual-mode run-selection is Phase 2.
- A zero-row Postgres SELECT shows `(0 rows)` without column headers (the simple
  protocol gives no column names when there are no rows).
- No interactive transactions/session state across invocations (verb model).
- Postgres requires no TLS yet (`NoTls`); managed cloud DBs needing SSL are Phase 3.
- Unix-only (no Windows path/keyring/editor handling yet).

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

# connect by URL once — then bare `nsql` resumes it next time (no URL in hand)
./target/debug/nsql postgres://user:pass@localhost:5432/mydb
./target/debug/nsql                # resumes the last connection
./target/debug/nsql "sqlite:///tmp/scratch.db" -e "select 1"

# the main event: a persistent inline session in your real neovim
./target/debug/nsql                # opens nvim; the session stays open
./target/debug/nsql --safe         # read-only session: refuses writes + a green SAFE badge
#   :w   -> run the statement under the cursor (write = execute; :wq runs + quits)
#   q    -> toggle into the results window (hjkl to move, y to copy clean values)
#   ,h   -> keys menu       ,i -> connection info       <C-x><C-o> -> completion
#   ,a   -> run uncapped    ,R -> force-run on prod   ,j/,c -> copy JSON/CSV
#   :q :wq :q! ZZ -> quit, the native way (buffer saved for next time)
```

On first run it bootstraps a `local` SQLite profile (a `dev.db` under your data
dir) so everything works immediately.

## How the editor loop works

> The **default** editor is the zero-flash inline editor (next section). The
> transient-child model below is what `--classic` (and any non-tty / no-feature
> build) uses; it's the same `,,`/`:wq` contract, so read it first — the
> zero-flash mode just removes the brief alt-screen flash in step 2.

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

### Zero-flash session — the default

Mode 1 (above) flashes the alternate screen *during* the edit, then restores it.
The **default** is a persistent zero-flash session that never enters the alternate
screen at all. It spawns `nvim --embed` (a headless editing engine that draws
nothing to the terminal), drives it over msgpack-RPC with your real config /
treesitter / LSP, and renders its `ext_linegrid` redraw stream — **in color** —
into a ratatui **inline** region. nvim itself owns a horizontal split: an editor on
top and a **results buffer** below, and we render nvim's whole grid.

Because the results are a **real nvim buffer**, you get all of nvim for free:

- **Type-aware colours** — integers, dates, booleans, strings and NULL are each
  painted by *value* (so Postgres text columns colour correctly too), via per-cell
  extmarks that resolve to your colorscheme.
- **Native navigation** — `q` (or `<C-w>j`) toggles into the results window; move with
  `hjkl`, visual-select, search — it's just a buffer. `q` / `<Esc>` toggle back.
- **Clean copy** — the table is borderless/aligned, so a yank copies the **values**,
  not box-drawing chars. Any yank there also mirrors to your system clipboard via
  OSC 52 (works over SSH, no `clipboard` setting needed).

The editor uses your **terminal's own background** (only distinct highlights, like a
selection, paint over it), so it blends in. There are at most **two bars** and no bar
above the editor. The **main header** — coloured badges: the database name, **SAFE**
(green, `--safe`) and **PROD** (red), with `,h help · ,i info` — sits just below the
editor when there's no output, and **moves to the bottom** once a table shows (the
slot above the rows then becomes the sticky **column header**). The row count is the
last row of the table itself — `1000+ rows` (coloured, so it never reads as data) when
capped, else `N rows`. Keys live in the **`,h`** menu, connection details in **`,i`**.
The editor and results panes share one height cap (`pane_height` in config, default 12).
The temp-file path and "written" noise are hidden.

**Native-first keys** — plain run / copy / quit are the vim verbs you already use;
custom `,`-keys are reserved for *features* (run-variants, exports):

- **`:w` runs** the statement under the cursor (write = execute); `:wq` runs + quits.
  The result lands in the bottom window — your scrollback above is never touched. A
  slow query runs in the background with a live `running… Ns` spinner (never freezes).
- **`:q` / `:wq` / `:q!` / `ZZ` quit** the whole session, the native way (your buffer
  is saved for next time). **`q`** toggles between the editor and the results window.
- **`,a`** runs uncapped; **`,R`** force-runs on a prod-tagged profile (otherwise
  destructive statements are refused in-session). **`,j`** / **`,c`** copy the last
  result as JSON / CSV (OSC 52). Copy a value with a native yank in the results window.
- **Schema-aware completion + highlighting** — nsql introspects the connected DB in
  the background and completes **live** table/column names (tables after `FROM`/`JOIN`,
  a table's columns after `tbl.`, both otherwise — so `select name from cat` completes
  `cat` and `name`). It wires itself into your completion engine automatically: it
  registers a **blink.cmp** source (auto-pops in blink's UI), and exposes the same via
  `omnifunc` — `<C-x><C-o>`, or nvim-cmp's `omni` source. `:NsqlSchema` reports what
  loaded. Recognised names are also
  **coloured as you type** — precise **treesitter** highlighting when the `sql` parser
  is installed (`:TSInstall sql`; skips strings/comments), else whole-word `matchadd`
  (`NsqlSchemaTable`→`Type`, `NsqlSchemaColumn`→`Identifier`, both overridable).
- **On quit, the last result is left in your scrollback** (the query + table, bounded
  to ~a screenful so your prior work stays visible above it).
- Errors render in the results buffer; the session keeps going.

**Portable everywhere.** No feature is locked behind your nvim config: each has a
config-independent baseline (completion via omnifunc, highlighting via `matchadd`,
badges drawn by nsql in explicit colours, the `--safe` guard in Rust), and plugins
(blink, treesitter) are additive enhancements. Over **SSH** nsql defaults to its own
bundled minimal nvim config (`--clean`) so a `curl|sh` / `yay` / `apt` install on a
bare server behaves identically to your local one (`--no-clean` opts back out).

```sh
nsql            # zero-flash persistent session (default)
nsql --clean    # use nsql's bundled minimal nvim config (auto over SSH)
nsql --classic  # the transient-child editor (Mode 1) instead
```

The async machinery (tokio/ratatui/nvim-rs) lives entirely in `src/embed.rs`
behind the `embed-editor` feature (on by default). For a leaner, fully-sync build
without it, use `cargo build --no-default-features` (then `nsql` uses Mode 1).

The scratch buffer opens **fully clean** — the active connection lives in nvim's
statusline, not in the buffer, so it is never saved or run.

**Status:** verified end-to-end against real nvim (no smcup; `:w` runs → result in a
real nvim buffer with type colours; `q` toggles in, a native yank copies clean values
via OSC 52; `:q`/`:wq` quit; scrollback above untouched). Editor features done: color
highlights, bracketed paste, width-resize, cursor-shape, clean buffer, the
type-coloured navigable results buffer, sticky-header bars, and JSON/CSV export.
Remaining: mouse (`nvim_input_mouse`), broader special keys, and the daily-driver
ergonomics in ERGONOMICS.md (the 2-flag CLI, query tabs, schema completion). Falls
back to Mode 1 automatically when there's no terminal (e.g. `--edit` while piping).

## Commands

| Command | What |
|---|---|
| `nsql` | open the zero-flash persistent session (`:w` run · `q` into results · `,j` json · `:wq` quit) |
| `nsql --safe` | read-only session (refuses writes, green SAFE badge) |
| `nsql --clean` / `--no-clean` | use nsql's bundled minimal nvim config (auto over SSH) / opt out |
| `nsql --classic` | use the classic transient-child editor (Mode 1) instead |
| `nsql --edit` | force the editor even when piping |
| `nsql -e "<sql>"` / `-f file.sql` / `-F <favorite>` | run without the editor |
| `nsql` | resume the **last connection** you used (no URL needed) |
| `nsql postgres://…` | connect to a URL (remembered for next time) |
| `nsql <label>` / `nsql <n>` | reconnect to a recent connection by name or index |
| `nsql @prod ...` / `-p prod` | pick a saved connection profile |
| `nsql -x` / `--json` / `--format csv\|tsv\|table` | output modes |
| `nsql --all` | don't cap rows (default cap: 1000) |
| `nsql -y` | skip the prod-destructive confirmation |
| `nsql connect <name> --url ... [--set-password] [--prod] [--readonly]` | add/update a profile |
| `nsql profiles` | list profiles |
| `nsql save <name>` / `nsql favorites` | favorites (plain `.sql` files) |
| `nsql history [--limit N]` | recent queries |
| `nsql discover` | local DB discovery (Phase 2 stub) |

## Safety (a fast SQL runner is a foot-gun by default)

- **`--safe`** makes the whole session **read-only** — anything but `SELECT`/`EXPLAIN`/
  `WITH`/`SHOW`/… is refused, enforced in nsql (no editor/config can bypass it), with a
  green **SAFE** badge as the reminder. Made for SSH-ing into a server you want to be
  *sure* you can only read from.
- `--readonly` profiles refuse non-SELECT statements (same guard, per-profile).
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

- SQLite is live via `rusqlite` (bundled — no system lib); Postgres via the sync
  `postgres` crate. The DB layer is fully sync.
- The **default** build includes the `embed-editor` feature (the zero-flash inline
  editor → +tokio/ratatui/nvim-rs, ~+0.7 MB). The async lives only inside
  `src/embed.rs` in a runtime scoped to the edit; the rest of nsql stays sync.
- `cargo build --no-default-features` drops the embed editor **and** OS-keyring
  support for a lean, fully-sync binary (`nsql` then uses the Mode-1 editor).
- Remaining roadmap (Postgres TLS/SSH, cancellation, introspection, parameterized
  favorites, embed M3) is laid out in PHASE3.md and DESIGN.md §12.

## Backends

- **SQLite** — bundled (no system lib), full type fidelity.
- **Postgres** — via the sync `postgres` crate using the *simple-query protocol*,
  so values render exactly like `psql` and NULL stays distinct from empty.
  Password resolution when the URL omits it: `PGPASSWORD` env → `~/.pgpass` →
  OS keyring (keyed on `user@host:port/db`). Remembered connections store the URL
  **without** the password.
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

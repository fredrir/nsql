# nsql — design & architecture

> A terminal-first SQL runner that uses **your real neovim** to compose queries
> but **never lives in the alternate screen**, so your terminal scrollback/history
> stays intact. Results print into your normal terminal flow, interleaved with the
> rest of your shell history — exactly like `git commit`.

---

## 0. The headline question: can nvim run "under the hood"?

**Yes — and the right way to do it is the *opposite* of "embedding" a TUI.**

The reason every neovim SQL plugin you've tried (dadbod-ui, etc.) eats your
scrollback is the terminal **alternate screen buffer**. Any full-screen TUI emits
`smcup` (`ESC[?1049h`) on launch, which switches the terminal to a *separate*
screen that has no scrollback; on exit it emits `rmcup` (`ESC[?1049l`) and the
original screen is restored. While you're "inside" the tool, your history is
hidden because it's parked on the other buffer.

Crucial insight (verified): **the alt-screen takeover is a property of neovim's
TUI *client*, not of neovim itself.** `nvim` is already a client/server program —
the UI client spawns an `nvim --embed` server child. So there are three ways to
drive nvim, with very different scrollback behavior:

| Mode | What it is | Alt-screen? | Effort |
|------|-----------|-------------|--------|
| **1. Spawn child on a temp file** (the `git commit` / `psql \e` / `vipe` model) | `nsql` writes a `.sql` temp file, runs `nvim file.sql` as a blocking child, reads it back on exit | Yes, but **transient** — one `smcup` on entry, one `rmcup` on exit. Scrollback frozen underneath and fully restored on exit. | **Low** ✅ |
| **2. `nvim --embed --headless` + RPC** | nvim runs as a pure headless editing engine over msgpack-RPC; draws *nothing*; you render the editor yourself inline | **Never** — but only if you do NOT call `nvim_ui_attach` (attaching makes you paint the full grid → back to a TUI) | **High** (novel work) |
| **3. `nvim --listen <sock>` server + remote** | a long-lived headless nvim you drive over a socket | Engine writes nothing; you still need a visible surface (mode 1 or 2) | Medium |

**Decision:** ship **Mode 1** as the default (proven, low-risk, gives you 100% of
your real nvim config/treesitter/LSP for free). The dadbod-ui pain is the
*persistent* takeover where the whole session lives in the alt screen and never
returns. Mode 1 inverts that: `nsql` owns the terminal, nvim is a transient guest,
and **all results land in normal scrollback**. Mode 2 (true zero-flash) is a
deferred Phase-2 option for the purist who objects to even the brief editor flash.

> The honest caveat: Mode 1 *does* flash the alt screen **during the edit itself**.
> If your complaint is "never the alt screen even for a second," only Mode 2
> satisfies that, and it's expensive (you re-implement the visible editor). The bet
> is that what you actually hate is the *persistent* takeover — which Mode 1 kills.

---

## 1. Core philosophy: nsql is a verb, not an app you enter

Like `git commit`, not like `htop`. `nsql` is always the top-level process; it
renders only to the **normal** screen via plain stdout and **never emits `smcup`**.
This single inversion is the whole design.

Consequences:
- Every query + result becomes permanent, greppable shell scrollback.
- It composes with Unix pipes: `nsql -e '...' | jq`, `echo '...' | nsql`, `> out.csv`.
- Favorites are just `.sql` files. Profiles are just TOML. History is one SQLite file.
- No lock-in, no proprietary store, no daemon to babysit.

**CI litmus test (enforced invariant):** run nsql, scroll up afterward, confirm
pre-session history *and* the queries/results it printed all survive. If anything
emitted `smcup` for nsql's own output, the test fails.

---

## 2. The core edit → run → print loop (exact terminal behavior)

```
nsql @prod                # 0. top-level process. NORMAL screen. nothing emitted yet.
  │
  ├─ 1. write durable scratch  $XDG_STATE_HOME/nsql/scratch-prod.sql (0600)
  │     copy → mkstemp temp file (O_EXCL, 0600, random name — not pid-based)
  │
  ├─ 2. spawn editor as a BLOCKING child, inheriting the real tty:
  │        nvim -i NONE <tmp.sql> -c 'setfiletype sql' -c 'luafile inject.lua'
  │     → nvim emits smcup ONCE → ALT SCREEN (transient). Your real config,
  │       treesitter, LSP all load because the file is *.sql (ftdetect fires).
  │       Normal-screen scrollback frozen + preserved underneath.
  │
  ├─ 3. you run (`,,` → :w + run-signal) or cancel (`,q` → :cq, nonzero exit)
  │     → nvim emits rmcup ONCE → back to NORMAL screen, scrollback restored
  │       byte-for-byte. The edit left zero trace in scrollback.
  │
  ├─ 4. branch on exit code: cancel → keep scratch, run nothing.
  │     run → read tmp back, update durable scratch, append to history.sqlite
  │
  ├─ 5. execute via sqlx; echo query as `-- comment`; render result table to
  │     plain STDOUT (comfy-table). NEVER EnterAlternateScreen. → PERMANENT scrollback.
  │
  ├─ 6. page ONLY if taller than terminal AND a usable pager exists (see §6).
  │
  └─ 7. exit (verb model). `--repeat` re-opens the editor preloaded with the
        last query for fast iteration.

ONE-SHOT BYPASS (never touches the editor or alt screen at all):
  nsql -e 'SELECT ...'        nsql -f q.sql        echo 'SELECT ...' | nsql
  → output auto-switches to TSV/--json/--csv when stdout is not a tty.
```

---

## 3. Components

| Component | Responsibility |
|-----------|----------------|
| **CLI dispatch** (`clap`) | Route: interactive edit-run-print (default), `-e <sql>`, `-f <file>`, stdin (auto when `!isatty`), subcommands (`connect`, `profiles`, `save`, `favorites`, `discover`, `history`). Resolve active profile from `@name` / `--profile` / `$NSQL_PROFILE` / config default. tty-vs-pipe decides human-table vs machine output. |
| **EditorSession** | The heart. Write durable scratch + `mkstemp` temp `.sql`; resolve editor; spawn blocking child with inherited tty; inject keymaps via `-c` (post-config so they win); interpret exit code; read buffer back; never lose text. Graceful fallback for non-nvim editors. |
| **inject.lua** | Tiny buffer-local Lua: fixed run/cancel maps (see §5), a help line as SQL comments, optional `sqls`/dadbod-completion configured with the active connection for schema-aware completion while composing. Buffer-local only → never disturbs your global config. |
| **ConnectionManager** | `profiles.toml` with `[[profile]]` (name, url-without-password, `${env:VAR}` interpolation, optional ssh tunnel). Resolve password from keyring at connect time. `connect <name>` / in-session switch. Honors psql-style env vars. |
| **QueryRunner** (`sqlx` Any) | Runtime query API + `Any` driver, dispatched by URL scheme (`postgres://`/`mysql://`/`sqlite://`). NOT compile-time `query!` macros (impossible for ad-hoc SQL). Statement splitter (see §4 gaps). Streams/maps rows + errors. Pluggable trait so mongo/redis could be added later. |
| **ResultRenderer** (`comfy-table`) | Aligned table (auto width-fit + wrap), expanded/vertical `\x` for wide rows, CSV, JSON, TSV-when-piped. NULL rendered distinctly. **Sanitizes cell bytes** (strip/escape ESC/CR/NUL) so binary/`bytea` data can't emit control sequences and break the no-altscreen guarantee. |
| **Pager** | Page only if output overflows AND a usable pager exists. **Do not assume `less`.** (see §6) |
| **FavoritesStore** | Named `.sql` files under `$XDG_CONFIG_HOME/nsql/favorites/`. `save <name>` / `@name` / fuzzy picker (`nucleo`). Greppable, git-friendly. |
| **HistoryStore** | Every run appended to `history.sqlite` (0600, timestamp + profile + SQL). Powers `--last`, `history`, Ctrl-R search, crash recovery. Per-profile opt-out for sensitive connections. |
| **Keyring** (`keyring` v4) | DB passwords in the OS keychain (Secret Service/libsecret, macOS Keychain, Windows Cred Mgr), keyed by `nsql` + profile. Plaintext never in TOML. Graceful fallback (env / `.pgpass` / prompt) when no keychain (headless/SSH). |
| **Discovery** | Opt-in, secondary. **Primary:** docker/podman inspection. **Secondary:** opt-in, rate-limited, `/24`-only TCP scan with a reconnaissance warning. mDNS dropped. (see §7) |
| **EmbeddedNvim — Phase 2** | `nvim --embed --headless` over `nvim-rs`, rendered inline (no `nvim_ui_attach`). The zero-flash purist mode. Deferred — novel, expensive. |

---

## 4. Feature → mechanism map

| Requested feature | How |
|---|---|
| **Edit in real neovim w/o screen takeover** (primary) | Mode 1: `nsql` top-level, nvim transient blocking child. Transient alt-screen during edit only; scrollback preserved before/during/after. Your full config/treesitter/LSP via `.sql` ftdetect. Phase-2 `--embed` for zero-flash. |
| **Results in scrollback** (the real win) | nsql never emits `smcup` for its own output; query + table go to plain stdout = permanent interleaved scrollback. |
| **Save/favorite queries** | `.sql` files + fuzzy picker; separate executed-query history. |
| **Connect to multiple databases** | URL-based named profiles in TOML; `sqlx` Any dispatches by scheme; password from keyring; `DATABASE_URL`/PG* env for zero-config. |
| **Change DB from the session** | It's a verb, so "switching" = picking a profile: `nsql @prod`, `nsql @staging`. In `--repeat` mode a `\c <name>` meta-command swaps the active connection (banner + per-profile scratch follow). |
| **Discover DBs on the network** | `nsql discover`: docker/podman first; opt-in `/24` TCP scan second; mDNS dropped. Found DBs become profile drafts. |
| **Store credentials securely** | `keyring` → OS keychain. Layered resolution: explicit URI > env/`DATABASE_URL` > `~/.pgpass` > keyring > external (`op://` 1Password, `pass`). Config holds only references. |
| **Unix composability** | `-e`, stdin auto-detect, `-f`, machine output when piped. |

---

## 5. ⚠️ Verified gotchas (these came from running commands on *your* machine)

1. **`--cmd 'set shada=NONE'` is WRONG** — it throws `E539: Illegal character` on
   every launch. Use **`-i NONE`** to disable shada. (Two of the three candidate
   designs shipped this bug; it was reproduced on nvim 0.12.2 here.)

2. **Do NOT bind `<CR>` to run.** Enter is reflexive cursor movement in normal mode;
   hijacking it means an accidental Enter executes a half-written query **against
   prod**. Use a deliberate, non-reflexive map (e.g. `,,` to run, `,q` to cancel)
   and **print the actual resolved key in the buffer header** — never show the
   abstract `<leader>`, because your default leader is `\` and is commonly remapped.

3. **`less` is not installed on your system** (you have `more`, `PAGER=more`).
   `more` doesn't accept `-RFX`, strips color, and on many builds *uses the alt
   screen* — which would silently break the whole no-altscreen guarantee. The pager
   layer must **detect** what's available and **fall back to direct printing**, not
   blindly invoke `less`/`more` with flags.

4. **`EDITOR`/`VISUAL`/`XDG_*` are all unset here.** Editor resolution must be
   `NSQL_EDITOR > VISUAL > EDITOR > nvim > vi` with an explicit "no usable editor"
   error path that still lets you run inline SQL. Use the `etcetera`/`directories`
   crate for XDG fallbacks (`~/.local/state`, `~/.config`, `~/.local/share`) — a
   naive `env::var("XDG_STATE_HOME")` will error.

5. **Statement splitting is a real correctness problem**, not an edge case. Naive
   split-on-`;` breaks on `';'` string literals, dollar-quoted bodies (`$$ ... ; ... $$`),
   and PL/pgSQL. Pick a real splitter (or run the whole buffer and let the driver
   handle it where possible). **Best ergonomic answer:** a visual-mode "run
   selection" map so you delimit the statement yourself — also the biggest
   iteration-speed win.

6. **Sanitize result bytes.** A `bytea`/blob/text cell can itself contain
   `ESC[?1049h` and break your terminal — the no-altscreen guarantee must hold
   against malicious/binary *data*, not just nsql's own code.

---

## 6. Pager policy (because `less` isn't guaranteed)

```
if output fits terminal height         → print directly (no pager)
elif $PAGER is less (present)           → less -RFX   (-X defeats alt screen, -F quit-if-fits, -R colors)
elif a known-safe pager is present      → use it with safe flags
else                                    → print directly + "N more rows, re-run with --all"
never                                   → invoke `more`/unknown pager that may enter the alt screen
```

---

## 7. Discovery — honest scope

- **Primary: docker/podman inspection.** List containers, match known DB images
  (postgres/mysql/mariadb/mongo/redis), read published host ports. Fast, quiet,
  reliable — matches how devs actually run local DBs. (Requires socket access;
  fail closed, never imply docker-group membership is required.)
- **Secondary: TCP scan, opt-in, off by default, `/24`-only, rate-limited, with a
  typed confirmation naming the exact subnet.** ⚠️ Active scanning of `5432/3306/…`
  trips corporate IDS/IPS and violates AWS/GCP acceptable-use (can get an account
  flagged). Handshake/banner probing sends partial protocol traffic to strangers'
  services. Strongly consider docker-only and dropping active scan entirely.
- **mDNS: dropped.** Postgres advertising is off by default; MySQL never supported
  it. Near-zero real-world value.

---

## 8. Safety (a fast SQL runner is a foot-gun by default)

The single highest-probability real-world harm isn't SQL injection — it's an
accidental `DELETE`/`DROP`/`UPDATE` without `WHERE` on a prod-tagged DB.

- **`readonly = true`** per-connection flag (Postgres `default_transaction_read_only`,
  or refuse non-SELECT).
- **`prod` tag**: color the banner/prompt **red**, and require a typed confirmation
  for destructive statements (auto-wrap in `BEGIN; … ;` showing affected-row count,
  then prompt commit/rollback).
- **DSN redaction everywhere**: the assembled connection string (with password)
  must never appear in `argv`/`/proc/<pid>/cmdline`, logs, or **error messages**
  (an error that prints the DSN leaks the password into your permanent scrollback —
  ironic given the whole point).
- **Secret-bearing files**: create temp/scratch/history with `O_EXCL` + `0600`
  (not write-then-chmod → TOCTOU); ensure SQLite WAL/journal sidecars also get
  `0600`; offer a "sensitive connection" mode that does **not** persist results to
  scrollback/history (DB errors and result rows routinely echo real data).

---

## 9. Other gaps to design for (from the completeness pass)

- **Transactions/session state**: a pure verb has no session, so `BEGIN; … COMMIT;`,
  temp tables, `SET search_path` can't span queries; pooled connections make
  cross-query `BEGIN/COMMIT` land on *different* backends. Decide: explicit
  multi-statement buffers, or a `--repeat`/session mode pinned to one connection.
- **Non-SELECT results**: show `INSERT 0 5` affected-row counts, `RETURNING`,
  multiple result sets, server `NOTICE`/`WARNING`.
- **Cancellation**: Ctrl-C must send a backend cancel, not just kill the process
  (leaves transactions open). Add `statement_timeout`.
- **Streaming vs auto-fit tension**: column auto-fit needs all rows; you can't both
  stream incrementally *and* auto-size columns. Pick: buffer with a row cap, or
  fixed-width streaming, or two-pass.
- **Row cap**: default `LIMIT`/`FETCH_COUNT` with "showing 100 of N, `--all`".
- **TLS/SSL + SSH tunnel/bastion**: most managed cloud DBs need `sslmode`/CA/client
  certs and jump-host access. Required for the multi-DB goal to be real.
- **Concurrency**: two terminals editing `scratch-<profile>.sql` clobber each other;
  add per-session isolation or locking.
- **Windows**: either commit (no default `vi`/`less`, no unix sockets on old
  Windows, different ANSI handling) or scope it out explicitly.

---

## 10. Ergonomics that make it feel fast

- **Trailing `;` = run, no `;` = keep editing** (psql `\e` convention) as the
  primary signal; exit-code run/cancel as the editor-agnostic fallback.
- **Visual-mode "run selection"** — run only the highlighted statement (solves
  splitting + fastest iteration).
- **`nsql @prod users`** shorthand: a bare table name → `SELECT * FROM users LIMIT 100`.
- **`nsql --last`** re-edits the previous query; persistent per-profile scratch means
  you always resume where you left off.
- **Ctrl-R** fuzzy history search (shell-style) in the inline path.
- **Auto-`\x`** (expanded/vertical) when a row is wider than the terminal.
- **Timing + row count after every query** (`5 rows in 12ms`) — cheap, feels fast.
- **Red banner for prod**, profile always visible.
- **Transparent warm nvim**: if a prior `--listen` socket exists and the nvim
  version matches, reuse it to kill per-edit cold-start; else spawn fresh. Speed of
  a daemon without making you manage one.

---

## 11. Stack

- **Language: Rust** (single static musl binary). Chosen for the cohesion of
  `nvim-rs` + `sqlx` + `keyring` + `comfy-table`, not a blanket perf claim. Go is a
  legitimate alternative (faster compiles, `modernc.org/sqlite` pure-Go, `neovim/go-client`).
- `clap` (CLI) · `std::process::Command` inherited stdio (the editor spawn — the
  whole primary mechanism) · `sqlx` runtime API + `Any` driver · `comfy-table`
  (**not** `prettytable-rs` — unmaintained + advisory) · `serde_json`/`csv` ·
  `keyring` v4 · `toml` · `rusqlite` (history) · `nucleo` (fuzzy) · `etcetera`/`directories` (XDG).
- **Phase 2 only:** `nvim-rs` (msgpack-RPC over `nvim --embed --headless`) — pin it
  (API unstable, **LGPL-3.0** — review, or hand-roll an `rmpv`/`rmp-serde` client).
- System: neovim (flags: `-i NONE`, `-c` post-config inject, `--clean` opt-in,
  `--embed --headless` for Phase 2); docker/podman CLI; a pager only if present.

---

## 12. Phased roadmap

**Phase 1 (MVP — nails the primary goal):** Rust verb + Mode-1 child-on-tempfile
(`-i NONE`, post-config inject, fixed run/cancel keys) + `sqlx` (Postgres + SQLite
first) + `comfy-table` + `keyring` + favorites(`.sql`) + profiles(TOML) +
history(SQLite) + the no-`smcup` CI litmus test. Pager detection, XDG fallbacks,
byte sanitization, DSN redaction, `O_EXCL` temp files, and the "no editor found"
path are **in scope for MVP** — they're correctness/safety, not polish.

**Phase 2:** ✅ Postgres backend (sync `postgres` crate, simple-query protocol) ·
✅ query timing · ✅ prod/readonly guardrails — then: warm-nvim `--listen` reuse ·
visual-mode run-selection · `--repeat` session mode with `\c`/`\x`/`\s` · auto-`\x` ·
Ctrl-R history · MySQL · TLS + SSH tunnel.

**Phase 3:** ✅ zero-flash `--embed` inline editor — M1 (nvim --embed over RPC →
ratatui inline viewport, no alt screen ever; behind the `embed-editor` feature) ·
then M2 color/overlays + M3 resize/input · docker discovery · schema introspection
(`\d`) · export/`\copy` · parameterized favorites · Postgres TLS/SSH (your daily
critical path). See PHASE3.md for the full brainstorm + value-per-effort ordering.

---

*Design synthesized from a multi-agent research + design panel (existing tools,
nvim RPC/altscreen internals, stack, scrollback mechanics → 3 competing
architectures → judge + completeness critic). Load-bearing nvim/terminal claims
were empirically verified on this machine (nvim 0.12.2).*

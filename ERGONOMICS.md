# nsql — daily-driver ergonomics plan

*Synthesized from a multi-agent design pass (4 researchers → 2 competing UX shapes →
judge + scope/security critic). Every load-bearing claim was verified against the
actual source (line numbers cited inline below).*

## Vision (the bright line)

nsql is for the **frictionless sidetrack** — a quick query mid-other-task, results in
your scrollback — and **deliberately not** a long-session SQL IDE (harlequin/DataGrip
win there). Every decision optimizes *zero-setup resume + fast inline iterate*, and we
hold hard lines against drifting into a query manager.

**Direction:** *Evolve-the-Verb* (judge 8.5) over *Persistent-Inline-Session* (7.5).
Change as little as possible; every feature is additive and **fails closed** to today's
behavior if nvim / introspection / a plugin / the DB is missing.

## The new daily flow

1. You type `nsql` (no URL, no flag). It resumes `recents[0]` (the DB you used last),
   writes the schema dict in the background, and opens your real neovim **inline** on
   your prior scratch — clean buffer, dim `nsql · myapp · ring 1/6` status line, no
   alt-screen flash, scrollback intact above.
2. You type `SELECT count(*) FROM ord` → `Ctrl-N` completes `orders` from the live schema.
3. You run (`,r`, or `:w`) the statement under your cursor → the result renders in a
   **results pane at the bottom of nsql's own region**. Your main-task scrollback **above
   stays put and fully visible** — running never scrolls it. (Decision: results go to a
   bottom pane, NOT into scrollback via `insert_before`, so you keep an eye on your main work.)
4. Wrong table — edit one word, run again → the results pane updates **in place**. Iterate
   without your main history moving at all.
5. `,y` copies the last result as TSV (OSC 52, works over SSH) so you capture the value.
   `,q` leaves; nsql's region disappears and your main scrollback is exactly as you left it.

## Progress

- ✅ **Step 0 — `render::format()`** extracted (pure `String`, no behavior change).
- ✅ **Step 1 — run-without-quit + bottom results pane + copy** (the headline). The
  embed session is now persistent: `,r` (or `:w`) runs the statement under the
  cursor via `rpcnotify('nsql_run')` → `db::run` → a **results pane at the bottom of
  nsql's own inline region**; the scrollback **above is never touched**. `,y` copies
  the last result as TSV via **OSC 52** (works over SSH). `,R` forces past the prod
  guard, `,a` runs uncapped; errors render in the pane and never end the session;
  `,,`/`,q` quit + persist the scratch. The RPC channel is handed to inject.lua as
  `g:nsql_chan` after attach (a startup `luafile` can't discover it). Verified by a
  deterministic `--clean` round-trip test **and** a real-config pty run (no smcup;
  result in pane; clipboard = `total\n60`).

- ✅ **Step 2 — zero-setup resume + credential hardening.** `recents.toml`
  (`state_dir`, **0600** in a **0700** dir, atomic `write_private` — no TOCTOU)
  records each interactively-chosen connection **with the password stripped**
  (`util::strip_url_password`, distinct from the display-only `redact_url`). Bare
  `nsql` resumes `recents[0]` (else the bootstrapped `local`); a positional can be
  a URL, a recents **label**, a recents **index**, or a saved profile;
  `no_history` profiles are never recorded; scripted `-e`/pipe one-shots don't pin.
  Passwords resolve at connect time via **PGPASSWORD → ~/.pgpass → OS keyring**,
  and the keyring is now **keyed on `user@host:port/db`** (fixes the cross-host
  collision leak — `connect --set-password` stores under the same identity).
  Verified: password never on disk; perms 600/700; resume + index/label targets
  work.
- ✅ **Adversarial security review** (13 findings, each independently verified) and
  fixes: (HIGH) `connect --url …:pw@…` no longer persists the password to
  config.toml — it's stripped to the keyring under the stable identity; (HIGH) a
  password containing `@` no longer leaks (`redact_url`/`strip_url_password` now use
  the *last* `@` in the authority); config.toml is written `0600` and all nsql dirs
  are `0700`; `persist_scratch` uses the TOCTOU-safe writer; the spawned editor no
  longer inherits `PGPASSWORD`; `~/.pgpass` rejects non-regular files.
  Accepted/inherent: `history.sqlite` stores raw SQL (may contain literal secrets)
  — kept `0600` in a `0700` dir, like a shell history.

Remaining: CLI collapse to `-e`/`-y` · history-ring tabs · schema completion ·
interactive "save password to keyring?" prompt on first connect.

## Stabilization pass ("never distract")

A reported crash triggered a robustness audit (4 lenses → each finding independently
verified → prioritized plan). Fixed this pass:

- ✅ **P0 — Postgres-in-session panic killed.** Running a Postgres query in the session
  used to panic ("Cannot start a runtime from within a runtime" — the sync `postgres`
  crate spins its own runtime inside ours). Queries now run on the **blocking pool**
  (`spawn_blocking`).
- ✅ **P0 — slow query no longer freezes the editor.** The query is offloaded; the event
  loop keeps pumping redraws/keys with a **live "running… Ns" spinner** (rendered at the
  top of the loop so nvim's redraws can't starve it). Verified: 337 bytes of redraws
  emitted *during* a 2s `pg_sleep` query (vs ~0 when it blocked). Quitting mid-query is
  instant (`shutdown_background`).
- ✅ **P0/P1 — time bounds** so nothing hangs unbounded: Postgres `connect_timeout(8s)` +
  `statement_timeout 30s`; SQLite `busy_timeout(5s)`.
- ✅ **P0 — terminal never left corrupted.** `TermGuard` restores raw mode / bracketed
  paste / cursor shape on Drop, including a panic unwind.
- ✅ **Redraw hardening** (a malformed redraw can't panic/freeze the loop): `grid_line`
  repeat clamped to remaining width; `grid_scroll` bounds-clamped + `unsigned_abs` (no
  i64::MIN overflow).
- ✅ **Memory bound** on in-session results (`,a`/--all) — rendered lines capped; the full
  result is still copyable with `,y`.
- ✅ `~/.pgpass` rejects symlinks.

Deferred (lower-ROL, mostly malformed-local-nvim hardening or P2 wording): per-query cancel
key (timeouts cover the unbounded case); SQLite silently running only the first of multiple
statements; friendlier config-parse error; panic-message legibility (a panic hook). The
grid OOB/overflow "P0s" the verifiers flagged were re-graded P3 by synthesis (in-bounds for
real nvim) and the clamps above cover them anyway.

## Unintrusive polish + testability

- ✅ **Background blends with the terminal.** The editor region no longer paints
  nvim's colorscheme background — the default bg is left transparent (terminal
  shows through); only highlights with a *distinct* bg (selection/search) paint.
- ✅ **Output persists in context on exit.** Results live in the transient bottom
  pane while iterating (don't disturb the scrollback), and on quit the **last
  result (query + table) is left in the real scrollback** (`insert_before`), so the
  answer sits in context with the user's main work — the tool's core purpose.
- ✅ **Testability:** the query→render core is now a pure `render_outcome()`
  (pane lines / TSV-for-copy / scrollback-persist block), unit-tested
  deterministically against in-memory SQLite — no nvim, no tty, no flaky pty.
  Background-blend and error-handling are unit-tested too. (Next testing
  investment: abstract the terminal + input behind traits so the full session loop
  gets a deterministic integration test; the pty drives stay as manual dev checks.)

## Unintrusive-UI cleanup

- ✅ Status moved into **nvim's native statusline** (the bar at the bottom of the
  editor window, which also serves as the **divider** above the results pane);
  shows the redacted connection, **prod in red**, and the key hints. Replaces the
  virtual-text line.
- ✅ Hidden noise: the temp-file path and `"… N lines written"` (`shortmess+=WF`),
  the `(no results yet)` placeholder, and the `capped at N rows` stderr message
  that was corrupting the inline view (folded into a compact row-count footer).
- ✅ Exit-persist **bounded** to ~a screenful so the user's prior work stays
  visible above the persisted result.

### ✅ Results in a real nvim buffer (the pivot — done)

The results now live in a **scratch nvim buffer** in a split below the editor
(`SETUP_RESULTS_LUA`); nsql renders nvim's whole grid (nvim owns the split + both
statuslines). This delivered:

- (7) **type-aware highlighting** — `format_for_buffer` classifies each cell by TYPE
  *and* value (so Postgres text columns colour correctly) into nvim hl groups
  (`Number` / `String` / `Boolean` / `Constant` for dates / `Comment` for NULL /
  `Title` for headers), applied as per-cell extmarks (`WRITE_RESULTS_LUA`).
- (8) **native navigation + clean copy** — `,o` (or `<C-w>j`) enters the results
  window; `hjkl` / visual-select / `q`-to-go-back all work because it's just a buffer.
  The table is **borderless/aligned**, so a yank copies values (verified: `42   widget`,
  no box chars). A `TextYankPost` autocmd mirrors any yank to the clipboard via OSC 52.

Verified end-to-end on real nvim (pty + headless): the split renders, values +
headers + footer show, the connection statusline divides, the extmark hl groups land,
and `yy` emits a clean OSC-52 payload. Bounded at 2000 on-screen rows (extmark cap;
the full result is still exportable / re-runnable).

### ✅ Native-first keymap + sticky-header bars (done)

"Plain run / copy / quit are the vim verbs you already use; custom `,`-keys are for
*features*." So:

- **Run = `:w`** (write = execute; `:wq` runs + quits). **Quit = native** `:q` / `:wq`
  / `:q!` / `ZZ` — the results split auto-closes when it's the last window (quickfix
  pattern), so quitting the editor exits nvim. **Copy = native yank** in the results
  window. Dropped `,r` / `,y` / `,,` / `,q` / `,o`.
- **`q` toggles** between the editor and the results window (both directions).
- Custom keys are now exports + run-variants: **`,j`** (JSON) / **`,c`** (CSV) copy the
  last result via OSC 52; **`,a`** (all rows) / **`,R`** (force on prod).
- **Sticky-header bars** (`WRITE_RESULTS_LUA` role-shift): with a result on screen the
  editor's statusline becomes the column **header**, pinned above the scrolling rows
  and aligned to the same columns; the connection moves to the bottom bar. With no
  result, the connection is the editor's bar (prod in red).

### ✅ Schema-aware completion + aligned sticky header

- **Completion** (`<C-x><C-o>`): nsql introspects the live DB (`introspect_schema`
  → `information_schema.columns` for Postgres, `sqlite_master` + `pragma_table_info`
  for SQLite) on the blocking pool at session start and hands the tables/columns map
  to a global the omnifunc reads (`SET_SCHEMA_LUA` → `_G.nsql_schema` → `NsqlOmni`).
  Context-aware: tables after `FROM`/`JOIN`/`INTO`/`UPDATE`, a table's columns after
  `tbl.`, both otherwise. Best-effort + background, so it never blocks or breaks the
  editor. Verified: `introspect_schema` unit test + a headless omnifunc probe
  (`from c`→tables, `cat.`→columns, base-filtered).
- **Per-identifier highlighting** (`SET_SCHEMA_LUA`): the same schema paints known
  table/column names in the editor via `matchadd` — a window-local layer that sits on
  top of syntax *and* treesitter, whole-word + case-insensitive, live as you type.
  `NsqlSchemaTable`→`Type`, `NsqlSchemaColumn`→`Identifier` (overridable). Verified
  headless (whole-word: `cat` yes / `category` no; case-insensitive; correct links).
  Caveat: matchadd is not context-aware, so a known name inside a string/comment also
  colours — acceptable for a scratch buffer.
- **Sticky-header alignment fix**: the header statusline had a leading space the data
  rows lacked — dropped it (`%<` truncation marker, no width), so the pinned header
  lines up column-for-column with the rows below.

### ✅ Completion that auto-pops (blink.cmp source) + treesitter highlighting

- **Root cause of "no completion":** omnifunc alone isn't consumed by nvim-cmp/blink;
  they need a registered source. (dadbod-completion is no help — it needs a dadbod
  `:DB` connection the nsql session doesn't set.) Fix: one shared `nsql_complete_words`
  context analyser feeds three consumers — a **blink.cmp source** (registered via
  `add_source_provider` + `add_filetype_provider_id('sql', …)`, deferred past blink's
  VimEnter load), the **omnifunc** (`<C-x><C-o>` / nvim-cmp `omni`), and a **vanilla
  auto-popup** (gated off when any engine is detected). Verified against the installed
  blink v1.10.2. `:NsqlSchema` reports tables/columns loaded.
- **Highlighting upgraded to treesitter** (`SET_SCHEMA_LUA`): walks the `sql` parse
  tree, colours only leaf identifier nodes that match the schema, **skipping
  string/comment subtrees**, kept fresh on edits (debounced 150ms). Falls back to
  `matchadd` when there's no `sql` parser (so install it with `:TSInstall sql` for the
  precise version). table-vs-column by membership (tables win ties); clause-precision
  is a future refinement.
- **Persist footer fixed** (`render_persist`): the old exit block rendered the full
  bordered result (~2000 lines for a 1000-row cap) then truncated by *lines*, so
  "1988 more rows" was actually lines. Now: query echo + first 10 rows borderless +
  ONE honest line (`-- first 1000 rows (capped) · ,a or nsql -e for all`).

### ✅ Portability principle + `--safe` + badge bar

**Principle (load-bearing): nsql's features must never be locked behind the user's
nvim config.** nsql loads the user's real config (familiar editing) and layers its
own features on top via `inject.lua` — but every feature has a **config-independent
baseline**, and plugin integrations are *additive enhancements* that activate only
when present:

| feature | baseline (bare nvim / SSH) | enhancement (if present) |
|---|---|---|
| completion | omnifunc + built-in auto-popup | blink.cmp source / cmp `omni` |
| schema highlight | `matchadd` | treesitter (`:TSInstall sql`) |
| status bar | nvim statusline + **explicit hex** badge colours | your colorscheme overrides |
| results | nvim scratch buffer (core) + OSC-52 yank | — |
| safety | **`--safe` guard enforced in Rust** | — |

So a `curl|sh` / `yay` / `apt` install on a bare server behaves identically — nothing
requires plugins, a colorscheme, or a parser.

- **`--safe`** sets the session read-only (`profile.readonly`), the existing guard
  refuses non-SELECT, enforced Rust-side. Visual reminder: a green **SAFE** badge.
- **Badge bar:** the editor statusline is now coloured **badges only** — db name
  (`NsqlDb`), `SAFE` (`NsqlSafe`), `PROD` (`NsqlProd`), explicit hex so they're
  identical everywhere. No `nsql ·`, no connection string, no key hints.
- **`,h` keys / `,i` connection** floats (plain nvim, work anywhere) replace the bar
  hints; the bar just points to them.
- **Bottom pane hidden until the first result** — the results split is created
  lazily (`nsql_ensure_rwin`, `laststatus=1`), so until you run something there's just
  the editor. The column header is pinned in the editor statusline (the divider above
  the rows), the row-count summary in the results statusline.

### ✅ nsql draws its own bar + portable-over-SSH (decided + done)

- **nsql draws the badge bar itself** (ratatui, `build_status_bar`, top row; nvim
  attaches at `view_h - 1`). Guaranteed visible — a statusline plugin can never hide
  the SAFE badge. Explicit hex colours. The editor statusline is freed for the sticky
  header.
- **Portable mode** (`--clean`, auto over SSH via `$SSH_TTY`/`$SSH_CONNECTION`,
  `--no-clean` opts out): spawn `nvim -u nsql_init.lua` — nsql's bundled minimal config
  (`assets/nsql_init.lua`) instead of the user's. Enables filetype/syntax for SQL,
  loads no plugins; inject.lua supplies every feature. So a bare `apt`/`yay` box
  behaves identically. Verified: omnifunc + SQL syntax + `,h` all work config-less.
- **Removed the built-in auto-popup.** Feeding `<C-x><C-o>` on every keystroke fought
  whole-word typing and *doubled characters* (`inv`→`invv`) — a real bug caught in
  pty. Completion is now on-demand (`<C-x><C-o>`) or through the engine's own UI (the
  blink source); typing is never disturbed.

### ✅ Bug: inline ad-hoc passwords now survive resume

`nsql postgres://user:pw@host/db` used to connect fine but break on bare-`nsql`
resume — recents stores the URL *without* the password (no plaintext on disk) and the
password was never saved. Now an inline password is migrated into the OS keyring
(keyed on `user@host:port/db`, like `connect`), so resume's `resolve_password` finds
it. The session's own profile keeps the full URL so the current connection still works.

## Build order (friction-removed-per-effort)

- **Step 0 — `render::format()` extraction** *(no behavior change; enables everything)*.
  Factor everything in `render::print` above the final `pager::emit` (render.rs:140) into
  `pub fn format(result, opts) -> String`; `print` becomes `pager::emit(&format(...), ..)`.
  The session uses `format()` directly (never a pager inside the live viewport).
- **Step 1 — Run-without-quit** *(the headline)*. A buffer-local trigger in `inject.lua`
  grabs the RPC channel (`vim.api.nvim_get_api_info()[1]` — 1-indexed; `[0]` silently
  no-ops) and `rpcnotify(ch, 'nsql_run', {sql=…})` with the **statement under the cursor**
  (or visual selection). In `embed.rs`: add `run_tx` to `RedrawHandler` + an
  `else if name=="nsql_run"` arm; thread `&Profile`+`all` into `run_session`; one new
  `select!` arm → `db::guard` → `db::run` → `render::format` →
  `terminal.insert_before(exact_line_count, …)` → `dirty=true`. Errors render as a red
  in-session block and **never kill the loop**. Invert `,,` to quit-*without*-write and
  return `SessionOutcome::{Handled,Run}` so `main` doesn't double-run (today's
  post-compose run at main.rs:81-94).
- **Step 2 — Zero-setup resume.** `recents.toml` in `state_dir` (0600); auto-record each
  successful connect (**url without password**); bare `nsql` → `recents[0]` else the
  bootstrapped `local`; broaden `extract_adhoc_url` (main.rs:131) from URL-only to
  URL | label | index (LRU bump).
- **Step 3 — CLI collapse to 2 flags + a smart positional** (see decision Q1). Delete the
  rest of the flags and the `connect/discover/profiles/save/favorites/history` subcommands
  and the `@name` preprocessing; move editor-mode to a config key.
- **Step 4 — History-ring "tabs."** Load `SELECT DISTINCT sql … LIMIT 9` from
  `history.sqlite` into an in-memory ring; `,n`/`,p` swap the buffer via
  `nvim_buf_set_lines` + an extmark slot label. **No tabline, no new persistence.**
- **Step 5 — Schema completion + highlighting.** `db::introspect` (one static query per
  engine) → sanitized identifiers → 0600 dict file → `inject.lua` adds
  `dictionary+=… complete+=k iskeyword+=.` (the dot is load-bearing for `table.column` —
  verified on nvim 0.12.2). Highlighting needs nothing beyond `setfiletype sql` (treesitter
  if installed, else nvim's regex fallback). Detect an attached sqls/cmp and step aside.
- **Step 6 — Credential hardening** (parts are quick wins, do early): `~/.pgpass` in
  `resolve_password`; **re-key the keyring on `user@host:port/db`** (fixes the security bug
  below); first-run hidden prompt + "save to keyring?" offer.
- **`,y` result-copy (OSC 52)** and a **`running…` + Ctrl-C-aborts** indicator land *with*
  Step 1 — the critic rates copy the #1 residual friction once querying is fast, and the
  spinner removes "is it hung?" anxiety on a slow query.

## Bright lines (enforce as tests, not preferences)

- **Results render in a fixed bottom pane inside nsql's own inline region; the user's main
  scrollback above is NEVER written to or scrolled** (no `insert_before`). The results pane
  is a **non-navigable preview** — it shows the latest result truncated to the pane height
  with a `(N rows · ,y copy · ,a all)` footer; it never scrolls/sorts/filters/pages in-app.
  *The moment a result is navigable in the viewport, nsql is harlequin.* `,y` (copy) is how
  you take a value out; `,a` re-runs uncapped. (Enforce non-navigability in `tests/smoke.rs`.)
- **Tabs are a pure read-projection of `history.sqlite`** — cap 9, no rendered tabline, no
  per-tab state (no rename/pin/close/reorder).
- **Completion is the dict file only** — no in-process alias/context/FK logic (that's
  rebuilding sqls). Detect-and-defer to an LSP is the ceiling.
- **Run = the statement under the cursor**, not the whole multi-statement buffer; prod
  profiles still require the typed confirm.
- **In-session meta set stays tiny** (run, cancel, copy, tabs). Connection-switch /
  favorites / history-search are things you **re-launch** for — re-launch is cheap
  *because* resume is instant.

## ⚠️ Security findings (verified against source — fix before/with the redesign)

1. **Keyring key collision = cross-host credential leak (HIGH).** `secrets::get` keys on
   `profile.name` (secrets.rs:14) and ad-hoc connections derive that from only the URL's
   last path segment (`adhoc_name`, main.rs:166). So `postgres://alice@prod/app` and
   `postgres://bob@staging/app` both key to keyring entry **`app`** — connecting to staging
   can inject the prod password. **Re-key on `user@host:port/db` (password stripped).** This
   is both the security fix *and* what makes silent resume work. Do it first, in isolation.
2. **TOCTOU file perms.** `create_dir_all` (config.rs:30) uses the umask (~0755) and every
   0600 chmod happens *after* `fs::write` (history.rs:23, editor.rs:109) — a world-readable
   window. Create `recents.toml` + the schema dict with `O_EXCL`+0600 (the `secure_tempfile`
   pattern, util.rs:88) and `chmod 0700` the state dir. The schema dict leaks your full
   table/column inventory otherwise.
3. **`recents.toml` stores the URL *without* password** (a separate, tested strip helper —
   *not* `util::redact_url`, which writes literal `***`). A `url_no_password` is still
   recon data (hostnames/ports/users/db names) → **`no_history` profiles are never recorded
   to recents** (default, not opt-in).
4. **`rpcnotify` payload is untrusted SQL.** Any plugin in the user's real nvim can fire
   `nsql_run`. Always re-derive `first_keyword`/`guard` on the **Rust** side per run; never
   trust a `mode`/`safe` flag from Lua; accept the notify from the embed channel only.
5. **Results in scrollback contain real data** (inherent — scrollback *is* the feature).
   Keep `render::sanitize` on the insert_before path (control-byte safety); never cache a
   result to any nsql-owned file; drop the in-memory last-result on exit.
6. **Inline prod-confirm stdin contention.** `db::guard` reads `yes` from stdin (db.rs:133)
   while the embed key-reader thread is draining stdin in raw mode — they race. Route the
   prod confirm **through nvim**, not a second raw stdin reader.

## Open decisions (yours) — see the question prompt
Q1 the 2 flags · Q2 the run trigger (`:w` vs `,r`) · Q3 credential persistence model ·
Q4 the result-copy "extract" key.

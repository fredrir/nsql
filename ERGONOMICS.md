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

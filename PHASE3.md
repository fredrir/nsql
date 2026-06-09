# nsql — Phase 3 brainstorm

*Synthesized from a multi-agent research + design panel (4 grounded researchers → 3
competing themes → judge + completeness critic). Several claims were verified on this
machine — including an agent that hand-rolled a msgpack-RPC client and drove this box's
nvim 0.12.2 `--embed` UI to confirm the zero-flash editor works today.*

---

## TL;DR verdict

The **primary goal is already met** by Mode 1 (transient-child nvim, scrollback preserved),
so more editor work has diminishing returns. The value now is in (a) **reaching real
databases** and (b) **compounding daily ergonomics**. The panel ranked:

| # | Theme | Score | One-liner |
|---|-------|-------|-----------|
| 1 | **Production-Ready Client** | 9 | TLS + SSH tunnel + Ctrl-C-cancels-the-backend. Today `NoTls` (postgres.rs:30) makes every managed cloud DB *literally unreachable*. |
| 2 | **Power DBA Ergonomics** | 8 | Introspection (`\d`), nvim completion, parameterized favorites, export, richer rendering. Densest cluster of cheap compounding wins. |
| 3 | **Editor Purist** (zero-flash `--embed`) | 6 | The signature flourish, *proven feasible here*, but L-effort (~3–4 wks, first async in a sync binary) to remove a sub-second flash Mode 1 already makes acceptable. |

**The three themes agree on the same cheapest, highest-value first moves** — the
connection-pinning refactor, Ctrl-C cancellation, and engine-agnostic introspection — so
the plan below front-loads that shared core and orders everything by value-per-effort.

---

## The blended plan (ordered by value-per-effort)

### Tier 0 — shared structural seam + biggest safety-per-line win
1. **Connection-pinning refactor** *(M)* — split `db::run` into `connect(profile) -> Conn`
   + `run_on(&Conn, sql, all)`, and formalize the implicit dispatch into a small `Backend`
   trait. No user-visible change. Prerequisite for *correct* (same-connection) cancellation,
   `statement_timeout`, and any future `--repeat` session.
   - ⚠️ The `Conn` type is the linchpin and the critic's top design gap: `rusqlite::Connection`
     and `postgres::Client` are different types with different cancel handles → `Conn` must be
     an enum (or boxed) that *also carries the cancel token / interrupt handle*. Design this first.
2. **Ctrl-C → backend cancel + connect/statement timeouts** *(S)* — `ctrlc` crate; PG uses
   `client.cancel_token().cancel_query(tls)` on a **separate thread with its own timeout**
   (2nd Ctrl-C hard-exits); SQLite uses `conn.get_interrupt_handle().interrupt()`.
   - ⚠️ Install the SIGINT handler **only around DB execution**, never during `editor::compose`
     — otherwise a Ctrl-C mid-edit could leave the terminal in nvim's alt screen, violating the
     sacred invariant.

### Tier 1 — the connection unblocker (this is why Production won)
3. **Postgres TLS** *(M)* — `postgres-native-tls` + `native-tls` (system OpenSSL 3.6 present).
   nsql owns the **4-way sslmode mapping** because the crate's `SslMode` only chooses *whether*
   to attempt TLS, not whether to verify:
   - `disable` → NoTls · `require` → encrypt-only (`danger_accept_invalid_certs/hostnames`) ·
     `verify-ca` → CA chain, skip hostname · `verify-full` → full (the secure default when a CA is present).
   - Add `#[serde(default)] sslmode/sslrootcert/sslcert/sslkey` to `Profile` **and** parse
     `?sslmode=...&sslrootcert=...` from the URL so pasted cloud strings work unchanged.
   - ⚠️ **Security footgun:** a too-liberal default silently downgrades to unverified ("encrypted
     but MITM-able") TLS. Default to verify-full when a CA is present; `require` is an explicit,
     loud opt-in. Add a test that **verify-full rejects a self-signed cert** (a "does it connect?"
     test would pass with verification silently off).
4. **SSH tunnel / bastion** *(M)* — shell out to OpenSSH (present), don't embed (`russh` is async,
   `ssh2` needs per-conn threads). Inherits `~/.ssh/config`, ProxyJump, agent, MFA for free.
   - ⚠️ Two concrete bugs the critic caught in the naive sketch:
     - `ssh -fN` **backgrounds itself**, so the captured child PID exits immediately — you can't
       kill the real forwarder. Use foreground `ssh` in a thread, or a `ControlMaster` socket with
       `-O exit` teardown. Also need a real **readiness probe** (poll-connect 127.0.0.1:port).
     - **TLS-over-tunnel hostname mismatch:** if you rewrite the socket to `127.0.0.1:port`,
       verify-full will validate the cert against `127.0.0.1` and fail — pin TLS verification to the
       **original DB hostname** while connecting the socket to localhost. (No theme stated this;
       it's exactly where security regressions hide. Add a combined verify-full-through-tunnel test.)

### Tier 2 — compounding daily ergonomics (cheap, sync, both invariants preserved)
5. **Introspection verbs** *(S)* — `src/introspect.rs` builds SQL strings per scheme
   (SQLite PRAGMA/`sqlite_master`; PG+MySQL share an `information_schema` template) fed through
   the *existing* `db::run`. Surface as `nsql tables / describe <t> / schemas / functions`. The
   guard already whitelists SELECT/PRAGMA/SHOW so output renders unchanged.
   - ⚠️ PG simple-query has **no bind params** → identifier interpolation is the #1 injection risk:
     escape single-quotes (double them), reject NUL/control chars, prefer `to_regclass`.
6. **Built-in dictionary completer** *(S)* — dump introspected identifiers to a 0600 temp file
   (short-TTL per-profile cache, `--refresh`) and append `-c 'setlocal dictionary+=<f> complete+=k'`
   to the nvim args. Real table/column completion in *your* nvim with **no LSP, no async**, works for
   SQLite too. Improves both editor modes immediately.
7. **Rendering polish** *(S, no deps, all independent)* — configurable NULL glyph (currently
   hardcoded), **auto-`\x`** when a row exceeds `term_size()` width (measurement + renderer already
   exist), wide-cell truncation with ellipsis, `--format ndjson`, and **capture Postgres
   NOTICE/WARNING** (currently dropped at `postgres.rs` `_ => {}`). Makes the one-shot path feel finished.
8. **Parameterized favorites** *(M, no deps)* — scan favorites `.sql` for `:name` / `:'name'`
   (literal-quote) / `:"name"` (identifier-quote); resolve from `--set k=v` / `@k=v` / env /
   interactive prompt; **nsql does the quoting**; bare raw `:x` gated behind `--unsafe-subst`.
   - ⚠️ Record the **template, not the bound values**, in history.sqlite — prompted/`--set` values
     may be secrets.
9. **Streaming export** *(S + M)* — `--out FILE` / `\g file` redirects the rendered output; for PG,
   `COPY (<q>) TO STDOUT (FORMAT csv, HEADER)` via `copy_out()` **bypasses the 1000-row cap and never
   buffers**. SQLite streams rows through the csv writer uncapped. (Skip Arrow/Parquet — too heavy.)

### Tier 3 — lower-urgency / opt-in (ship whenever)
10. **Docker/podman discovery** *(S)* — replace the stub by shelling `docker ps --format '{{json .}}'`
    + `docker inspect` to synthesize a ready profile URL. Zero deps. (docker present, podman absent here.)
11. **Typed Postgres `--json`** *(M)* — only under `--json`, re-run via `client.query()` and OID-dispatch
    (bool/int/float/numeric → real JSON; json/jsonb → raw; unknown → text, never panic). Keeps the
    default text path psql-identical. Same `query()` entrypoint is the foundation for safe `$1` binds.
    - ⚠️ The existing `json_output` test passes against **SQLite** (where ints already serialize as
      numbers) — it gives false coverage. The defect is **PG-only**; add a PG `number → JSON number` test.
12. **MySQL/MariaDB backend** *(M)* — behind the now-formalized `Backend` trait, reusing TLS + SSH.

### Deferred — luxury / largest, lowest value-per-effort
- **Zero-flash `--embed` inline editor** *(L)* — proven feasible on this box (Strategy A:
  `nvim_ui_attach` + `ext_linegrid` redraw stream → ratatui `Viewport::Inline`). Build **only** behind
  a `embed-editor` cargo feature + `--embed` flag, Mode 1 stays the default, and **only after the above
  ship**. Sequence M1 (dumb monochrome loop proving the exit-code/readback contract + headless regression
  test) → M2 (color/cursor/scroll fidelity + popupmenu/cmdline/messages overlays) → M3 (resize, u16
  clamping, input fidelity), soak before ever flipping default.
  - ⚠️ Pin the nvim **UI protocol version** on attach (events carry `since=`); refuse/degrade on
    unsupported nvim or rendering silently breaks on user upgrades. **Async contagion** is the strategic
    risk: keep the runtime strictly inside `compose()`'s stack frame and add a CI job that builds+tests the
    **default sync** feature set so async never becomes mandatory.
- **`--repeat` line-oriented session REPL** *(L)* — unlocks interactive transactions (BEGIN/COMMIT
  pinned to one connection), temp tables, `SET`. Cheap *after* the Tier-0 pinning refactor. Keep it
  strictly line-oriented (never alt screen); **cap the meta-command set** (~5: `\e \g \q \d \c`) to avoid
  becoming a second product.
- **LSP auto-attach** (sqls / postgrestools) *(M)* — opt-in, layered over the dictionary completer for
  users who already have the binary. Transient connection config must be 0600 + removed on exit (it embeds
  the password).

---

## Cross-cutting gotchas the critic flagged (don't lose these)
- **EXPLAIN** was researched but dropped from every theme — it's a cheap verb (`EXPLAIN [ANALYZE]
  [FORMAT JSON]` prefixes per engine). ⚠️ `EXPLAIN ANALYZE` **executes** the statement (incl. writes) →
  route through the prod/readonly guard. Include it as a Tier-2 quick win or explicitly skip it.
- **Transaction-on-fresh-connection:** today a bare `BEGIN` in one-shot mode silently targets a
  connection that's dropped before the next call — no error. Detect a bare `BEGIN` and warn it has no
  effect without `--repeat`.
- **Pager interaction:** export must NOT page (write straight to file); NDJSON-to-tty should still respect
  the pager.
- **Offline builds:** every new dep must be resolved/committed to `Cargo.lock` where crates.io is reachable
  (it is, on your box) before CI in a sandboxed env can build.

---

## Progress

- ✅ **Zero-flash `--embed` editor — M1 shipped** (behind the `embed-editor` cargo
  feature + `--embed` flag; Mode 1 remains default). Spawns `nvim --embed` over
  msgpack-RPC (nvim-rs), renders the `ext_linegrid` stream into a ratatui inline
  `Viewport::Inline`, reuses the `,,`/`:wq` exit-code + temp-file contract.
  Verified end-to-end against real nvim 0.12.2: a headless test (type → grid
  renders → `:wq` → readback) **and** a pty drive asserting **no smcup** with the
  result printed to scrollback. A select-loop starvation bug (chatty configs
  starving keypresses) was found and fixed via the pty test. Monochrome renderer;
  **M2** = `hl_attr_define`/`default_colors_set` color + cursor/mode + popupmenu/
  cmdline overlays; **M3** = resize/`u16` clamping/input fidelity.

## Recommended first 3 tasks (the *other* tracks, for later)
1. **Connection-pinning refactor** (design the `Conn` enum carrying the cancel handle; formalize `Backend`).
2. **Ctrl-C cancellation + timeouts** (smallest diff, biggest safety win, validates the seam).
3. **Postgres TLS** with the 4-way sslmode mapping + the verify-full-rejects-self-signed test
   (the line that unblocks every managed cloud Postgres).

…unless your day-to-day is mostly local SQLite / docker Postgres, in which case **lead with Tier 2
ergonomics** (introspection + completer + rendering polish + parameterized favorites) — same cheap shared
core, more felt value for that workflow.

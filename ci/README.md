# Checks

| Scope | Command |
| --- | --- |
| Fast unit tests | `cargo nextest run --bins --locked -- --skip embed_drives_real_nvim_and_reads_back --skip embed_captures_highlight_colors --skip run_keymap_round_trips_via_rpcnotify` |
| Full local suite, including Neovim integration | `command -v nvim && cargo nextest run --all-targets --locked` |
| Neovim integration | `command -v nvim && cargo nextest run --bins --locked -E 'test(embed::session::tests)'` |

| Local dependency | Value |
| --- | --- |
| Neovim | `nvim` on `PATH` |
| PostgreSQL integration | `NSQL_TEST_PG_URL` |

The full local suite does not apply the CI fast-test filters.

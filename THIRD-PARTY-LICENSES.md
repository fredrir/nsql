# Third-party license notices

nsql is licensed under [0BSD](LICENSE). Its release binaries statically link
many permissively licensed Rust crates (MIT/Apache-2.0/BSD), plus one
dependency whose license carries notice obligations:

## nvim-rs — LGPL-3.0

[`nvim-rs`](https://crates.io/crates/nvim-rs) implements the msgpack-RPC
protocol used by the optional `embed-editor` feature (the zero-flash inline
Neovim session). It is licensed under the GNU Lesser General Public License
v3.0.

How the LGPLv3 combined-work terms (§4) are met for nsql's binaries:

- The complete corresponding source of nsql — including the exact `nvim-rs`
  version pinned in `Cargo.lock` and everything needed to rebuild the
  combined work — is publicly available at
  <https://github.com/fredrir/nsql>. You can modify `nvim-rs` (e.g. via a
  `[patch.crates-io]` override) and relink by rebuilding with `cargo build`.
- Nothing in nsql restricts modification or reverse engineering for
  debugging such modifications.
- This notice ships with the release archives and packages.

To obtain a binary with no LGPL-licensed code at all, build with
`cargo build --no-default-features` (drops the embed editor; the classic
editor flow and all backends keep working, minus OS-keyring support).

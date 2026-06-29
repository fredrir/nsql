# nsql

> Embedded Neovim CLI SQL Editor


nsql is a small SQL client for **SQLite** and **PostgreSQL**.

## Install

### Arch Linux (AUR)

```sh
yay -S nsql
yay -S nsql-bin
```

### Homebrew (macOS / Linux)

```sh
brew install fredrir/nsql/nsql
```


### Debian / Ubuntu (apt)

```sh
curl -fsSL https://fredrir.github.io/nsql/deb/nsql-archive-keyring.asc \
  | sudo gpg --dearmor -o /usr/share/keyrings/nsql.gpg
echo "deb [signed-by=/usr/share/keyrings/nsql.gpg] https://fredrir.github.io/nsql/deb stable main" \
  | sudo tee /etc/apt/sources.list.d/nsql.list
sudo apt update && sudo apt install nsql
```

### Cargo (crates.io)

```sh
cargo install nsql
```

### Prebuilt binary (shell installer)

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/fredrir/nsql/releases/latest/download/nsql-installer.sh | sh
```

[GitHub Release](https://github.com/fredrir/nsql/releases).

> **Note:** Neovim is the expected editor but optional — nsql falls back to
> `$EDITOR`/vim/vi. On Linux the default build links libdbus for OS-keychain storage;
> if a keychain isn't available, nsql falls back to `PGPASSWORD` / `~/.pgpass` / prompt.

## Quick start

```sh
nsql -e "select 1 + 1 as two"                  
nsql sqlite://app.db -e "select * from users"  
nsql --edit                                    
nsql --json -e "select 1 as a"                 
```

## Build from source

```sh
cargo build --release              
cargo build --no-default-features  
```

Linux builds need `libdbus-1-dev` + `pkg-config` for the default `keyring-store` feature.

## License

[0BSD](LICENSE).

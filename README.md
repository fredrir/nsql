# nsql

> Embedded Neovim CLI SQL Editor


nsql is a small SQL client for **SQLite** and **PostgreSQL**.

## Install

| Platform               | Command                                                          |
| ---------------------- | ---------------------------------------------------------------- |
| Any Linux / macOS      | `curl -fsSL https://pkgs.fredrir.com/install.sh \| sh -s -- nsql` |
| Homebrew (macOS/Linux) | `brew install fredrir/tap/nsql`                                  |
| Arch Linux (AUR)       | `yay -S nsql` or `yay -S nsql-bin`                               |
| Nix                    | `nix run github:fredrir/nur-packages#nsql`                       |
| Cargo                  | `cargo install nsql` or `cargo binstall nsql`                    |

### Debian / Ubuntu

```sh
sudo curl -fsSLo /etc/apt/keyrings/fredrir.asc https://pkgs.fredrir.com/keys/fredrir.asc
sudo curl -fsSLo /etc/apt/sources.list.d/fredrir.list https://pkgs.fredrir.com/deb/fredrir.list
sudo apt update && sudo apt install nsql
```

### Fedora / RHEL / openSUSE

```sh
sudo curl -fsSLo /etc/yum.repos.d/fredrir.repo https://pkgs.fredrir.com/rpm/fredrir.repo
sudo dnf install nsql
```

```sh
sudo zypper addrepo https://pkgs.fredrir.com/rpm/fredrir.repo
sudo zypper install nsql
```

### Alpine

```sh
wget -qO /etc/apk/keys/fredrir.rsa.pub https://pkgs.fredrir.com/keys/fredrir.rsa.pub
echo https://pkgs.fredrir.com/apk >> /etc/apk/repositories
apk add nsql
```

[GitHub Releases](https://github.com/fredrir/nsql/releases) carry Linux (glibc 2.28+ and static musl) and macOS archives for x86_64 and aarch64.

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

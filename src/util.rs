//! Small cross-cutting helpers: secure temp files, PATH lookup, editor
//! resolution, DSN redaction, terminal size.

use anyhow::{bail, Result};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// What kind of editor we resolved, so we know which flags are safe to pass.
#[derive(Debug, PartialEq, Eq)]
pub enum EditorKind {
    /// Neovim: gets `-i NONE` + filetype + our buffer-local keymap inject.
    Nvim,
    /// Vim/vi family: gets filetype but no neovim-only lua inject.
    Vimlike,
    /// Anything else (nano, code, …): just open the file.
    Other,
}

/// Resolve the editor command, mirroring psql/kubectl precedence:
/// NSQL_EDITOR > VISUAL > EDITOR > nvim > vi. Returns the program and its kind.
pub fn resolve_editor() -> Result<(String, EditorKind)> {
    let mut candidates: Vec<String> = Vec::new();
    for var in ["NSQL_EDITOR", "VISUAL", "EDITOR"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                candidates.push(v);
            }
        }
    }
    candidates.push("nvim".to_string());
    candidates.push("vi".to_string());

    for cand in candidates {
        // EDITOR may include args ("code -w"); take the first token as the program.
        let program = cand.split_whitespace().next().unwrap_or(&cand).to_string();
        if find_on_path(&program).is_some() {
            return Ok((program.clone(), editor_kind(&program)));
        }
    }
    bail!(
        "no usable editor found (set $NSQL_EDITOR or $EDITOR, or install neovim). \
You can still run SQL without the editor via -e/-f or a pipe."
    )
}

fn editor_kind(program: &str) -> EditorKind {
    let base = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    if base.contains("nvim") {
        EditorKind::Nvim
    } else if matches!(base, "vi" | "vim" | "view" | "gvim" | "ex" | "vimx") {
        EditorKind::Vimlike
    } else {
        EditorKind::Other
    }
}

/// Find an executable on $PATH (or honour an explicit path containing '/').
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Create a fresh 0600 temp file with an unpredictable name (O_EXCL guards
/// against a symlink/TOCTOU pre-creation attack on multi-user boxes). Returns
/// the path; the caller owns it.
pub fn secure_tempfile(prefix: &str, ext: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir();
    for _ in 0..16 {
        let token = rand_token();
        let path = dir.join(format!("{prefix}-{}-{}.{ext}", std::process::id(), token));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("could not create a unique temp file in {}", dir.display())
}

fn rand_token() -> String {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    // Fallback: pid + monotonic-ish nanos (not security-critical because
    // create_new still guarantees we created the file).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}{nanos:08x}", std::process::id())
}

/// Decompose `scheme://user:pass@host:port/path` into (head, user, password,
/// host_and_path). Uses the LAST `@` in the *authority* (before the path) so a
/// password that itself contains `@` is handled correctly.
fn split_url(url: &str) -> Option<(&str, &str, Option<&str>, String)> {
    let i = url.find("://")?;
    let (head, rest) = url.split_at(i + 3);
    let auth_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(auth_end);
    let at = authority.rfind('@')?;
    let (userinfo, hostat) = authority.split_at(at); // hostat = "@host:port"
    let (user, password) = match userinfo.find(':') {
        Some(c) => (&userinfo[..c], Some(&userinfo[c + 1..])),
        None => (userinfo, None),
    };
    Some((head, user, password, format!("{hostat}{path}")))
}

/// Mask the password in a connection URL so it never lands in scrollback, logs,
/// or error messages. `scheme://user:secret@host` -> `scheme://user:***@host`.
pub fn redact_url(url: &str) -> String {
    match split_url(url) {
        Some((head, user, Some(_), hostpath)) => format!("{head}{user}:***{hostpath}"),
        _ => url.to_string(),
    }
}

/// Remove the password from a connection URL for *storage* (keeps the username).
/// `scheme://user:secret@host` -> `scheme://user@host`. NOTE: distinct from
/// `redact_url`, which substitutes `***` for *display* — never use that for
/// storage or it would write a literal `***` password.
pub fn strip_url_password(url: &str) -> String {
    match split_url(url) {
        Some((head, user, Some(_), hostpath)) => format!("{head}{user}{hostpath}"),
        _ => url.to_string(),
    }
}

/// The password embedded in a connection URL, if any (used to migrate it into
/// the keyring at `connect` time so it never persists in config.toml).
pub fn url_password(url: &str) -> Option<String> {
    match split_url(url) {
        Some((_, _, Some(pw), _)) if !pw.is_empty() => Some(pw.to_string()),
        _ => None,
    }
}

/// Atomically write a file readable only by the owner (0600), closing the
/// write-then-chmod TOCTOU window: write to an O_EXCL 0600 temp in the same
/// directory, then rename over the target.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("nsql");
    for _ in 0..16 {
        let tmp = dir.join(format!(".{name}.{}.tmp", rand_token()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(mut f) => {
                f.write_all(bytes)?;
                f.sync_all().ok();
                drop(f);
                std::fs::rename(&tmp, path)?;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("could not create a temp file next to {}", path.display())
}

/// Best-effort: tighten a directory to owner-only (0700).
pub fn chmod_private_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

/// (columns, rows) of the terminal, with a sane fallback when not a tty.
pub fn term_size() -> (u16, u16) {
    match terminal_size::terminal_size() {
        Some((terminal_size::Width(w), terminal_size::Height(h))) => (w.max(20), h.max(4)),
        None => (100, 40),
    }
}

pub fn url_has_password(url: &str) -> bool {
    url.split_once("://")
        .and_then(|(_, rest)| rest.split_once('@'))
        .map(|(userinfo, _)| userinfo.contains(':'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction() {
        assert_eq!(
            redact_url("postgres://joe:hunter2@db.local:5432/app"),
            "postgres://joe:***@db.local:5432/app"
        );
        // no password -> unchanged
        assert_eq!(redact_url("sqlite:///x/y.db"), "sqlite:///x/y.db");
        assert_eq!(
            redact_url("postgres://joe@host/db"),
            "postgres://joe@host/db"
        );
    }

    #[test]
    fn password_with_at_sign_does_not_leak() {
        // A password containing '@' must be fully masked/stripped (uses the LAST
        // '@' in the authority, not the first).
        let u = "postgres://joe:p@ss@db.host:5432/app";
        assert_eq!(redact_url(u), "postgres://joe:***@db.host:5432/app");
        assert_eq!(strip_url_password(u), "postgres://joe@db.host:5432/app");
        assert_eq!(url_password(u).as_deref(), Some("p@ss"));
    }

    #[test]
    fn secure_tempfile_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let p = secure_tempfile("nsql-test", "sql").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_file(&p).ok();
    }
}

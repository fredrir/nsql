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

/// Mask the password in a connection URL so it never lands in scrollback, logs,
/// or error messages. `scheme://user:secret@host` -> `scheme://user:***@host`.
pub fn redact_url(url: &str) -> String {
    if let Some(i) = url.find("://") {
        let (head, rest) = url.split_at(i + 3);
        if let Some(at) = rest.find('@') {
            let creds = &rest[..at];
            let after = &rest[at..];
            if let Some(colon) = creds.find(':') {
                return format!("{head}{}:***{after}", &creds[..colon]);
            }
        }
    }
    url.to_string()
}

/// (columns, rows) of the terminal, with a sane fallback when not a tty.
pub fn term_size() -> (u16, u16) {
    match terminal_size::terminal_size() {
        Some((terminal_size::Width(w), terminal_size::Height(h))) => (w.max(20), h.max(4)),
        None => (100, 40),
    }
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
        assert_eq!(redact_url("postgres://joe@host/db"), "postgres://joe@host/db");
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

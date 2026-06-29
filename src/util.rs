use anyhow::{bail, Result};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum EditorKind {
    Nvim,
    Vimlike,
    Other,
}

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
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}{nanos:08x}", std::process::id())
}

fn split_url(url: &str) -> Option<(&str, &str, Option<&str>, String)> {
    let i = url.find("://")?;
    let (head, rest) = url.split_at(i + 3);
    let auth_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(auth_end);
    let at = authority.rfind('@')?;
    let (userinfo, hostat) = authority.split_at(at);
    let (user, password) = match userinfo.find(':') {
        Some(c) => (&userinfo[..c], Some(&userinfo[c + 1..])),
        None => (userinfo, None),
    };
    Some((head, user, password, format!("{hostat}{path}")))
}

pub fn redact_url(url: &str) -> String {
    match split_url(url) {
        Some((head, user, Some(_), hostpath)) => format!("{head}{user}:***{hostpath}"),
        _ => url.to_string(),
    }
}

pub fn strip_url_password(url: &str) -> String {
    match split_url(url) {
        Some((head, user, Some(_), hostpath)) => format!("{head}{user}{hostpath}"),
        _ => url.to_string(),
    }
}

pub fn url_password(url: &str) -> Option<String> {
    match split_url(url) {
        Some((_, _, Some(pw), _)) if !pw.is_empty() => Some(pw.to_string()),
        _ => None,
    }
}

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

pub fn chmod_private_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

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
        assert_eq!(redact_url("sqlite:///x/y.db"), "sqlite:///x/y.db");
        assert_eq!(
            redact_url("postgres://joe@host/db"),
            "postgres://joe@host/db"
        );
    }

    #[test]
    fn password_with_at_sign_does_not_leak() {
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

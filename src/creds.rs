//! Credential resolution for connections whose URL carries no password — so a
//! resumed `postgres://user@host/db` connects silently without re-prompting.
//!
//! Resolution order (the caller uses an explicit in-URL password first):
//!   PGPASSWORD env  >  ~/.pgpass  >  OS keyring (keyed on user@host:port/db)
//!
//! The keyring is keyed on a STABLE identity (user@host:port/db), NOT the
//! display/profile name — keying on a volatile name caused a cross-host
//! credential collision (e.g. two different `…/app` databases sharing one entry).

use crate::config::Profile;
use crate::secrets;

pub struct PgIdentity {
    pub user: String,
    pub host: String,
    pub port: u16,
    pub db: String,
}

/// Stable keyring key for a connection. Never contains a password.
pub fn identity_key(id: &PgIdentity) -> String {
    format!("{}@{}:{}/{}", id.user, id.host, id.port, id.db)
}

/// Parse a postgres URL into its identity parts (best-effort).
pub fn pg_identity(url: &str) -> Option<PgIdentity> {
    let rest = url.split_once("://")?.1; // [user[:pw]@]host[:port]/db[?params]
    let (authority, after) = rest.split_once('/').unwrap_or((rest, ""));
    let db = after.split(['?', '&']).next().unwrap_or("").to_string();

    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (u, h),
        None => ("", authority),
    };
    let mut user = userinfo.split(':').next().unwrap_or("").to_string();
    if user.is_empty() {
        // libpq falls back to PGUSER / the OS user when none is given.
        user = std::env::var("PGUSER")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_default();
    }

    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h.to_string(), p.parse().unwrap_or(5432))
        }
        _ => (hostport.to_string(), 5432u16),
    };
    if host.is_empty() {
        return None;
    }
    Some(PgIdentity {
        user,
        host,
        port,
        db,
    })
}

/// Resolve a password for a profile whose URL omits one. Returns None if nothing
/// in the chain has it (the connection then fails with a clear error).
pub fn resolve_password(profile: &Profile) -> Option<String> {
    if let Ok(v) = std::env::var("PGPASSWORD") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    let id = pg_identity(&profile.url)?;
    if let Some(pw) = pgpass_lookup(&id) {
        return Some(pw);
    }
    secrets::get(&identity_key(&id))
}

/// Look up `~/.pgpass` (or `$PGPASSFILE`) for a matching line. Mirrors libpq:
/// the file is IGNORED if its permissions are group/world accessible.
fn pgpass_lookup(id: &PgIdentity) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::var_os("PGPASSFILE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".pgpass")))?;

    let meta = std::fs::metadata(&path).ok()?;
    // Must be a regular file (reject a symlink-to-dir/fifo/device target).
    if !meta.is_file() {
        return None;
    }
    // Refuse a too-permissive file, like libpq (warn once, then ignore). A file
    // the owner can read at <=0600 is, by construction, owned by them.
    if meta.permissions().mode() & 0o077 != 0 {
        eprintln!(
            "nsql: ignoring {} — permissions are too open (chmod 600 it)",
            path.display()
        );
        return None;
    }

    let contents = std::fs::read_to_string(&path).ok()?;
    for line in contents.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some([h, p, d, u, pw]) = split_pgpass_line(line) else {
            continue;
        };
        if field_matches(&h, &id.host)
            && field_matches(&p, &id.port.to_string())
            && field_matches(&d, &id.db)
            && field_matches(&u, &id.user)
        {
            return Some(pw);
        }
    }
    None
}

fn field_matches(field: &str, value: &str) -> bool {
    field == "*" || field == value
}

/// Split a pgpass line into [host, port, database, user, password], honoring
/// `\` escaping of `:` and `\`. The password (5th field) may contain `:`.
fn split_pgpass_line(line: &str) -> Option<[String; 5]> {
    let mut fields: Vec<String> = Vec::with_capacity(5);
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ':' if fields.len() < 4 => fields.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    fields.push(cur);
    if fields.len() == 5 {
        let mut it = fields.into_iter();
        Some([
            it.next().unwrap(),
            it.next().unwrap(),
            it.next().unwrap(),
            it.next().unwrap(),
            it.next().unwrap(),
        ])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_parsing_and_key() {
        let id = pg_identity("postgres://alice:secret@db.host:5433/payments?sslmode=require").unwrap();
        assert_eq!(id.user, "alice");
        assert_eq!(id.host, "db.host");
        assert_eq!(id.port, 5433);
        assert_eq!(id.db, "payments");
        assert_eq!(identity_key(&id), "alice@db.host:5433/payments");

        // Default port, no user in URL still yields a host/db.
        let id2 = pg_identity("postgres://just.host/mydb").unwrap();
        assert_eq!(id2.port, 5432);
        assert_eq!(id2.db, "mydb");
    }

    #[test]
    fn pgpass_line_split_with_escapes() {
        let f = split_pgpass_line("db.host:5432:app:alice:p\\:a\\\\ss").unwrap();
        assert_eq!(f[0], "db.host");
        assert_eq!(f[3], "alice");
        assert_eq!(f[4], "p:a\\ss"); // \: -> :  and  \\ -> \
        assert!(field_matches("*", "anything"));
        assert!(!field_matches("app", "other"));
    }
}

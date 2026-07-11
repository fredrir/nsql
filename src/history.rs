use crate::config::Paths;
use anyhow::{Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};

fn open(paths: &Paths) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(&paths.history_db)
        .with_context(|| format!("opening {}", paths.history_db.display()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS history (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            ts      INTEGER NOT NULL,
            profile TEXT NOT NULL,
            sql     TEXT NOT NULL
        );",
    )?;
    set_0600(&paths.history_db);
    Ok(conn)
}

fn set_0600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

pub fn record(paths: &Paths, profile: &str, sql: &str) -> Result<()> {
    let conn = open(paths)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    conn.execute(
        "INSERT INTO history (ts, profile, sql) VALUES (?1, ?2, ?3)",
        rusqlite::params![ts, profile, sql],
    )?;
    Ok(())
}

/// The most recent query recorded for a profile (used by --last).
pub fn last_for(paths: &Paths, profile: &str) -> Result<Option<String>> {
    if !paths.history_db.exists() {
        return Ok(None);
    }
    let conn = open(paths)?;
    let mut stmt =
        conn.prepare("SELECT sql FROM history WHERE profile = ?1 ORDER BY id DESC LIMIT 1")?;
    let mut rows = stmt.query([profile])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get::<_, String>(0)?),
        None => None,
    })
}

pub fn list(paths: &Paths, limit: usize) -> Result<()> {
    if !paths.history_db.exists() {
        println!("(no history yet)");
        return Ok(());
    }
    let conn = open(paths)?;
    let mut stmt =
        conn.prepare("SELECT ts, profile, sql FROM history ORDER BY id DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (ts, profile, sql) = row?;
        let one_line = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        let preview: String = one_line.chars().take(100).collect();
        println!("{ts}  [{profile}]  {preview}");
    }
    Ok(())
}

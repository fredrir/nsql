use crate::config::Paths;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn path_for(paths: &Paths, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        bail!("invalid favorite name `{name}`");
    }
    Ok(paths.favorites_dir.join(format!("{name}.sql")))
}

pub fn save(paths: &Paths, name: &str, content: &str) -> Result<PathBuf> {
    let p = path_for(paths, name)?;
    std::fs::write(&p, content).with_context(|| format!("writing {}", p.display()))?;
    Ok(p)
}

pub fn load(paths: &Paths, name: &str) -> Result<String> {
    let p = path_for(paths, name)?;
    std::fs::read_to_string(&p)
        .with_context(|| format!("no favorite `{name}` ({})", p.display()))
}

pub fn list(paths: &Paths) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&paths.favorites_dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) == Some("sql") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    Ok(names)
}

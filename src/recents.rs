use crate::config::{Config, Paths, Profile};
use crate::util;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const CAP: usize = 10;

#[derive(Deserialize, Serialize, Default)]
pub struct Recents {
    #[serde(default, rename = "recent")]
    pub entries: Vec<Recent>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Recent {
    pub label: String,
    pub url: String,
    pub last_used: i64,
    #[serde(default)]
    pub profile: Option<String>,
}

impl Recent {
    pub fn to_profile(&self, cfg: &Config) -> Profile {
        if let Some(name) = &self.profile {
            if let Some(p) = cfg.profiles.iter().find(|p| &p.name == name) {
                return p.clone();
            }
        }
        Profile {
            name: self.label.clone(),
            url: self.url.clone(),
            prod: false,
            readonly: false,
            no_history: false,
        }
    }
}

pub fn load(paths: &Paths) -> Recents {
    std::fs::read_to_string(&paths.recents_file)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(paths: &Paths, r: &Recents) {
    if let Ok(text) = toml::to_string_pretty(r) {
        let _ = util::write_private(&paths.recents_file, text.as_bytes());
    }
}

pub fn record(paths: &Paths, profile: &Profile, saved: bool) {
    if profile.no_history {
        return;
    }
    let url = util::strip_url_password(&profile.url);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut r = load(paths);
    r.entries.retain(|e| e.url != url);
    r.entries.insert(
        0,
        Recent {
            label: profile.name.clone(),
            url,
            last_used: now,
            profile: saved.then(|| profile.name.clone()),
        },
    );
    r.entries.truncate(CAP);
    save(paths, &r);
}

pub fn most_recent(paths: &Paths) -> Option<Recent> {
    load(paths).entries.into_iter().next()
}

pub fn resolve(paths: &Paths, target: &str) -> Option<Recent> {
    let r = load(paths);
    if let Ok(idx) = target.parse::<usize>() {
        if idx >= 1 {
            return r.entries.into_iter().nth(idx - 1);
        }
    }
    r.entries.into_iter().find(|e| e.label == target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(dir: &std::path::Path) -> Paths {
        Paths {
            config_file: dir.join("config.toml"),
            state_dir: dir.to_path_buf(),
            favorites_dir: dir.join("fav"),
            history_db: dir.join("h.sqlite"),
            inject_lua: dir.join("inject.lua"),
            default_db: dir.join("dev.db"),
            recents_file: dir.join("recents.toml"),
        }
    }

    fn prof(name: &str, url: &str) -> Profile {
        Profile {
            name: name.into(),
            url: url.into(),
            prod: false,
            readonly: false,
            no_history: false,
        }
    }

    #[test]
    fn records_lru_without_password_and_resolves() {
        let dir = util::secure_tempfile("nsql-rec", "d").unwrap();
        std::fs::remove_file(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let p = paths_in(&dir);

        record(&p, &prof("app", "postgres://u:secret@h:5432/app"), false);
        record(&p, &prof("local", "sqlite:///tmp/x.db"), false);

        let raw = std::fs::read_to_string(&p.recents_file).unwrap();
        assert!(!raw.contains("secret"), "password leaked into recents: {raw}");
        assert_eq!(most_recent(&p).unwrap().label, "local");
        assert_eq!(resolve(&p, "1").unwrap().label, "local");
        assert_eq!(resolve(&p, "app").unwrap().url, "postgres://u@h:5432/app");

        record(&p, &prof("app", "postgres://u:other@h:5432/app"), false);
        assert_eq!(most_recent(&p).unwrap().label, "app");
        assert_eq!(load(&p).entries.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_history_is_not_recorded() {
        let dir = util::secure_tempfile("nsql-rec2", "d").unwrap();
        std::fs::remove_file(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let p = paths_in(&dir);
        let mut sensitive = prof("sec", "postgres://u:pw@h/db");
        sensitive.no_history = true;
        record(&p, &sensitive, false);
        assert!(most_recent(&p).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}

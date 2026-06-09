//! Paths (XDG with proper fallbacks) and the profiles config.
//!
//! Note: $XDG_CONFIG_HOME / $XDG_STATE_HOME are frequently unset (verified on
//! this machine), so we go through the `directories` crate which applies the
//! spec fallbacks (~/.config, ~/.local/share, …) instead of reading env vars
//! directly.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub struct Paths {
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub favorites_dir: PathBuf,
    pub history_db: PathBuf,
    pub inject_lua: PathBuf,
    pub default_db: PathBuf,
    pub recents_file: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        let pd = directories::ProjectDirs::from("", "", "nsql")
            .context("cannot determine config/home directories")?;
        let config_dir = pd.config_dir().to_path_buf();
        let data_dir = pd.data_dir().to_path_buf();
        let state_dir = data_dir.join("state");
        let favorites_dir = data_dir.join("favorites");

        std::fs::create_dir_all(&config_dir).ok();
        std::fs::create_dir_all(&data_dir).ok();
        std::fs::create_dir_all(&state_dir).ok();
        std::fs::create_dir_all(&favorites_dir).ok();
        // Keep all of nsql's dirs owner-only: they hold connection inventories,
        // query history (may contain literal secrets), scratch, and recents.
        for d in [&config_dir, &data_dir, &state_dir, &favorites_dir] {
            crate::util::chmod_private_dir(d);
        }

        Ok(Self {
            config_file: config_dir.join("config.toml"),
            history_db: data_dir.join("history.sqlite"),
            inject_lua: data_dir.join("inject.lua"),
            default_db: data_dir.join("dev.db"),
            recents_file: state_dir.join("recents.toml"),
            favorites_dir,
            state_dir,
        })
    }

    pub fn scratch_for(&self, profile: &str) -> PathBuf {
        self.state_dir.join(format!("scratch-{profile}.sql"))
    }
}

#[derive(Deserialize, Serialize, Default, Debug)]
pub struct Config {
    pub default: Option<String>,
    #[serde(default, rename = "profile")]
    pub profiles: Vec<Profile>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub prod: bool,
    #[serde(default)]
    pub readonly: bool,
    /// Skip recording this profile's queries to history (for sensitive DBs).
    #[serde(default)]
    pub no_history: bool,
}

impl Profile {
    pub fn scheme(&self) -> &str {
        self.url.split(':').next().unwrap_or("")
    }

    /// For sqlite URLs, the filesystem path (or ":memory:").
    pub fn sqlite_target(&self) -> String {
        let rest = self
            .url
            .strip_prefix("sqlite://")
            .or_else(|| self.url.strip_prefix("sqlite:"))
            .unwrap_or(&self.url);
        if rest.is_empty() || rest.contains(":memory:") {
            ":memory:".to_string()
        } else {
            rest.to_string()
        }
    }
}

impl Config {
    pub fn load_or_init(paths: &Paths) -> Result<Self> {
        if paths.config_file.exists() {
            let text = std::fs::read_to_string(&paths.config_file)
                .with_context(|| format!("reading {}", paths.config_file.display()))?;
            let cfg: Config = toml::from_str(&text)
                .with_context(|| format!("parsing {}", paths.config_file.display()))?;
            return Ok(cfg);
        }

        // First run: bootstrap a working local sqlite profile so `nsql -e ...`
        // works out of the box.
        let cfg = Config {
            default: Some("local".to_string()),
            profiles: vec![Profile {
                name: "local".to_string(),
                url: format!("sqlite://{}", paths.default_db.display()),
                prod: false,
                readonly: false,
                no_history: false,
            }],
        };
        cfg.save(paths)?;
        Ok(cfg)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing config")?;
        crate::util::write_private(&paths.config_file, text.as_bytes())
            .with_context(|| format!("writing {}", paths.config_file.display()))?;
        Ok(())
    }

    pub fn select(&self, override_name: Option<&str>) -> Result<Profile> {
        let name = override_name
            .map(|s| s.to_string())
            .or_else(|| self.default.clone())
            .context("no profile selected and no default set (see `nsql connect`)")?;
        self.profiles
            .iter()
            .find(|p| p.name == name)
            .cloned()
            .with_context(|| format!("no profile named `{name}` (try `nsql profiles`)"))
    }

    pub fn upsert(&mut self, p: Profile) {
        if let Some(existing) = self.profiles.iter_mut().find(|x| x.name == p.name) {
            *existing = p;
        } else {
            if self.default.is_none() {
                self.default = Some(p.name.clone());
            }
            self.profiles.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prof(url: &str) -> Profile {
        Profile {
            name: "t".into(),
            url: url.into(),
            prod: false,
            readonly: false,
            no_history: false,
        }
    }

    #[test]
    fn scheme_parsing() {
        assert_eq!(prof("sqlite:///x/y.db").scheme(), "sqlite");
        assert_eq!(prof("postgres://u@h/db").scheme(), "postgres");
    }

    #[test]
    fn sqlite_target_parsing() {
        assert_eq!(prof("sqlite:///home/x/dev.db").sqlite_target(), "/home/x/dev.db");
        assert_eq!(prof("sqlite::memory:").sqlite_target(), ":memory:");
    }
}

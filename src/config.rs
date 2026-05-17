use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ssh: SshSettings,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SshSettings {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub key_path: String,
    #[serde(default)]
    pub root: String,
}

fn config_path() -> Option<PathBuf> {
    let mut path = dirs::config_dir()?;
    path.push("twelf");
    path.push("config.toml");
    Some(path)
}

pub fn load() -> Config {
    let Some(path) = config_path() else { return Config::default() };
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            eprintln!("[twelf] failed to read {}: {e}", path.display());
            return Config::default();
        }
    };
    toml::from_str(&contents).unwrap_or_else(|e| {
        eprintln!("[twelf] failed to parse {}: {e}", path.display());
        Config::default()
    })
}

pub fn save(config: &Config) {
    let Some(path) = config_path() else {
        eprintln!("[twelf] no config dir available; skipping save");
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("[twelf] failed to create {}: {e}", parent.display());
        return;
    }
    let contents = match toml::to_string(config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[twelf] failed to serialize config: {e}");
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, contents) {
        eprintln!("[twelf] failed to write {}: {e}", path.display());
    }
}

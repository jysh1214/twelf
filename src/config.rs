use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ssh: SshSettings,
    /// Saved connections, in the order they were added.
    #[serde(default)]
    pub favorites: Vec<Favorite>,
}

/// A connection worth keeping: everything needed to restore it, plus the label
/// the dialog lists it under. The fields are flat rather than a nested
/// `SshSettings` so each entry stays one readable `[[favorites]]` table that can
/// be hand-edited — renaming one is just editing its `label`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Favorite {
    #[serde(default)]
    pub label: String,
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

impl Favorite {
    /// Whether two entries point at the same folder on the same host. Used to
    /// keep saving the same place twice from adding a duplicate row.
    pub fn same_target(&self, other: &Favorite) -> bool {
        self.host == other.host
            && self.port == other.port
            && self.user == other.user
            && self.root == other.root
    }

    /// The label used when the user has not written one: scp-ish and stable, so
    /// entries differing only by folder still read differently.
    pub fn derive_label(user: &str, host: &str, root: &str) -> String {
        if user.is_empty() {
            format!("{host}:{root}")
        } else {
            format!("{user}@{host}:{root}")
        }
    }
}

/// Add `favorite` unless the same target is already saved. Returns whether the
/// list changed, so the caller only rewrites the config when it did.
pub fn add_favorite(favorites: &mut Vec<Favorite>, favorite: Favorite) -> bool {
    if favorites.iter().any(|f| f.same_target(&favorite)) {
        return false;
    }
    favorites.push(favorite);
    true
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
            crate::log!("failed to read {}: {e}", path.display());
            return Config::default();
        }
    };
    toml::from_str(&contents).unwrap_or_else(|e| {
        crate::log!("failed to parse {}: {e}", path.display());
        Config::default()
    })
}

pub fn save(config: &Config) {
    let Some(path) = config_path() else {
        crate::log!("no config dir available; skipping save");
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        crate::log!("failed to create {}: {e}", parent.display());
        return;
    }
    let contents = match toml::to_string(config) {
        Ok(s) => s,
        Err(e) => {
            crate::log!("failed to serialize config: {e}");
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, contents) {
        crate::log!("failed to write {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn favorite(host: &str, root: &str) -> Favorite {
        Favorite {
            label: Favorite::derive_label("alex", host, root),
            host: host.to_string(),
            port: "22".to_string(),
            user: "alex".to_string(),
            key_path: "~/.ssh/id".to_string(),
            root: root.to_string(),
        }
    }

    #[test]
    fn adding_the_same_target_twice_is_refused() {
        let mut favorites = Vec::new();
        assert!(add_favorite(&mut favorites, favorite("nas", "/photos")));
        // Same host and folder, even relabelled, is the same entry.
        let mut relabelled = favorite("nas", "/photos");
        relabelled.label = "Photos".to_string();
        assert!(!add_favorite(&mut favorites, relabelled));
        assert_eq!(favorites.len(), 1);
        // A different folder on the same host is its own entry.
        assert!(add_favorite(&mut favorites, favorite("nas", "/video")));
        assert_eq!(favorites.len(), 2);
    }

    #[test]
    fn derived_label_reads_like_an_scp_target() {
        assert_eq!(
            Favorite::derive_label("alex", "nas", "/volume1/photos"),
            "alex@nas:/volume1/photos"
        );
        assert_eq!(Favorite::derive_label("", "nas", "/photos"), "nas:/photos");
    }

    #[test]
    fn favorites_survive_a_config_round_trip() {
        let config = Config {
            ssh: SshSettings {
                host: "nas".to_string(),
                port: "22".to_string(),
                user: "alex".to_string(),
                key_path: "~/.ssh/id".to_string(),
                root: "/photos".to_string(),
            },
            favorites: vec![favorite("nas", "/photos"), favorite("nas", "/video")],
        };
        let text = toml::to_string(&config).expect("serialize");
        // One plain table per entry, which is what makes relabelling by hand
        // reasonable — the doc comment on `Favorite` promises this.
        assert_eq!(text.matches("[[favorites]]").count(), 2);
        assert!(text.contains("label = \"alex@nas:/photos\""));
        let parsed: Config = toml::from_str(&text).expect("parse");
        assert_eq!(parsed.favorites, config.favorites);
        assert_eq!(parsed.ssh.root, "/photos");
    }

    #[test]
    fn a_config_written_before_favorites_existed_still_parses() {
        let parsed: Config = toml::from_str("[ssh]\nhost = \"nas\"\n").expect("parse");
        assert_eq!(parsed.ssh.host, "nas");
        assert!(parsed.favorites.is_empty());
    }
}

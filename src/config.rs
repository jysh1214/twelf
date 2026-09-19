use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

/// Where the config lives for this user, if the platform has such a place. The
/// only function here that knows: loading and saving take the path, so tests
/// can use one of their own and never come near the real file.
pub fn config_path() -> Option<PathBuf> {
    let mut path = dirs::config_dir()?;
    path.push("twelf");
    path.push("config.toml");
    Some(path)
}

/// What `load` hands back: the config to run with and, when a config file
/// exists but could not be used, why. Falling back to the defaults without a
/// word is what made a typo expensive — the app started blank, and the next
/// save wrote that blank over every favorite in the file.
#[derive(Default)]
pub struct Loaded {
    pub config: Config,
    pub problem: Option<String>,
}

pub fn load_from(path: &Path) -> Loaded {
    let problem = match std::fs::read_to_string(path) {
        Ok(contents) => match toml::from_str(&contents) {
            Ok(config) => {
                return Loaded {
                    config,
                    problem: None,
                };
            }
            // The message alone: the full Display adds a multi-line excerpt
            // that the one-line status bar cannot show.
            Err(e) => e.message().to_string(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded {
                config: Config::default(),
                problem: None,
            };
        }
        Err(e) => e.to_string(),
    };
    crate::log!("not using {}: {problem}", path.display());
    Loaded {
        config: Config::default(),
        problem: Some(problem),
    }
}

/// Write the config. `set_aside_existing` is for a file `load` could not use:
/// it is renamed to `config.toml.bad` first, so the hand-edits in it outlive
/// the defaults about to replace them.
pub fn save_to(path: &Path, config: &Config, set_aside_existing: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let contents =
        toml::to_string(config).map_err(|e| format!("failed to serialize config: {e}"))?;
    if set_aside_existing && path.exists() {
        let bad = with_suffix(path, ".bad");
        std::fs::rename(path, &bad).map_err(|e| {
            format!(
                "failed to keep {} as {}: {e}",
                path.display(),
                bad.display()
            )
        })?;
    }
    // Written beside the target and renamed over it, so a crash or a full disk
    // mid-write leaves the old file rather than a truncated one — which the next
    // start would fail to parse.
    let tmp = with_suffix(path, ".tmp");
    std::fs::write(&tmp, contents)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("failed to write {}: {e}", path.display())
        })
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
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

    fn sample() -> Config {
        Config {
            ssh: SshSettings::default(),
            favorites: vec![favorite("nas", "/photos")],
        }
    }

    #[test]
    fn a_missing_file_is_a_fresh_start_not_a_problem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = load_from(&dir.path().join("config.toml"));
        assert!(loaded.problem.is_none());
        assert!(loaded.config.favorites.is_empty());
    }

    #[test]
    fn an_unusable_file_is_reported_and_outlives_the_next_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        // A hand-edit gone wrong: the port as a number instead of a string.
        let hand_edited = "[[favorites]]\nlabel = \"Photos\"\nhost = \"nas\"\nport = 2222\n";
        std::fs::write(&path, hand_edited).unwrap();

        let loaded = load_from(&path);
        assert!(loaded.problem.is_some());
        assert!(loaded.config.favorites.is_empty());

        // Saving the defaults it fell back to must not cost the user the file.
        save_to(&path, &loaded.config, true).expect("save");
        assert_eq!(
            std::fs::read_to_string(with_suffix(&path, ".bad")).unwrap(),
            hand_edited
        );
        assert!(load_from(&path).problem.is_none());
    }

    #[test]
    fn save_replaces_the_file_whole_and_leaves_no_staging_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("twelf").join("config.toml");
        save_to(&path, &sample(), false).expect("first save creates the directory");
        save_to(&path, &Config::default(), false).expect("second save replaces");
        assert!(load_from(&path).config.favorites.is_empty());
        assert!(!with_suffix(&path, ".tmp").exists());
        // Nothing was unusable, so nothing is set aside.
        assert!(!with_suffix(&path, ".bad").exists());
    }

    #[test]
    fn a_failed_save_is_an_error_and_keeps_the_old_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        save_to(&path, &sample(), false).expect("save");
        // A directory squatting on the staging name makes the write fail.
        std::fs::create_dir(with_suffix(&path, ".tmp")).unwrap();
        assert!(save_to(&path, &Config::default(), false).is_err());
        assert_eq!(load_from(&path).config.favorites, sample().favorites);
    }

    #[test]
    fn favorites_differing_in_any_connection_field_are_distinct() {
        let mut favorites = vec![favorite("nas", "/photos")];
        // Same folder path, different machine — an entirely different place.
        assert!(add_favorite(&mut favorites, favorite("backup", "/photos")));
        // Same machine and folder, different account.
        let mut other_user = favorite("nas", "/photos");
        other_user.user = "root".to_string();
        assert!(add_favorite(&mut favorites, other_user));
        // Same machine and folder, different port.
        let mut other_port = favorite("nas", "/photos");
        other_port.port = "2222".to_string();
        assert!(add_favorite(&mut favorites, other_port));
        assert_eq!(favorites.len(), 4);
    }
}

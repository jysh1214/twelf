use crate::config;
use russh::client::{self, Handler};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, load_secret_key};
use russh_sftp::client::SftpSession;
use std::path::PathBuf;
use std::sync::Arc;

pub enum SshState {
    Disconnected,
    Connecting,
    Connected {
        #[allow(dead_code)]
        session: Arc<SftpSession>,
        info: ConnInfo,
    },
    Failed {
        error: String,
    },
}

pub struct ConnInfo {
    pub host: String,
    pub port: u16,
    pub user: String,
    #[allow(dead_code)]
    pub root: String,
    pub key_path: String,
}

pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: String,
    pub root: String,
}

pub type ConnectResult = Result<(Arc<SftpSession>, ConnInfo), String>;

pub struct ConnectDialog {
    pub open: bool,
    pub host: String,
    pub port: String,
    pub user: String,
    pub key_path: String,
    pub root: String,
}

impl ConnectDialog {
    pub fn from_settings(s: config::SshSettings) -> Self {
        let port = if s.port.is_empty() {
            "22".to_string()
        } else {
            s.port
        };
        Self {
            open: false,
            host: s.host,
            port,
            user: s.user,
            key_path: s.key_path,
            root: s.root,
        }
    }

    pub fn to_settings(&self) -> config::SshSettings {
        config::SshSettings {
            host: self.host.clone(),
            port: self.port.clone(),
            user: self.user.clone(),
            key_path: self.key_path.clone(),
            root: self.root.clone(),
        }
    }

    /// Snapshot the dialog as a saveable favorite, labelled from its own fields.
    pub fn to_favorite(&self) -> config::Favorite {
        config::Favorite {
            label: config::Favorite::derive_label(&self.user, &self.host, &self.root),
            host: self.host.clone(),
            port: self.port.clone(),
            user: self.user.clone(),
            key_path: self.key_path.clone(),
            root: self.root.clone(),
        }
    }

    /// Fill the dialog from a saved favorite, ready to connect.
    pub fn load_favorite(&mut self, favorite: &config::Favorite) {
        self.host = favorite.host.clone();
        self.port = favorite.port.clone();
        self.user = favorite.user.clone();
        self.key_path = favorite.key_path.clone();
        self.root = favorite.root.clone();
    }
}

// MVP shortcut: accept any server key. Tightening to TOFU / known-hosts is deferred.
struct AcceptAnyHostKey;

impl Handler for AcceptAnyHostKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub async fn connect(req: ConnectRequest) -> ConnectResult {
    let key_path = expand_home(&req.key_path);
    let private_key = load_secret_key(&key_path, None).map_err(stringify)?;
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, (req.host.as_str(), req.port), AcceptAnyHostKey)
        .await
        .map_err(stringify)?;
    let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(private_key), None);
    let auth = session
        .authenticate_publickey(req.user.as_str(), key_with_alg)
        .await
        .map_err(stringify)?;
    if !auth.success() {
        return Err("authentication failed".to_string());
    }
    let channel = session
        .channel_open_session()
        .await
        .map_err(stringify)?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(stringify)?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(stringify)?;
    // Resolve the root on the server (SSH_FXP_REALPATH). The sftp:// URIs are
    // built by concatenating a path onto "sftp://{host}", which assumes a
    // leading slash — a relative root like "photos" would splice into the host
    // ("sftp://nasphotos/a.jpg") and the loader, splitting at the first slash,
    // would read back "/a.jpg": a different file from the one the tree listed.
    // Resolving keeps the browsed directory identical while making the URI
    // well-formed. A server that refuses leaves the entered value as-is.
    let root = sftp
        .canonicalize(req.root.clone())
        .await
        .unwrap_or_else(|e| {
            crate::log!("could not resolve root {:?}: {e}", req.root);
            req.root.clone()
        });
    Ok((
        Arc::new(sftp),
        ConnInfo {
            host: req.host,
            port: req.port,
            user: req.user,
            root,
            key_path: req.key_path,
        },
    ))
}

pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        let mut p = PathBuf::from(home);
        p.push(rest);
        return p;
    }
    PathBuf::from(path)
}

fn stringify<E: std::error::Error>(err: E) -> String {
    let mut s = err.to_string();
    let mut src = err.source();
    while let Some(inner) = src {
        s.push_str(": ");
        s.push_str(&inner.to_string());
        src = inner.source();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_only_expands_a_tilde_slash_prefix() {
        // Absolute and bare-relative paths pass through untouched.
        assert_eq!(expand_home("/etc/ssh/key"), PathBuf::from("/etc/ssh/key"));
        assert_eq!(expand_home("key"), PathBuf::from("key"));
        // A leading "~" without a slash is not expansion syntax — left as-is.
        assert_eq!(expand_home("~user/key"), PathBuf::from("~user/key"));
        // "~/" expands against $HOME.
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(expand_home("~/.ssh/id"), PathBuf::from(home).join(".ssh/id"));
        }
    }

    #[test]
    fn dialog_defaults_blank_port_to_22() {
        let blank = config::SshSettings::default();
        assert_eq!(ConnectDialog::from_settings(blank).port, "22");
        let set = config::SshSettings { port: "2222".to_string(), ..Default::default() };
        assert_eq!(ConnectDialog::from_settings(set).port, "2222");
    }
}

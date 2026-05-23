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
    Ok((
        Arc::new(sftp),
        ConnInfo {
            host: req.host,
            port: req.port,
            user: req.user,
            root: req.root,
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

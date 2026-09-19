use crate::config;
use eframe::egui;
use russh::client::{self, Handler};
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, load_secret_key};
use russh_sftp::client::SftpSession;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub enum SshState {
    Disconnected,
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

/// How long a connection attempt may take. russh sets no deadline of its own: a
/// black-holed address held "Connecting…" for the OS's SYN timeout of about two
/// minutes, and a host that accepts TCP but never sends a banner held it for
/// good.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// A connection attempt in flight. It is not an `SshState`: whatever session is
/// current stays current, and usable, until this one succeeds — a mistyped host
/// should cost nothing but the wait. Dropping the attempt abandons it, which is
/// both the Cancel button and what a second Connect click does to the first.
pub struct ConnectAttempt {
    /// `user@host:port`, for the menu bar and a failure message.
    pub target: String,
    rx: tokio::sync::mpsc::Receiver<ConnectResult>,
    task: tokio::task::JoinHandle<()>,
}

impl ConnectAttempt {
    pub fn spawn(
        req: ConnectRequest,
        runtime: &tokio::runtime::Runtime,
        ctx: &egui::Context,
    ) -> Self {
        let target = format!("{}@{}:{}", req.user, req.host, req.port);
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let ctx = ctx.clone();
        let task = runtime.spawn(async move {
            let result = connect(req).await;
            let _ = tx.send(result).await;
            ctx.request_repaint();
        });
        Self { target, rx, task }
    }

    /// The outcome, once it is in (non-blocking).
    pub fn poll(&mut self) -> Option<ConnectResult> {
        self.rx.try_recv().ok()
    }
}

impl Drop for ConnectAttempt {
    fn drop(&mut self) {
        // Closes the socket with it, rather than leaving a handshake nobody is
        // waiting for to run on.
        self.task.abort();
    }
}

pub struct ConnectDialog {
    pub open: bool,
    pub host: String,
    pub port: String,
    /// Why the last Connect click was refused, shown under the fields until
    /// the port is edited.
    pub port_error: Option<String>,
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
            port_error: None,
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
        self.port_error = None;
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
    match tokio::time::timeout(CONNECT_TIMEOUT, establish(req)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "no connection after {} s",
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

async fn establish(req: ConnectRequest) -> ConnectResult {
    let key_path = expand_home(&req.key_path);
    let private_key = load_secret_key(&key_path, None).map_err(stringify)?;
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, (req.host.as_str(), req.port), AcceptAnyHostKey)
        .await
        .map_err(stringify)?;
    // An RSA key has to be told which hash to sign with, and `None` means the
    // legacy SHA-1 `ssh-rsa` that OpenSSH 8.8+ refuses — so ask the server what
    // it takes. Only RSA pays for the question: it can wait up to a second for
    // the server's extension info, and every other key type ignores the answer.
    let hash_alg = if private_key.algorithm().is_rsa() {
        session
            .best_supported_rsa_hash()
            .await
            .map_err(stringify)?
            .flatten()
    } else {
        None
    };
    let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(private_key), hash_alg);
    let auth = session
        .authenticate_publickey(req.user.as_str(), key_with_alg)
        .await
        .map_err(stringify)?;
    if !auth.success() {
        return Err("authentication failed".to_string());
    }
    let channel = session.channel_open_session().await.map_err(stringify)?;
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

/// The port typed into the dialog. Blank means 22, as `from_settings` fills
/// it in; surrounding whitespace (a pasted "2222 ") is forgiven. Anything else
/// that is not a port is an error rather than a quiet fall-back to 22: that
/// offered the user's key to a different sshd from the one they asked for.
pub fn parse_port(text: &str) -> Result<u16, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(22);
    }
    match text.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(format!(
            "Port must be a number from 1 to 65535, not {text:?}"
        )),
    }
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
            assert_eq!(
                expand_home("~/.ssh/id"),
                PathBuf::from(home).join(".ssh/id")
            );
        }
    }

    #[test]
    fn parse_port_refuses_what_is_not_a_port() {
        assert_eq!(parse_port("2222"), Ok(2222));
        assert_eq!(parse_port("65535"), Ok(65535));
        // Blank is the default port; pasted whitespace is not the user's fault.
        assert_eq!(parse_port(""), Ok(22));
        assert_eq!(parse_port(" 2222 "), Ok(2222));
        // None of these may turn into 22.
        for bad in ["0", "65536", "22222222", "ssh", "22a", "-22"] {
            assert!(parse_port(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn an_attempt_names_its_target_and_hands_back_its_outcome() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let ctx = egui::Context::default();
        // Fails at the key file, before anything touches the network.
        let req = ConnectRequest {
            host: "nas".to_string(),
            port: 2222,
            user: "alex".to_string(),
            key_path: "/nonexistent/twelf-test-key".to_string(),
            root: "/photos".to_string(),
        };
        let mut attempt = ConnectAttempt::spawn(req, &rt, &ctx);
        assert_eq!(attempt.target, "alex@nas:2222");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            if let Some(outcome) = attempt.poll() {
                break outcome;
            }
            assert!(std::time::Instant::now() < deadline, "no outcome within 5s");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(outcome.is_err());
    }

    #[test]
    fn dialog_defaults_blank_port_to_22() {
        let blank = config::SshSettings::default();
        assert_eq!(ConnectDialog::from_settings(blank).port, "22");
        let set = config::SshSettings {
            port: "2222".to_string(),
            ..Default::default()
        };
        assert_eq!(ConnectDialog::from_settings(set).port, "2222");
    }
}

use crate::config;
use eframe::egui;
use russh::client::{self, DisconnectReason, Handler};
use russh::keys::known_hosts::{known_host_keys_path, learn_known_hosts_path};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, load_secret_key};
use russh_sftp::client::SftpSession;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub enum SshState {
    Disconnected,
    Connected {
        #[allow(dead_code)]
        session: Arc<SftpSession>,
        info: ConnInfo,
        /// Where this session reports its own end; see `SessionEnd`.
        ended: SessionEnd,
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

#[derive(Clone)]
pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: String,
    pub root: String,
}

impl ConnectRequest {
    /// `user@host:port`, for the menu bar and a failure message.
    pub fn target(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }
}

pub enum ConnectError {
    /// The server's key is in no known_hosts file. Not a failure to report but
    /// a question to put to the user, who may trust the key and try again.
    UnknownHostKey {
        key: PublicKey,
    },
    Other(String),
}

impl From<String> for ConnectError {
    fn from(message: String) -> Self {
        ConnectError::Other(message)
    }
}

pub type ConnectResult = Result<(Arc<SftpSession>, ConnInfo, SessionEnd), ConnectError>;

/// How a session tells the app that it is over, and why. Nothing used to: the
/// russh handle was dropped once SFTP was up, so after a server restart or a
/// resume from sleep the menu bar went on saying "Connected" while every folder
/// click stalled for ten seconds and every image went into a 30 s back-off.
///
/// One per connection, so a session the app has already left reports into a
/// slot nobody reads rather than into its successor's.
#[derive(Clone, Default)]
pub struct SessionEnd(Arc<Mutex<Option<String>>>);

impl SessionEnd {
    /// Only the first reason is kept: what follows it is the fallout.
    fn record(&self, reason: String) {
        self.0.lock().unwrap().get_or_insert(reason);
    }

    /// Why the session ended, handed over once; `None` while it is alive.
    pub fn take(&self) -> Option<String> {
        self.0.lock().unwrap().take()
    }
}

/// How often an otherwise idle session pings the server, and how many pings may
/// go unanswered. A link that dies silently — a NAT table entry expiring, a
/// cable — is noticed within about a minute instead of never, and the traffic
/// keeps such a NAT entry from expiring in the first place.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEPALIVE_MAX: usize = 3;

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
    /// What is being attempted: named in the menu bar, and needed again if the
    /// user has to be asked about the host key first.
    pub request: ConnectRequest,
    rx: tokio::sync::mpsc::Receiver<ConnectResult>,
    task: tokio::task::JoinHandle<()>,
}

impl ConnectAttempt {
    pub fn spawn(
        req: ConnectRequest,
        runtime: &tokio::runtime::Runtime,
        ctx: &egui::Context,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let ctx = ctx.clone();
        let request = req.clone();
        let task = runtime.spawn(async move {
            let result = connect(req, ctx.clone()).await;
            let _ = tx.send(result).await;
            ctx.request_repaint();
        });
        Self { request, rx, task }
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
    /// Why the last Connect or Save current click was refused, shown under the
    /// fields until the host or the port is edited.
    pub error: Option<String>,
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
            error: None,
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

    /// The host and port typed, tidied and checked: what both Connect and Save
    /// current need before they do anything.
    fn target(&self) -> Result<(&str, u16), String> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err("Enter a HostName first".to_string());
        }
        Ok((host, parse_port(&self.port)?))
    }

    /// What Connect should attempt. Refused, with the reason for the dialog to
    /// show, when there is no host or the port does not parse: a blank host went
    /// all the way to a connection attempt and came back as a resolver error in
    /// the menu bar, with the dialog already closed.
    pub fn to_request(&self) -> Result<ConnectRequest, String> {
        let (host, port) = self.target()?;
        Ok(ConnectRequest {
            host: host.to_string(),
            port,
            user: self.user.trim().to_string(),
            key_path: self.key_path.clone(),
            root: self.root.clone(),
        })
    }

    /// Snapshot the dialog as a saveable favorite, labelled from its own fields.
    /// Refused when it names nowhere to connect to: with the fields blank, Save
    /// current used to store a favorite labelled ":", and one with a port that
    /// does not parse could be saved but never used. Host, user and port are
    /// stored tidied, so the same place typed with a stray space is recognised
    /// as already saved.
    pub fn to_favorite(&self) -> Result<config::Favorite, String> {
        let (host, port) = self.target()?;
        let user = self.user.trim();
        Ok(config::Favorite {
            label: config::Favorite::derive_label(user, host, &self.root),
            host: host.to_string(),
            port: port.to_string(),
            user: user.to_string(),
            key_path: self.key_path.clone(),
            root: self.root.clone(),
        })
    }

    /// Fill the dialog from a saved favorite, ready to connect.
    pub fn load_favorite(&mut self, favorite: &config::Favorite) {
        self.host = favorite.host.clone();
        self.port = favorite.port.clone();
        self.error = None;
        self.user = favorite.user.clone();
        self.key_path = favorite.key_path.clone();
        self.root = favorite.root.clone();
    }
}

/// What the known_hosts files say about a server's key.
#[derive(Debug, PartialEq)]
enum HostKeyVerdict {
    Trusted,
    /// No file has a key of this type for the host.
    Unknown,
    /// A file has a different key of the same type: the server was reinstalled,
    /// or something is answering in its place.
    Changed {
        file: PathBuf,
        line: usize,
    },
}

/// The files a server key is looked up in: the user's own OpenSSH list, so a
/// host they have already ssh'd into needs no asking, then the keys accepted
/// from inside the app. Only the second is ever written to.
fn known_hosts_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        files.push(PathBuf::from(home).join(".ssh").join("known_hosts"));
    }
    files.extend(app_known_hosts());
    files
}

fn app_known_hosts() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("twelf").join("known_hosts"))
}

/// Judge `key` against every file as one list, the way OpenSSH reads its
/// `UserKnownHostsFile`s: a match anywhere is trust. russh's own
/// `check_known_hosts_path` gives up at the first line that differs, so a host
/// with its old and its new key both recorded would be refused.
fn judge_host_key(host: &str, port: u16, key: &PublicKey, files: &[PathBuf]) -> HostKeyVerdict {
    let mut verdict = HostKeyVerdict::Unknown;
    for file in files {
        // An unreadable file, or one whose line for this host does not parse,
        // vouches for nothing.
        let Ok(recorded) = known_host_keys_path(host, port, file) else {
            crate::log!("could not read host keys from {}", file.display());
            continue;
        };
        if recorded.iter().any(|(_, k)| k == key) {
            return HostKeyVerdict::Trusted;
        }
        if verdict == HostKeyVerdict::Unknown
            && let Some((line, _)) = recorded
                .iter()
                .find(|(_, k)| k.algorithm() == key.algorithm())
        {
            verdict = HostKeyVerdict::Changed {
                file: file.clone(),
                line: *line,
            };
        }
    }
    verdict
}

/// How a key is shown to the user: its type and SHA-256 fingerprint, the form
/// `ssh-keygen -lf` prints for comparison.
pub fn fingerprint(key: &PublicKey) -> String {
    format!("{} {}", key.algorithm(), key.fingerprint(HashAlg::Sha256))
}

/// Record `key` as the one to expect from `host`, in the app's own list.
pub fn trust_host_key(host: &str, port: u16, key: &PublicKey) -> Result<(), String> {
    let file = app_known_hosts().ok_or_else(|| "no config directory available".to_string())?;
    learn_known_hosts_path(host, port, key, &file)
        .map_err(|e| format!("failed to write {}: {e}", file.display()))
}

/// Accepts the server only when its key is already trusted. Anything else is
/// turned down and the reason left in `rejected`, since russh reports every
/// refusal as the same "unknown key".
///
/// Until now every key was accepted: whoever answered on the port was taken for
/// the server, and could feed the image and video decoders whatever it liked.
struct VerifyHostKey {
    host: String,
    port: u16,
    files: Vec<PathBuf>,
    rejected: Arc<Mutex<Option<(HostKeyVerdict, PublicKey)>>>,
    /// Told when the session ends, with the app woken up to look.
    ended: SessionEnd,
    ctx: egui::Context,
}

impl Handler for VerifyHostKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let verdict = judge_host_key(&self.host, self.port, server_public_key, &self.files);
        if verdict == HostKeyVerdict::Trusted {
            return Ok(true);
        }
        *self.rejected.lock().unwrap() = Some((verdict, server_public_key.clone()));
        Ok(false)
    }

    /// russh calls this however the session ends: the server saying goodbye, an
    /// I/O error, keepalives going unanswered — or the app itself letting go of
    /// the session, in which case nobody is reading `ended` any more.
    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let (why, outcome) = match reason {
            DisconnectReason::ReceivedDisconnect(info) if info.message.is_empty() => {
                ("the server closed the connection".to_string(), Ok(()))
            }
            DisconnectReason::ReceivedDisconnect(info) => (
                format!("the server closed the connection: {}", info.message),
                Ok(()),
            ),
            // Handed back, as the default implementation does, so whoever still
            // holds the handle sees the same error.
            DisconnectReason::Error(e) => {
                crate::log!("session ended: {e}");
                (describe_session_error(&e), Err(e))
            }
        };
        self.ended.record(why);
        self.ctx.request_repaint();
        outcome
    }
}

/// Why a session ended, in words for the menu bar. russh's own are written for
/// its callers: a server going away surfaced as "early eof", and the peer
/// falling silent as "Keepalive timeout".
fn describe_session_error(error: &russh::Error) -> String {
    use std::io::ErrorKind;
    match error {
        russh::Error::KeepaliveTimeout | russh::Error::InactivityTimeout => {
            "the server stopped responding".to_string()
        }
        russh::Error::HUP | russh::Error::Disconnect => {
            "the server closed the connection".to_string()
        }
        russh::Error::IO(io) => match io.kind() {
            ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe => "the connection was closed".to_string(),
            ErrorKind::TimedOut => "the connection timed out".to_string(),
            _ => io.to_string(),
        },
        other => other.to_string(),
    }
}

pub async fn connect(req: ConnectRequest, ctx: egui::Context) -> ConnectResult {
    match tokio::time::timeout(CONNECT_TIMEOUT, establish(req, ctx)).await {
        Ok(result) => result,
        Err(_) => Err(ConnectError::Other(format!(
            "no connection after {} s",
            CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

async fn establish(req: ConnectRequest, ctx: egui::Context) -> ConnectResult {
    let key_path = expand_home(&req.key_path);
    let private_key = load_secret_key(&key_path, None).map_err(stringify)?;
    let config = Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        ..Default::default()
    });
    let ended = SessionEnd::default();
    let rejected = Arc::new(Mutex::new(None));
    let handler = VerifyHostKey {
        host: req.host.clone(),
        port: req.port,
        files: known_hosts_files(),
        rejected: rejected.clone(),
        ended: ended.clone(),
        ctx,
    };
    let connected = client::connect(config, (req.host.as_str(), req.port), handler).await;
    let mut session = match connected {
        Ok(session) => session,
        Err(e) => {
            return Err(match rejected.lock().unwrap().take() {
                Some((HostKeyVerdict::Changed { file, line }, key)) => {
                    ConnectError::Other(format!(
                        "the host key of {}:{} ({}) does not match line {line} of {}. \
                         Refusing to connect; if the server's key really changed, \
                         remove that line",
                        req.host,
                        req.port,
                        fingerprint(&key),
                        file.display()
                    ))
                }
                Some((_, key)) => ConnectError::UnknownHostKey { key },
                None => ConnectError::Other(stringify(e)),
            });
        }
    };
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
        return Err("authentication failed".to_string().into());
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
        ended,
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

    // Throwaway keys generated for these tests; the private halves are gone.
    const ED25519_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDas5exaMxO62/EkqANCSvgMPxGV3gACEVvq2yzyf7p+";
    const ED25519_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICZpAzz8DgzrQtWCSXpCQ1L+SInUTFOdyH9OPYDRTIAD";
    const ECDSA_C: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBJFHSBh9Pslp36fIcP/hzmBYCk4IBgFtXrw5qDvkDCIHwhVRB5NPH7pU4sVGNoe4EbehUoRLOogtk/umCBDixss=";

    fn key(openssh: &str) -> PublicKey {
        PublicKey::from_openssh(openssh).expect("test key")
    }

    #[test]
    fn a_host_is_trusted_only_once_its_key_is_on_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = vec![dir.path().join("known_hosts")];
        // No file at all yet: nothing vouches for anyone.
        assert_eq!(
            judge_host_key("nas", 22, &key(ED25519_A), &files),
            HostKeyVerdict::Unknown
        );
        learn_known_hosts_path("nas", 22, &key(ED25519_A), &files[0]).expect("learn");
        assert_eq!(
            judge_host_key("nas", 22, &key(ED25519_A), &files),
            HostKeyVerdict::Trusted
        );
        // The record is for that host and that port only.
        assert_eq!(
            judge_host_key("backup", 22, &key(ED25519_A), &files),
            HostKeyVerdict::Unknown
        );
        assert_eq!(
            judge_host_key("nas", 2222, &key(ED25519_A), &files),
            HostKeyVerdict::Unknown
        );
    }

    #[test]
    fn a_different_key_of_the_same_type_is_a_changed_host() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = vec![dir.path().join("known_hosts")];
        learn_known_hosts_path("nas", 22, &key(ED25519_A), &files[0]).expect("learn");
        assert_eq!(
            judge_host_key("nas", 22, &key(ED25519_B), &files),
            // Line 2: russh starts a new file with a blank line.
            HostKeyVerdict::Changed {
                file: files[0].clone(),
                line: 2
            }
        );
        // A key type the file has never seen for this host is merely unknown.
        assert_eq!(
            judge_host_key("nas", 22, &key(ECDSA_C), &files),
            HostKeyVerdict::Unknown
        );
        // Old and new key both on record, as after a rotation: still trusted.
        learn_known_hosts_path("nas", 22, &key(ED25519_B), &files[0]).expect("learn");
        assert_eq!(
            judge_host_key("nas", 22, &key(ED25519_B), &files),
            HostKeyVerdict::Trusted
        );
    }

    #[test]
    fn the_files_are_read_as_one_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = vec![dir.path().join("openssh"), dir.path().join("twelf")];
        // The user's own file has an old key; the app's has the current one.
        learn_known_hosts_path("nas", 22, &key(ED25519_A), &files[0]).expect("learn");
        learn_known_hosts_path("nas", 22, &key(ED25519_B), &files[1]).expect("learn");
        assert_eq!(
            judge_host_key("nas", 22, &key(ED25519_B), &files),
            HostKeyVerdict::Trusted
        );
    }

    #[test]
    fn a_session_error_is_put_in_the_users_words() {
        use std::io::{Error, ErrorKind};
        // What killing the server side of a session produced, verbatim.
        let eof = russh::Error::IO(Error::new(ErrorKind::UnexpectedEof, "early eof"));
        assert_eq!(describe_session_error(&eof), "the connection was closed");
        let reset = russh::Error::IO(Error::from(ErrorKind::ConnectionReset));
        assert_eq!(describe_session_error(&reset), "the connection was closed");
        assert_eq!(
            describe_session_error(&russh::Error::KeepaliveTimeout),
            "the server stopped responding"
        );
        assert_eq!(
            describe_session_error(&russh::Error::HUP),
            "the server closed the connection"
        );
        // Anything unforeseen keeps russh's own wording rather than none.
        assert_eq!(
            describe_session_error(&russh::Error::DecryptionError),
            "Failed to decrypt a packet"
        );
    }

    #[test]
    fn a_session_reports_its_end_once_with_the_first_reason() {
        let ended = SessionEnd::default();
        let seen_by_the_app = ended.clone();
        assert_eq!(seen_by_the_app.take(), None);
        ended.record("keepalive timeout".to_string());
        // What breaks next is fallout, not the cause.
        ended.record("channel closed".to_string());
        assert_eq!(seen_by_the_app.take().as_deref(), Some("keepalive timeout"));
        assert_eq!(seen_by_the_app.take(), None);
    }

    #[test]
    fn a_fingerprint_reads_like_ssh_keygen_prints_it() {
        let shown = fingerprint(&key(ED25519_A));
        assert!(shown.starts_with("ssh-ed25519 SHA256:"), "{shown}");
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
        assert_eq!(attempt.request.target(), "alex@nas:2222");
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
    fn connect_needs_a_host_and_a_port_that_parses() {
        let mut dialog = ConnectDialog::from_settings(config::SshSettings::default());
        // Blank host: nothing to attempt, and the dialog should say so.
        assert!(dialog.to_request().is_err());
        dialog.host = " nas ".to_string();
        dialog.user = " alex".to_string();
        dialog.port = "2222 ".to_string();
        let request = dialog.to_request().expect("valid");
        assert_eq!(request.target(), "alex@nas:2222");
        dialog.port = "ssh".to_string();
        assert!(dialog.to_request().is_err());
    }

    #[test]
    fn a_favorite_needs_somewhere_to_connect_to() {
        let mut dialog = ConnectDialog::from_settings(config::SshSettings::default());
        // Every field blank, as on first start.
        assert!(dialog.to_favorite().is_err());
        dialog.host = "   ".to_string();
        assert!(dialog.to_favorite().is_err());

        dialog.host = " nas ".to_string();
        dialog.user = "alex ".to_string();
        dialog.port = String::new();
        dialog.root = "/photos".to_string();
        let favorite = dialog.to_favorite().expect("a host is enough");
        // Tidied, with the blank port spelled out.
        assert_eq!(favorite.host, "nas");
        assert_eq!(favorite.user, "alex");
        assert_eq!(favorite.port, "22");
        assert_eq!(favorite.label, "alex@nas:/photos");

        // A port that could never connect is not worth saving either.
        dialog.port = "99999".to_string();
        assert!(dialog.to_favorite().is_err());
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

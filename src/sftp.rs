//! SFTP (SSH file transfer) connections.
//!
//! russh is async; each connection owns a small tokio runtime and the
//! blocking `Remote` methods drive it with `block_on` from worker threads.

use crate::fsutil::Entry;
use crate::remote::{self, ConnectError, Pump, Remote, Site};
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::FileAttributes;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(20);

static KNOWN_HOSTS_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Tests use their own known_hosts file instead of ~/.ssh/known_hosts.
#[cfg(test)]
pub fn set_known_hosts_path(p: PathBuf) {
    *KNOWN_HOSTS_OVERRIDE.lock().unwrap() = Some(p);
}

fn known_hosts_path() -> PathBuf {
    if let Some(p) = KNOWN_HOSTS_OVERRIDE.lock().unwrap().clone() {
        return p;
    }
    dirs::home_dir().unwrap_or_default().join(".ssh").join("known_hosts")
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

// ----------------------------------------------------------------------------
// Host key verification
// ----------------------------------------------------------------------------

enum HostKeyIssue {
    Unknown(String),
    Changed(String),
}

struct Client {
    host: String,
    port: u16,
    /// Fingerprint the user accepted for a so far unknown host.
    trusted: Option<String>,
    issue: Arc<Mutex<Option<HostKeyIssue>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.issue.lock().unwrap() =
                Some(HostKeyIssue::Changed(l!("Der Server nutzt ein SSH-Zertifikat – wird nicht unterstützt", "The server uses an SSH certificate – not supported").into()));
            return Ok(false);
        };
        let path = known_hosts_path();
        let fp = fingerprint(key);
        match russh::keys::check_known_hosts_path(&self.host, self.port, key, &path) {
            Ok(true) => Ok(true),
            Ok(false) if self.trusted.as_deref() == Some(fp.as_str()) => {
                if let Err(e) = russh::keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, &path) {
                    log_warn(&format!("known_hosts konnte nicht geschrieben werden: {e}"));
                }
                Ok(true)
            }
            Ok(false) => {
                *self.issue.lock().unwrap() = Some(HostKeyIssue::Unknown(fp));
                Ok(false)
            }
            Err(russh::keys::Error::KeyChanged { line }) => {
                *self.issue.lock().unwrap() = Some(HostKeyIssue::Changed(format!(
                    "WARNUNG: Der Host-Schlüssel von {} hat sich geändert (known_hosts Zeile {line})! \
                     Möglicherweise ein Angriff (Man-in-the-Middle). Verbindung abgebrochen. Neuer Fingerabdruck: {fp}",
                    self.host
                )));
                Ok(false)
            }
            Err(e) => {
                *self.issue.lock().unwrap() = Some(HostKeyIssue::Changed(format!("known_hosts: {e}")));
                Ok(false)
            }
        }
    }
}

fn log_warn(msg: &str) {
    eprintln!("MieryCommander: {msg}");
}

// ----------------------------------------------------------------------------
// Connection
// ----------------------------------------------------------------------------

struct Session {
    handle: client::Handle<Client>,
    sftp: Arc<SftpSession>,
}

pub struct SftpConn {
    id: usize,
    site: Site,
    password: String,
    home: String,
    rt: Option<tokio::runtime::Runtime>,
    session: Mutex<Option<Arc<Session>>>,
}

impl Drop for SftpConn {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn sftp_err(e: SftpError) -> String {
    match e {
        SftpError::Status(s) => {
            let msg = s.error_message.trim().to_string();
            if msg.is_empty() { format!("{:?}", s.status_code) } else { msg }
        }
        other => other.to_string(),
    }
}

fn is_connection_error(e: &SftpError) -> bool {
    matches!(e, SftpError::IO(_) | SftpError::Timeout | SftpError::UnexpectedBehavior(_))
}

async fn authenticate(handle: &mut client::Handle<Client>, site: &Site, password: &str) -> Result<(), String> {
    let user = site.user.trim().to_string();
    let rsa_hash = handle.best_supported_rsa_hash().await.ok().flatten().flatten();
    let mut tried = Vec::new();

    let try_key = async |handle: &mut client::Handle<Client>, key: russh::keys::PrivateKey| -> bool {
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
        handle
            .authenticate_publickey(user.clone(), key)
            .await
            .map(|r| r.success())
            .unwrap_or(false)
    };

    // 1. Explicitly configured key file (password field = passphrase).
    let key_file = site.key_file.trim();
    if !key_file.is_empty() {
        let path = crate::panel::expand_tilde(key_file);
        let pass = (!password.is_empty()).then_some(password);
        let key = russh::keys::load_secret_key(&path, pass)
            .map_err(|e| lf!("Schlüssel {path} konnte nicht geladen werden: {e}", "Could not load key {path}: {e}"))?;
        if try_key(handle, key).await {
            return Ok(());
        }
        tried.push(l!("Schlüsseldatei", "key file"));
    }

    if site.auto_keys {
        // 2. SSH agent.
        #[cfg(unix)]
        if let Ok(mut agent) = russh::keys::agent::client::AgentClient::connect_env().await {
            for id in agent.request_identities().await.unwrap_or_default() {
                if let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = id {
                    let ok = handle
                        .authenticate_publickey_with(user.clone(), key, rsa_hash, &mut agent)
                        .await
                        .map(|r| r.success())
                        .unwrap_or(false);
                    if ok {
                        return Ok(());
                    }
                }
            }
            tried.push("SSH-Agent");
        }
        // 3. Default keys in ~/.ssh (unencrypted, or encrypted with the password as passphrase).
        if let Some(home) = dirs::home_dir() {
            for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
                let p = home.join(".ssh").join(name);
                if !p.is_file() {
                    continue;
                }
                let key = russh::keys::load_secret_key(&p, None)
                    .or_else(|_| russh::keys::load_secret_key(&p, (!password.is_empty()).then_some(password)));
                if let Ok(key) = key
                    && try_key(handle, key).await
                {
                    return Ok(());
                }
                tried.push(l!("~/.ssh-Schlüssel", "~/.ssh keys"));
            }
        }
    }

    // 4. Password.
    if !password.is_empty() {
        let ok = handle
            .authenticate_password(user.clone(), password)
            .await
            .map_err(|e| e.to_string())?
            .success();
        if ok {
            return Ok(());
        }
        tried.push(l!("Passwort", "Password"));
    }
    tried.dedup();
    if tried.is_empty() {
        Err(l!("Anmeldung fehlgeschlagen: kein Passwort oder Schlüssel angegeben", "Login failed: no password or key given").into())
    } else {
        Err(lf!("Anmeldung fehlgeschlagen (versucht: {})", "Login failed (tried: {})", tried.join(", ")))
    }
}

async fn open_session(site: &Site, password: &str, trusted: Option<String>) -> Result<Session, ConnectError> {
    let host = site.host.trim().to_string();
    if host.is_empty() {
        return Err(l!("Kein Server angegeben", "No server given").to_string().into());
    }
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let issue = Arc::new(Mutex::new(None));
    let handler = Client { host: host.clone(), port: site.port, trusted, issue: issue.clone() };
    let connected = tokio::time::timeout(TIMEOUT, client::connect(config, (host.as_str(), site.port), handler)).await;
    let mut handle = match connected {
        Err(_) => return Err(lf!("{host}:{}: Zeitüberschreitung", "{host}:{}: timeout", site.port).into()),
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            return Err(match issue.lock().unwrap().take() {
                Some(HostKeyIssue::Unknown(fp)) => ConnectError::UnknownHostKey(fp),
                Some(HostKeyIssue::Changed(msg)) => ConnectError::Failed(msg),
                None => ConnectError::Failed(format!("{host}:{}: {e}", site.port)),
            });
        }
    };
    authenticate(&mut handle, site, password).await?;
    let channel = handle.channel_open_session().await.map_err(|e| e.to_string())?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("SFTP-Subsystem: {e}"))?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| format!("SFTP: {}", sftp_err(e)))?;
    sftp.set_timeout(30);
    Ok(Session { handle, sftp: Arc::new(sftp) })
}

/// Blocking `Read`/`Write` over an async SFTP file.
struct BlockingFile<'a> {
    rt: &'a tokio::runtime::Runtime,
    file: &'a mut russh_sftp::client::fs::File,
}

impl Read for BlockingFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.rt.block_on(self.file.read(buf))
    }
}

impl Write for BlockingFile<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.rt.block_on(self.file.write(buf))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.rt.block_on(self.file.flush())
    }
}

impl SftpConn {
    pub fn connect(site: Site, password: String, trusted: Option<String>) -> Result<SftpConn, ConnectError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("miery-sftp")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let session = rt.block_on(open_session(&site, &password, trusted))?;
        let dir = site.remote_dir.trim().to_string();
        let start = if dir.is_empty() { ".".to_string() } else { dir.clone() };
        let home = rt
            .block_on(session.sftp.canonicalize(start))
            .map_err(|e| if dir.is_empty() { sftp_err(e) } else { lf!("Startordner {dir}: {}", "Start folder {dir}: {}", sftp_err(e)) })?;
        Ok(SftpConn {
            id: remote::next_id(),
            site,
            password,
            home,
            rt: Some(rt),
            session: Mutex::new(Some(Arc::new(session))),
        })
    }

    fn rt(&self) -> &tokio::runtime::Runtime {
        self.rt.as_ref().unwrap()
    }

    fn session(&self) -> Result<Arc<Session>, String> {
        let mut guard = self.session.lock().unwrap();
        if guard.is_none() {
            let s = self
                .rt()
                .block_on(open_session(&self.site, &self.password, None))
                .map_err(|e| e.to_string())?;
            *guard = Some(Arc::new(s));
        }
        Ok(guard.as_ref().unwrap().clone())
    }

    /// Run an SFTP call; a dropped connection is re-established once.
    fn run<R>(&self, f: impl AsyncFn(&SftpSession) -> Result<R, SftpError>) -> Result<R, String> {
        for attempt in 0..2 {
            let s = self.session()?;
            match self.rt().block_on(f(&s.sftp)) {
                Ok(r) => return Ok(r),
                Err(e) if attempt == 0 && is_connection_error(&e) => {
                    *self.session.lock().unwrap() = None;
                }
                Err(e) => return Err(sftp_err(e)),
            }
        }
        Err(l!("Verbindung verloren", "Connection lost").into())
    }
}

fn to_time(secs: Option<u32>) -> Option<SystemTime> {
    secs.filter(|s| *s > 0).map(|s| UNIX_EPOCH + Duration::from_secs(s as u64))
}

impl Remote for SftpConn {
    fn id(&self) -> usize {
        self.id
    }

    fn site(&self) -> &Site {
        &self.site
    }

    fn home(&self) -> &str {
        &self.home
    }

    fn url(&self) -> String {
        format!("sftp://{}@{}", self.site.user, self.site.host)
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>, String> {
        let items = self.run(async |s| {
            let mut out = Vec::new();
            for e in s.read_dir(dir).await? {
                let name = e.file_name();
                let meta = e.metadata();
                let path = remote::join(dir, &name);
                // Follow symlinks to find out whether they point to a directory.
                let target_is_dir = if meta.is_symlink() {
                    s.metadata(path.clone()).await.map(|m| m.is_dir()).unwrap_or(false)
                } else {
                    meta.is_dir()
                };
                out.push((name, path, meta, target_is_dir));
            }
            Ok(out)
        })?;
        Ok(items
            .into_iter()
            .filter(|(name, ..)| name != "." && name != "..")
            .map(|(name, path, meta, is_dir)| Entry {
                name,
                path: PathBuf::from(path),
                is_dir,
                is_link: meta.is_symlink(),
                is_parent: false,
                size: if is_dir { 0 } else { meta.size.unwrap_or(0) },
                modified: to_time(meta.mtime),
                mode: meta.permissions.unwrap_or(0) & 0o7777,
            })
            .collect())
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.run(async |s| s.create_dir(path).await)
    }

    fn is_dir(&self, path: &str) -> bool {
        self.run(async |s| s.metadata(path).await).map(|m| m.is_dir()).unwrap_or(false)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.run(async |s| s.rename(from, to).await)
    }

    fn remove_file(&self, path: &str) -> Result<(), String> {
        self.run(async |s| s.remove_file(path).await)
    }

    fn remove_dir(&self, path: &str) -> Result<(), String> {
        self.run(async |s| s.remove_dir(path).await)
    }

    fn download(&self, remote: &str, local: &Path, pump: &Pump) -> Result<(), String> {
        let mut file = self.run(async |s| s.open(remote).await)?;
        let mut out = std::fs::File::create(local).map_err(|e| e.to_string())?;
        let r = pump(&mut BlockingFile { rt: self.rt(), file: &mut file }, &mut out);
        let _ = self.rt().block_on(file.shutdown());
        r.map_err(|e| e.to_string())
    }

    fn upload(&self, local: &Path, remote: &str, pump: &Pump) -> Result<(), String> {
        let mut input = std::fs::File::open(local).map_err(|e| e.to_string())?;
        let mut file = self.run(async |s| s.create(remote).await)?;
        let r = pump(&mut input, &mut BlockingFile { rt: self.rt(), file: &mut file });
        // shutdown flushes pending writes and closes the handle.
        let closed = self.rt().block_on(file.shutdown());
        r.map_err(|e| e.to_string())?;
        closed.map_err(|e| e.to_string())?;
        // Keep the modification time, like a local copy does.
        if let Ok(m) = std::fs::metadata(local).and_then(|m| m.modified())
            && let Ok(d) = m.duration_since(UNIX_EPOCH)
        {
            let secs = d.as_secs() as u32;
            let attrs = FileAttributes { mtime: Some(secs), atime: Some(secs), ..FileAttributes::empty() };
            let _ = self.run(async |s| s.set_metadata(remote, attrs.clone()).await);
        }
        Ok(())
    }

    fn quit(&self) {
        if let Some(s) = self.session.lock().unwrap().take() {
            let _ = self.rt().block_on(async {
                let _ = s.sftp.close().await;
                s.handle.disconnect(russh::Disconnect::ByApplication, "", "en").await
            });
        }
    }
}

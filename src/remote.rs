//! Remote file systems (FTP, SFTP) behind one interface.
//!
//! Every open connection lives in a global registry so panels (which only
//! store a connection id in their `Location`) and worker threads can reach it.

use crate::fsutil::Entry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

const KEYRING_SERVICE: &str = "MieryCommander";

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
pub enum Protocol {
    #[default]
    Ftp,
    Sftp,
    /// Windows/NAS share, made available as a local folder by the desktop (see `smb`).
    Smb,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
pub enum Security {
    #[default]
    None,
    /// AUTH TLS on the normal port (recommended).
    Explicit,
    /// TLS from the first byte, usually port 990.
    Implicit,
}

/// A saved (or quick-connect) server. The password is never stored here.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
pub struct Site {
    pub name: String,
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub remote_dir: String,
    pub save_password: bool,
    // FTP only
    pub security: Security,
    pub passive: bool,
    pub accept_invalid_certs: bool,
    // SFTP only
    /// Private key file; empty = none.
    pub key_file: String,
    /// Try the SSH agent and ~/.ssh/id_* keys.
    pub auto_keys: bool,
}

impl Default for Site {
    fn default() -> Self {
        Site {
            name: String::new(),
            protocol: Protocol::Ftp,
            host: String::new(),
            port: 21,
            user: "anonymous".into(),
            remote_dir: String::new(),
            save_password: false,
            security: Security::None,
            passive: true,
            accept_invalid_certs: false,
            key_file: String::new(),
            auto_keys: true,
        }
    }
}

impl Site {
    pub fn icon(&self) -> &'static str {
        match self.protocol {
            Protocol::Ftp => "🌐",
            Protocol::Sftp => "🔒",
            Protocol::Smb => "🖧",
        }
    }

    pub fn label(&self) -> String {
        if self.name.trim().is_empty() {
            format!("{}@{}", self.user, self.host)
        } else {
            self.name.clone()
        }
    }

    fn keyring_user(&self) -> String {
        let scheme = match self.protocol {
            Protocol::Ftp => "ftp",
            Protocol::Sftp => "sftp",
            Protocol::Smb => "smb",
        };
        format!("{scheme}://{}@{}:{}", self.user, self.host, self.port)
    }

    pub fn load_password(&self) -> Option<String> {
        keyring::Entry::new(KEYRING_SERVICE, &self.keyring_user()).ok()?.get_password().ok()
    }

    pub fn store_password(&self, password: &str) -> Result<(), String> {
        keyring::Entry::new(KEYRING_SERVICE, &self.keyring_user())
            .and_then(|e| e.set_password(password))
            .map_err(|e| format!("Passwort konnte nicht im Schlüsselbund gespeichert werden: {e}"))
    }

    pub fn forget_password(&self) {
        if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, &self.keyring_user()) {
            let _ = e.delete_credential();
        }
    }
}

/// Copies from a reader to a writer, reporting progress and honoring cancel.
pub type Pump<'a> = dyn Fn(&mut dyn Read, &mut dyn Write) -> std::io::Result<()> + 'a;

pub trait Remote: Send + Sync {
    fn id(&self) -> usize;
    fn site(&self) -> &Site;
    /// Directory the session started in.
    fn home(&self) -> &str;
    /// "ftp://user@host" – prefix for display.
    fn url(&self) -> String;

    fn list(&self, dir: &str) -> Result<Vec<Entry>, String>;
    fn mkdir(&self, path: &str) -> Result<(), String>;
    fn is_dir(&self, path: &str) -> bool;
    fn rename(&self, from: &str, to: &str) -> Result<(), String>;
    fn remove_file(&self, path: &str) -> Result<(), String>;
    fn remove_dir(&self, path: &str) -> Result<(), String>;
    /// Download `remote` into the local file `local` (created/truncated).
    fn download(&self, remote: &str, local: &Path, pump: &Pump) -> Result<(), String>;
    /// Upload the local file `local` to `remote`.
    fn upload(&self, local: &Path, remote: &str, pump: &Pump) -> Result<(), String>;
    /// Close the session (best effort).
    fn quit(&self);

    fn mkdir_all(&self, path: &str) -> Result<(), String> {
        if self.is_dir(path) {
            return Ok(());
        }
        let mut cur = if path.starts_with('/') { "/".to_string() } else { String::new() };
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = join(&cur, part);
            if !self.is_dir(&cur) {
                self.mkdir(&cur).map_err(|e| format!("{cur}: {e}"))?;
            }
        }
        Ok(())
    }

    /// Recursively list everything below `dir`, parents before children.
    fn walk(&self, dir: &str, out: &mut Vec<Entry>) -> Result<(), String> {
        for e in self.list(dir)? {
            let is_dir = e.is_dir;
            let p = e.path.to_string_lossy().into_owned();
            out.push(e);
            if is_dir {
                self.walk(&p, out)?;
            }
        }
        Ok(())
    }
}

/// Why a connection attempt failed.
#[derive(Debug, Clone)]
pub enum ConnectError {
    Failed(String),
    /// SSH: the server's host key is not in known_hosts yet. Contains the
    /// SHA256 fingerprint the user has to confirm.
    UnknownHostKey(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Failed(m) => f.write_str(m),
            ConnectError::UnknownHostKey(fp) => write!(f, "Unbekannter Host-Schlüssel {fp}"),
        }
    }
}

impl From<String> for ConnectError {
    fn from(s: String) -> Self {
        ConnectError::Failed(s)
    }
}

// ----------------------------------------------------------------------------
// Registry
// ----------------------------------------------------------------------------

static REGISTRY: LazyLock<Mutex<HashMap<usize, Arc<dyn Remote>>>> = LazyLock::new(Default::default);
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

pub fn next_id() -> usize {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn get(id: usize) -> Option<Arc<dyn Remote>> {
    REGISTRY.lock().unwrap().get(&id).cloned()
}

pub fn connections() -> Vec<Arc<dyn Remote>> {
    let mut v: Vec<_> = REGISTRY.lock().unwrap().values().cloned().collect();
    v.sort_by_key(|c| c.id());
    v
}

pub fn disconnect(id: usize) {
    let conn = REGISTRY.lock().unwrap().remove(&id);
    if let Some(c) = conn {
        c.quit();
    }
}

/// Open a connection, log in and register it. `trusted_host_key` is the
/// fingerprint the user confirmed after an `UnknownHostKey` error.
pub fn connect(site: Site, password: String, trusted_host_key: Option<String>) -> Result<Arc<dyn Remote>, ConnectError> {
    let conn: Arc<dyn Remote> = match site.protocol {
        Protocol::Ftp => Arc::new(crate::ftp::FtpConn::connect(site, password)?),
        Protocol::Sftp => Arc::new(crate::sftp::SftpConn::connect(site, password, trusted_host_key)?),
        Protocol::Smb => return Err(ConnectError::Failed("SMB wird über smb::mount eingebunden".into())),
    };
    REGISTRY.lock().unwrap().insert(conn.id(), conn.clone());
    Ok(conn)
}

// ----------------------------------------------------------------------------
// Remote path helpers
// ----------------------------------------------------------------------------

/// Join remote path components with '/'.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Parent of a remote path, `None` at the root.
pub fn parent(path: &str) -> Option<String> {
    let t = path.trim_end_matches('/');
    if t.is_empty() {
        return None;
    }
    match t.rfind('/') {
        Some(0) => Some("/".into()),
        Some(i) => Some(t[..i].to_string()),
        None => Some(String::new()),
    }
}

pub fn file_name(path: &str) -> &str {
    let t = path.trim_end_matches('/');
    t.rsplit('/').next().unwrap_or(t)
}

/// Local cache dir for files opened from a server (F3/F4).
pub fn cache_dir(id: usize) -> PathBuf {
    std::env::temp_dir().join(format!("miery-remote-{}", std::process::id())).join(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths() {
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/pub", "a"), "/pub/a");
        assert_eq!(parent("/pub/a"), Some("/pub".into()));
        assert_eq!(parent("/pub"), Some("/".into()));
        assert_eq!(parent("/"), None);
        assert_eq!(file_name("/pub/a.txt"), "a.txt");
    }
}

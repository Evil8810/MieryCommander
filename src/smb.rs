//! SMB / Windows shares.
//!
//! Instead of speaking SMB ourselves we let the desktop make the share a
//! normal folder, the same way the system file manager does it:
//! - KDE: kio-fuse (D-Bus `org.kde.KIOFuse`), credentials via KWallet /
//!   the KDE password dialog – exactly like Dolphin.
//! - GNOME & co.: `gio mount` (gvfs) → /run/user/<uid>/gvfs/…
//! - macOS: Finder's "mount volume" → /Volumes/<share>.
//! The result is a local path the panel can open like any other folder.

use std::path::{Path, PathBuf};

/// Percent-encode a user name or password for the user-info part of a URL.
fn encode_userinfo(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// "DOMAIN\user" → (Some("DOMAIN"), "user")
fn split_domain(user: &str) -> (Option<&str>, &str) {
    match user.split_once('\\') {
        Some((d, u)) if !d.is_empty() => (Some(d), u),
        _ => (None, user),
    }
}

/// "smb://[user@]host[/share]" – never contains a password (safe to show).
pub fn url(host: &str, share: &str, user: &str) -> String {
    url_with_password(host, share, user, "")
}

/// Like `url`, with the password in the user info (KIO's way of passing it on).
/// A Windows domain ("DOMAIN\user") becomes KIO's "DOMAIN;user".
fn url_with_password(host: &str, share: &str, user: &str, password: &str) -> String {
    let host = host.trim().trim_start_matches("smb://").trim_matches('/');
    let share = share.trim().trim_matches('/');
    let user = user.trim();
    let mut u = String::from("smb://");
    if !user.is_empty() && user != "anonymous" {
        let (domain, name) = split_domain(user);
        if let Some(d) = domain {
            u.push_str(&encode_userinfo(d));
            u.push(';');
        }
        u.push_str(&encode_userinfo(name));
        if !password.is_empty() {
            u.push(':');
            u.push_str(&encode_userinfo(password));
        }
        u.push('@');
    }
    u.push_str(host);
    if !share.is_empty() {
        u.push('/');
        u.push_str(share);
    }
    u
}

/// Make the share (or, without a share, the list of shares) available as a
/// local folder and return its path. Blocks until done; call off the UI thread.
///
/// With an empty password the desktop's own login (KWallet / KDE dialog,
/// GNOME keyring, macOS keychain) is used. A given password is handed to the
/// desktop directly: over D-Bus to kio-fuse, on stdin to `gio`/`osascript` –
/// never on a command line where other processes could see it.
pub fn mount(host: &str, share: &str, user: &str, password: &str) -> Result<PathBuf, String> {
    if host.trim().is_empty() {
        return Err(l!("Kein Server angegeben", "No server given").into());
    }
    if !password.is_empty() && (user.trim().is_empty() || user.trim() == "anonymous") {
        return Err(l!("Für ein Passwort bitte auch den Benutzer angeben", "Please also enter the user for a password").into());
    }
    #[cfg(target_os = "linux")]
    {
        match kio_fuse(&url_with_password(host, share, user, password), password) {
            Ok(p) => Ok(p),
            Err(kio_err) => gio_mount(&url(host, share, user), host, share, user, password).map_err(|gio_err| {
                lf!("SMB konnte nicht eingebunden werden.\nKDE (kio-fuse): {kio_err}\nGNOME (gio): {gio_err}", "Could not mount the SMB share.\nKDE (kio-fuse): {kio_err}\nGNOME (gio): {gio_err}")
            }),
        }
    }
    #[cfg(target_os = "macos")]
    {
        mac_mount(&url(host, share, user), share, user, password)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = url;
        Err(l!("SMB wird auf diesem System nicht unterstützt", "SMB is not supported on this system").into())
    }
}

/// The folder path must never reveal the password (it is shown and saved).
fn check_no_password(path: &Path, password: &str) -> Result<(), String> {
    let p = path.to_string_lossy();
    if password.len() >= 2 && (p.contains(password) || p.contains(&encode_userinfo(password))) {
        return Err(l!("Abgebrochen: Das System hätte das Passwort im Ordnerpfad angezeigt", "Cancelled: the system would have shown the password in the folder path").into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn kio_fuse(url: &str, password: &str) -> Result<PathBuf, String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let reply = conn
        .call_method(
            Some("org.kde.KIOFuse"),
            "/org/kde/KIOFuse",
            Some("org.kde.KIOFuse.VFS"),
            "mountUrl",
            &(url,),
        )
        .map_err(|e| match e {
            zbus::Error::MethodError(name, msg, _) => {
                let msg = msg.unwrap_or_default();
                if name.as_str().contains("ServiceUnknown") {
                    l!("kio-fuse ist nicht installiert", "kio-fuse is not installed").to_string()
                } else if msg.is_empty() {
                    name.to_string()
                } else {
                    msg
                }
            }
            other => other.to_string(),
        })?;
    let path: String = reply.body().deserialize().map_err(|e| e.to_string())?;
    let path = PathBuf::from(path);
    check_no_password(&path, password)?;
    // The first listing is what actually talks to the server: surface errors here.
    std::fs::read_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Answers for `gio mount`'s prompts: "User", "Domain", "Password".
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn gio_answers(user: &str, password: &str) -> String {
    let (domain, name) = split_domain(user.trim());
    format!("{name}\n{}\n{password}\n", domain.unwrap_or(""))
}

#[cfg(target_os = "linux")]
fn gio_mount(url: &str, host: &str, share: &str, user: &str, password: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    let mut child = std::process::Command::new("gio")
        .args(["mount", url])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| lf!("gio nicht verfügbar ({e})", "gio not available ({e})"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // Only answer with a password if we have one; otherwise gio uses the keyring.
        let answers = if password.is_empty() { String::new() } else { gio_answers(user, password) };
        let _ = stdin.write_all(answers.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    // "already mounted" is fine.
    if !out.status.success() && !stderr.to_lowercase().contains("already") {
        return Err(stderr.trim().to_string());
    }
    let gvfs = PathBuf::from(format!("/run/user/{}/gvfs", unsafe { libc::getuid() }));
    let host = host.trim().trim_start_matches("smb://").trim_matches('/').to_lowercase();
    let share = share.trim().trim_matches('/').to_lowercase();
    let wanted = if share.is_empty() {
        format!("smb-server:server={host}")
    } else {
        format!("smb-share:server={host},share={share}")
    };
    std::fs::read_dir(&gvfs)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().starts_with(&wanted)))
        .ok_or_else(|| l!("Eingebundene Freigabe nicht gefunden", "Mounted share not found").to_string())
}

/// AppleScript string literal.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_script(url: &str, user: &str, password: &str) -> String {
    let mut script = format!("mount volume {}", applescript_str(url));
    if !password.is_empty() {
        script.push_str(&format!(" as user name {} with password {}", applescript_str(user.trim()), applescript_str(password)));
    }
    script
}

#[cfg(target_os = "macos")]
fn mac_mount(url: &str, share: &str, user: &str, password: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    // Without a password Finder asks itself (and offers the keychain).
    // The script goes in on stdin so the password never shows up in `ps`.
    let mut child = std::process::Command::new("osascript")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(mac_script(url, user, password).as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    // osascript prints e.g. "file Media:"; the volume lives in /Volumes.
    let printed = String::from_utf8_lossy(&out.stdout);
    let name = printed
        .trim()
        .trim_start_matches("file ")
        .trim_end_matches(':')
        .to_string();
    let candidate = if !name.is_empty() { name } else { share.trim_matches('/').to_string() };
    let p = Path::new("/Volumes").join(candidate);
    Ok(if p.is_dir() { p } else { PathBuf::from("/Volumes") })
}

/// Shares currently provided by kio-fuse below its mount point
/// (`<mp>/smb/<host>/<share>`), as (label, path).
pub fn kio_fuse_shares(mount_point: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(protocols) = std::fs::read_dir(mount_point) else { return out };
    for proto in protocols.flatten() {
        let proto_name = proto.file_name().to_string_lossy().into_owned();
        let Ok(hosts) = std::fs::read_dir(proto.path()) else { continue };
        for host in hosts.flatten() {
            let host_name = host.file_name().to_string_lossy().into_owned();
            let Ok(shares) = std::fs::read_dir(host.path()) else { continue };
            for share in shares.flatten() {
                if share.path().is_dir() {
                    let label = format!("{host_name}/{} ({proto_name})", share.file_name().to_string_lossy());
                    out.push((label, share.path()));
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smb_urls() {
        assert_eq!(url("nas.local", "Media", ""), "smb://nas.local/Media");
        assert_eq!(url("smb://nas.local/", "/Media/", "anna"), "smb://anna@nas.local/Media");
        assert_eq!(url("nas", "", "anonymous"), "smb://nas");
        // Passwords with URL special characters are encoded; domains use KIO's ';'.
        assert_eq!(url_with_password("nas", "x", "anna", "p@ss:w/rd?"), "smb://anna:p%40ss%3Aw%2Frd%3F@nas/x");
        assert_eq!(url_with_password("nas", "", "FIRMA\\anna", "pw"), "smb://FIRMA;anna:pw@nas");
        // The displayable URL never carries the password.
        assert!(!url("nas", "x", "anna").trim_start_matches("smb://").contains(':'));
    }

    #[test]
    fn password_never_in_path() {
        assert!(check_no_password(Path::new("/run/user/1000/kio-fuse-x/smb/anna@nas/x"), "geheim").is_ok());
        assert!(check_no_password(Path::new("/run/user/1000/kio-fuse-x/smb/anna:geheim@nas/x"), "geheim").is_err());
        assert!(check_no_password(Path::new("/x/anna:p%40ss@nas"), "p@ss").is_err());
    }

    #[test]
    fn desktop_prompts() {
        assert_eq!(gio_answers("FIRMA\\anna", "pw"), "anna\nFIRMA\npw\n");
        assert_eq!(gio_answers("anna", "pw"), "anna\n\npw\n");
        assert_eq!(
            mac_script("smb://anna@nas/Media", "anna", "a\"b\\c"),
            "mount volume \"smb://anna@nas/Media\" as user name \"anna\" with password \"a\\\"b\\\\c\""
        );
        assert_eq!(mac_script("smb://nas/Media", "", ""), "mount volume \"smb://nas/Media\"");
    }

    #[test]
    fn kio_fuse_tree() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("smb/nas.local/Media/Filme")).unwrap();
        std::fs::create_dir_all(d.path().join("smb/nas.local/backup")).unwrap();
        let got: Vec<String> = kio_fuse_shares(d.path()).into_iter().map(|(l, _)| l).collect();
        assert_eq!(got, ["nas.local/Media (smb)", "nas.local/backup (smb)"]);
    }
}

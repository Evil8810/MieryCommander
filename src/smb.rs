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
        let attempt = |host: &str| {
            if password.is_empty() && is_guest(user) {
                // Guest share: log in as guest directly – otherwise Finder
                // insists on asking for a user name and password.
                mac_mount(&guest_url(host, share), share, "", "").or_else(|_| mac_mount(&url(host, share, ""), share, "", ""))
            } else {
                mac_mount(&url(host, share, user), share, user, password)
            }
        };
        attempt(host).or_else(|err| {
            // "nas" alone often isn't found on a Mac, "nas.local" (Bonjour) is.
            let h = host.trim().trim_start_matches("smb://").trim_end_matches('/');
            if h.contains('.') || h.contains(':') { Err(err) } else { attempt(&format!("{h}.local")).map_err(|_| err) }
        })
    }
    #[cfg(windows)]
    {
        // Windows opens \\server\share directly; we only log in first.
        let h = bare_host(host);
        let s = share.trim().trim_matches(['/', '\\']);
        if s.is_empty() {
            return Err(l!("Bitte eine Freigabe eintragen oder mit „📂 Anzeigen“ auswählen", "Please enter a share or pick one with “📂 List”").into());
        }
        let unc = format!("\\\\{h}\\{}", s.replace('/', "\\"));
        // Guest: try with the current Windows login.
        let login_user = if is_guest(user) && password.is_empty() { "" } else { user };
        crate::winsys::connect_share(&unc, login_user, password)?;
        let path = PathBuf::from(format!("{unc}\\"));
        std::fs::read_dir(&path).map_err(|e| format!("{unc}: {e}"))?;
        Ok(path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = url;
        Err(l!("SMB wird auf diesem System nicht unterstützt", "SMB is not supported on this system").into())
    }
}

/// "smb://nas.local/" or "\\\\nas" → "nas.local" / "nas".
#[cfg_attr(not(windows), allow(dead_code))]
fn bare_host(host: &str) -> &str {
    host.trim().trim_start_matches("smb://").trim_start_matches("\\\\").trim_end_matches(['/', '\\'])
}

/// No user (or "guest"/"Gast"): log in as guest.
pub fn is_guest(user: &str) -> bool {
    let u = user.trim().to_lowercase();
    u.is_empty() || u == "guest" || u == "gast" || u == "anonymous"
}

/// "smb://guest:@host/share" – the guest login Finder accepts without asking.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn guest_url(host: &str, share: &str) -> String {
    url(host, share, "guest").replacen("guest@", "guest:@", 1)
}

/// Run a program for at most `limit` and return what it printed so far
/// (browsing tools like `dns-sd -B` never end on their own).
fn output_within(cmd: &mut std::process::Command, limit: std::time::Duration) -> Result<String, String> {
    use std::io::Read;
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or("stdout")?;
    let mut stderr = child.stderr.take().ok_or("stderr")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let end = std::time::Instant::now() + limit;
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(st);
        }
        if std::time::Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    match status {
        Some(st) if !st.success() && out.trim().is_empty() => Err(if err.trim().is_empty() { st.to_string() } else { err.trim().to_string() }),
        _ => Ok(out),
    }
}

/// SMB servers announced in the local network (Bonjour/Avahi), as
/// (name, host). Takes a few seconds; call off the UI thread.
pub fn discover() -> Result<Vec<(String, String)>, String> {
    #[cfg(target_os = "macos")]
    {
        use std::time::Duration;
        let browse = output_within(std::process::Command::new("dns-sd").args(["-B", "_smb._tcp", "local."]), Duration::from_secs(3))?;
        let names = parse_dns_sd_browse(&browse);
        // Resolve the names to host names in parallel ("TrueNAS" → "truenas.local").
        let handles: Vec<_> = names
            .into_iter()
            .map(|name| {
                std::thread::spawn(move || {
                    let out = output_within(std::process::Command::new("dns-sd").args(["-L", name.as_str(), "_smb._tcp", "local."]), Duration::from_secs(2)).unwrap_or_default();
                    let host = parse_dns_sd_resolve(&out).unwrap_or_else(|| format!("{}.local", name.replace(' ', "-")));
                    (name, host)
                })
            })
            .collect();
        let mut found: Vec<(String, String)> = handles.into_iter().filter_map(|h| h.join().ok()).collect();
        found.sort();
        found.dedup_by(|a, b| a.1 == b.1);
        Ok(found)
    }
    #[cfg(windows)]
    {
        Err(l!("Die Netzwerksuche gibt es unter Windows nicht – bitte Servername oder IP-Adresse eintragen", "Network search is not available on Windows – please enter the server name or IP address").into())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let out = output_within(std::process::Command::new("avahi-browse").args(["-tpr", "_smb._tcp"]), std::time::Duration::from_secs(5))
            .map_err(|e| lf!("Netzwerksuche nicht möglich (avahi-browse): {e}", "Network search not possible (avahi-browse): {e}"))?;
        Ok(parse_avahi(&out))
    }
}

/// `dns-sd -B` lines: "12:00:00.000  Add  3  4 local.  _smb._tcp.  TrueNAS" → instance names.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_dns_sd_browse(out: &str) -> Vec<String> {
    let mut names: Vec<String> = out
        .lines()
        .filter_map(|line| {
            let t: Vec<&str> = line.split_whitespace().collect();
            (t.len() > 6 && t[1] == "Add").then(|| t[6..].join(" "))
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `dns-sd -L`: "… can be reached at truenas.local.:445 (interface 4)" → "truenas.local".
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_dns_sd_resolve(out: &str) -> Option<String> {
    let rest = out.split("can be reached at ").nth(1)?;
    let host = rest.split(|c: char| c == ':' || c.is_whitespace()).next()?.trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_string())
}

/// `avahi-browse -tpr` lines: "=;eth0;IPv4;TrueNAS;_smb._tcp;local;truenas.local;192.168.1.5;445;…".
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn parse_avahi(out: &str) -> Vec<(String, String)> {
    let unescape = |s: &str| {
        // avahi writes special characters as \DDD (decimal), e.g. "\032" = space.
        let b = s.as_bytes();
        let mut v = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
                v.push(s[i + 1..i + 4].parse::<u8>().unwrap_or(b'?'));
                i += 4;
            } else {
                v.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&v).into_owned()
    };
    let mut found: Vec<(String, String)> = out
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(';').collect();
            (f.len() > 7 && f[0] == "=").then(|| (unescape(f[3]), f[6].to_string()))
        })
        .collect();
    found.sort();
    found.dedup_by(|a, b| a.1 == b.1);
    found
}

/// The shares a server offers (only normal folders, no "IPC$" etc.).
/// Uses guest access, the given password or the system's saved login.
/// Call off the UI thread.
pub fn list_shares(host: &str, user: &str, password: &str) -> Result<Vec<String>, String> {
    let host = host.trim().trim_start_matches("smb://").trim_end_matches('/');
    if host.is_empty() {
        return Err(l!("Kein Server angegeben", "No server given").into());
    }
    #[cfg(windows)]
    {
        if !(is_guest(user) && password.is_empty()) {
            // Log in to the server first, so it shows its shares to this user.
            crate::winsys::connect_share(&format!("\\\\{host}\\IPC$"), user, password)?;
        }
        let shares = crate::winsys::list_shares(host).map_err(|e| lf!("Freigaben können nicht abgefragt werden: {e}", "Can't list the shares: {e}"))?;
        if shares.is_empty() {
            return Err(l!("Keine Freigaben gefunden. Bitte den Freigabenamen eintragen.", "No shares found. Please enter the share name.").into());
        }
        return Ok(shares);
    }
    #[cfg_attr(windows, allow(unreachable_code))]
    let limit = std::time::Duration::from_secs(15);
    #[cfg(target_os = "macos")]
    let out = {
        let mut cmd = std::process::Command::new("smbutil");
        if is_guest(user) {
            cmd.args(["view", "-N", "-g", &format!("//{host}")]);
        } else {
            // -N: no password prompt; the keychain (saved by Finder) is used.
            // (smbutil only reads a password from the terminal or the command
            // line, where other processes could see it – so it isn't passed.)
            let _ = password;
            cmd.args(["view", "-N", &format!("//{}@{host}", user.trim())]);
        }
        output_within(&mut cmd, limit)?
    };
    #[cfg(not(target_os = "macos"))]
    let out = {
        #[cfg(windows)]
        let _ = (user, password);
        let mut cmd = std::process::Command::new("smbclient");
        cmd.args(["-g", "-L", host]);
        if is_guest(user) && password.is_empty() {
            cmd.arg("-N");
        } else {
            // The password goes in through the environment, not the command line.
            cmd.args(["-U", user.trim()]).env("PASSWD", password);
        }
        output_within(&mut cmd, limit).map_err(|e| lf!("Freigaben können nicht abgefragt werden (smbclient): {e}", "Can't list the shares (smbclient): {e}"))?
    };
    let shares = parse_share_list(&out);
    if shares.is_empty() {
        return Err(if is_guest(user) {
            l!("Keine Freigaben gefunden – evtl. ist kein Gastzugang erlaubt. Bitte Freigabe und Benutzer eintragen.", "No shares found – guest access may not be allowed. Please enter the share and user.").into()
        } else {
            l!("Keine Freigaben gefunden. Bitte den Freigabenamen eintragen.", "No shares found. Please enter the share name.").into()
        });
    }
    Ok(shares)
}

/// Share names from `smbutil view` (table with a "Type" column) or
/// `smbclient -g -L` ("Disk|Media|comment").
fn parse_share_list(out: &str) -> Vec<String> {
    let mut shares = Vec::new();
    let mut type_col = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("Disk|") {
            shares.push(rest.split('|').next().unwrap_or("").to_string());
            continue;
        }
        if type_col.is_none() {
            if line.trim_start().starts_with("Share") && line.contains("Type") {
                type_col = line.find("Type").map(|b| line[..b].chars().count());
            }
            continue;
        }
        let Some(col) = type_col else { continue };
        if line.trim().is_empty() || line.trim_start().starts_with('-') || line.contains("shares listed") {
            continue;
        }
        let name: String = line.chars().take(col).collect();
        let kind: String = line.chars().skip(col).collect();
        if kind.split_whitespace().next() == Some("Disk") {
            shares.push(name.trim().to_string());
        }
    }
    shares.retain(|s| !s.is_empty() && !s.ends_with('$'));
    shares.sort_by_key(|s| s.to_lowercase());
    shares.dedup();
    shares
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
    fn guest_and_discovery() {
        assert!(is_guest("") && is_guest(" Gast ") && is_guest("guest") && !is_guest("anna"));
        assert_eq!(guest_url("nas.local", "Media"), "smb://guest:@nas.local/Media");
        let browse = "Browsing for _smb._tcp\nDATE: ---Thu 08 Oct 2026---\n18:00:00.000  ...STARTING...\n\
Timestamp     A/R    Flags  if Domain               Service Type         Instance Name\n\
18:00:00.100  Add        3  14 local.               _smb._tcp.           TrueNAS\n\
18:00:00.101  Add        2  14 local.               _smb._tcp.           Papas Mac mini\n";
        assert_eq!(parse_dns_sd_browse(browse), ["Papas Mac mini", "TrueNAS"]);
        let resolve = "Lookup TrueNAS._smb._tcp.local\n18:00:01.000  TrueNAS._smb._tcp.local. can be reached at truenas.local.:445 (interface 14)\n";
        assert_eq!(parse_dns_sd_resolve(resolve).as_deref(), Some("truenas.local"));
        let avahi = "+;eth0;IPv4;TrueNAS;_smb._tcp;local\n=;eth0;IPv4;Papas\\032Mac;_smb._tcp;local;mac.local;192.168.1.9;445;\n=;eth0;IPv6;Papas\\032Mac;_smb._tcp;local;mac.local;fe80::1;445;\n";
        assert_eq!(parse_avahi(avahi), [("Papas Mac".to_string(), "mac.local".to_string())]);
    }

    #[test]
    fn share_lists() {
        let smbutil = "Share                                           Type    Comments\n-------------------------------\n\
Media                                           Disk    \n\
Fotos Familie                                   Disk    Bilder\n\
IPC$                                            Pipe    IPC Service\n\
\n3 shares listed\n";
        assert_eq!(parse_share_list(smbutil), ["Fotos Familie", "Media"]);
        let smbclient = "Disk|backup|\nDisk|print$|Printer Drivers\nIPC|IPC$|IPC Service\nDisk|Media|Filme\n";
        assert_eq!(parse_share_list(smbclient), ["backup", "Media"]);
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

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_link: bool,
    pub is_parent: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub mode: u32,
}

impl Entry {
    pub fn parent(path: PathBuf) -> Self {
        Self {
            name: "..".into(),
            path,
            is_dir: true,
            is_link: false,
            is_parent: true,
            size: 0,
            modified: None,
            mode: 0,
        }
    }

    /// (stem, extension) split the way Total Commander shows it.
    /// Directories and dot-files without a further dot have no extension.
    pub fn split_name(&self) -> (&str, &str) {
        split_ext(&self.name, self.is_dir)
    }

    pub fn ext(&self) -> &str {
        self.split_name().1
    }
}

/// The UI context, so background threads can trigger a repaint when they finish.
static UI_CTX: std::sync::OnceLock<eframe::egui::Context> = std::sync::OnceLock::new();

pub fn set_ui_context(ctx: &eframe::egui::Context) {
    let _ = UI_CTX.set(ctx.clone());
}

/// Ask the UI to redraw (call from worker threads after producing results).
pub fn wake_ui() {
    if let Some(c) = UI_CTX.get() {
        c.request_repaint();
    }
}

pub fn split_ext(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(i) => (&name[..i], &name[i + 1..]),
    }
}

/// Branch view: all files below `dir`, named by their path relative to `dir`
/// ("src/main.rs"). Symlinked folders are not followed.
pub fn read_tree(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<Entry>> {
    const LIMIT: usize = 500_000;
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut first = true;
    while let Some(d) = stack.pop() {
        let entries = match read_dir(&d, show_hidden) {
            Ok(v) => v,
            Err(e) if first => return Err(e),
            Err(_) => continue, // unreadable subfolder: skip it
        };
        first = false;
        for mut e in entries {
            if e.is_dir {
                if !e.is_link {
                    stack.push(e.path.clone());
                }
                continue;
            }
            e.name = e.path.strip_prefix(dir).unwrap_or(&e.path).to_string_lossy().into_owned();
            out.push(e);
            if out.len() >= LIMIT {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

pub fn read_dir(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<Entry>> {
    // Tests simulate an unresponsive network drive with a folder named "__slow__".
    #[cfg(test)]
    if dir.file_name().is_some_and(|n| n == "__slow__") {
        std::thread::sleep(std::time::Duration::from_millis(3600));
    }
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let path = item.path();
        let Ok(lmeta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let is_link = lmeta.file_type().is_symlink();
        // Follow links for type/size, but keep the link flag.
        let meta = if is_link {
            std::fs::metadata(&path).unwrap_or(lmeta)
        } else {
            lmeta
        };
        out.push(Entry {
            name,
            path,
            is_dir: meta.is_dir(),
            is_link,
            is_parent: false,
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta.modified().ok(),
            mode: mode_of(&meta),
        });
    }
    Ok(out)
}

#[cfg(unix)]
pub fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
pub fn mode_of(meta: &std::fs::Metadata) -> u32 {
    if meta.permissions().readonly() { 0o444 } else { 0o644 }
}

pub fn format_mode(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

pub fn format_size(n: u64) -> String {
    // Thousands separator: 1.234.567 (German) / 1,234,567 (English)
    let sep = if crate::i18n::en() { ',' } else { '.' };
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

pub fn format_size_short(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        let num = format!("{v:.1}");
        // Decimal comma in German: 15,4 GB
        let num = if crate::i18n::en() { num } else { num.replace('.', ",") };
        format!("{num} {}", UNITS[u])
    }
}

pub fn format_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format(if crate::i18n::en() { "%Y-%m-%d %H:%M" } else { "%d.%m.%Y %H:%M" }).to_string()
        }
        None => String::new(),
    }
}

/// Free and total bytes of the filesystem containing `path`.
#[cfg(unix)]
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some((st.f_bavail as u64 * frsize, st.f_blocks as u64 * frsize))
}

#[cfg(not(unix))]
pub fn disk_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaceKind {
    Root,
    Home,
    Removable,
    Network,
}

impl PlaceKind {
    pub fn icon(self) -> &'static str {
        match self {
            PlaceKind::Root => "💻",
            PlaceKind::Home => "🏠",
            PlaceKind::Removable => "💾",
            PlaceKind::Network => "🖧",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
    pub kind: PlaceKind,
}

impl Place {
    /// Compact name for a drive button: "/", "Home", "USB STICK", "Media".
    pub fn short_label(&self) -> String {
        match self.kind {
            PlaceKind::Root => "/".into(),
            PlaceKind::Home => "Home".into(),
            PlaceKind::Removable => self.label.clone(),
            PlaceKind::Network => {
                // "nas.local/Media (smb)" → "Media", "nas (nas/daten)" → "nas"
                let base = self.label.split(" (").next().unwrap_or(&self.label);
                base.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(base).to_string()
            }
        }
    }
}

/// File systems that live on another machine.
const NETWORK_FS: &[&str] = &[
    "cifs", "smb3", "smbfs", "nfs", "nfs4", "ceph", "glusterfs", "afs", "9p", "davfs",
    "fuse.sshfs", "fuse.rclone", "fuse.davfs2", "fuse.s3fs", "fuse.gcsfuse", "fuse.curlftpfs",
];

/// Undo the octal escapes /proc/mounts uses for spaces, tabs etc.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "smb-share:server=nas,share=daten" → "nas/daten" (GNOME gvfs mount names).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn gvfs_label(name: &str) -> String {
    let Some((proto, rest)) = name.split_once(':') else { return name.to_string() };
    let mut host = None;
    let mut share = None;
    for kv in rest.split(',') {
        match kv.split_once('=') {
            Some(("server" | "host", v)) => host = Some(v),
            Some(("share" | "prefix", v)) => share = Some(v.trim_start_matches("%2F")),
            _ => {}
        }
    }
    let proto = proto.trim_end_matches("-share");
    match (host, share) {
        (Some(h), Some(s)) => format!("{h}/{s} ({proto})"),
        (Some(h), None) => format!("{h} ({proto})"),
        _ => name.to_string(),
    }
}

/// Removable media and network file systems from a /proc/mounts text.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_mounts(text: &str) -> Vec<Place> {
    let mut v = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(dev), Some(mp), Some(fstype)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let mp = unescape_mount(mp);
        let dev = unescape_mount(dev);
        let leaf = Path::new(&mp)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| mp.clone());
        if NETWORK_FS.contains(&fstype) {
            // Show where it comes from: "daten (nas/daten)".
            let source = dev.trim_start_matches("//");
            let label = if source.ends_with(&leaf) { source.to_string() } else { format!("{leaf} ({source})") };
            v.push(Place { label, path: PathBuf::from(mp), kind: PlaceKind::Network });
        } else if ["/run/media/", "/media/", "/mnt/", "/var/mnt/"].iter().any(|p| mp.starts_with(p)) {
            v.push(Place { label: leaf, path: PathBuf::from(mp), kind: PlaceKind::Removable });
        }
    }
    v
}

fn scan_locations() -> Vec<Place> {
    let mut v = vec![Place { label: "/".into(), path: "/".into(), kind: PlaceKind::Root }];
    if let Some(home) = dirs::home_dir() {
        v.push(Place { label: "Home".into(), path: home, kind: PlaceKind::Home });
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            for e in rd.flatten() {
                let path = e.path();
                let kind = match macos_fs_type(&path).as_deref() {
                    Some("smbfs" | "nfs" | "afpfs" | "webdav" | "cifs" | "ftp") => PlaceKind::Network,
                    _ => PlaceKind::Removable,
                };
                v.push(Place { label: e.file_name().to_string_lossy().into_owned(), path, kind });
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            v.extend(parse_mounts(&mounts));
            // SMB shares opened via KDE (Dolphin or MieryCommander) through kio-fuse.
            for line in mounts.lines() {
                let mut it = line.split_whitespace();
                if let (Some(_), Some(mp), Some("fuse.kio-fuse")) = (it.next(), it.next(), it.next()) {
                    for (label, path) in crate::smb::kio_fuse_shares(Path::new(&unescape_mount(mp))) {
                        v.push(Place { label, path, kind: PlaceKind::Network });
                    }
                }
            }
        }
        // GNOME/GVfs network mounts (smb://, sftp://, … opened in Nautilus).
        let gvfs = PathBuf::from(format!("/run/user/{}/gvfs", unsafe { libc::getuid() }));
        if let Ok(rd) = std::fs::read_dir(&gvfs) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                v.push(Place { label: gvfs_label(&name), path: e.path(), kind: PlaceKind::Network });
            }
        }
    }
    // Root, home, removable media, then network drives.
    v.sort_by_key(|p| p.kind as u8);
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| seen.insert(p.path.clone()));
    v
}

#[cfg(target_os = "macos")]
fn macos_fs_type(path: &Path) -> Option<String> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let name = unsafe { CStr::from_ptr(st.f_fstypename.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

/// The "drive" list: root, home, removable media and network drives.
/// Returns the cached list immediately and refreshes it in the background
/// every few seconds, so a hanging network mount can never block the UI.
/// Tests (and website screenshots) can replace the real drive list so no
/// private mount names end up in pictures.
#[cfg(test)]
pub static PLACES_OVERRIDE: std::sync::Mutex<Option<Vec<Place>>> = std::sync::Mutex::new(None);

pub fn locations() -> Vec<Place> {
    #[cfg(test)]
    if let Some(v) = PLACES_OVERRIDE.lock().unwrap().clone() {
        return v;
    }
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Vec<Place>)>> = Mutex::new(None);
    static REFRESHING: AtomicBool = AtomicBool::new(false);
    let cached = CACHE.lock().unwrap().clone();
    let stale = cached.as_ref().is_none_or(|(t, _)| t.elapsed() > Duration::from_secs(3));
    if stale && !REFRESHING.swap(true, Ordering::SeqCst) {
        std::thread::spawn(|| {
            let v = scan_locations();
            // Real paths, e.g. /home/anna → /var/home/anna on Fedora Atomic/Bazzite.
            // Resolved here in the background: a dead network mount can't block the UI.
            let aliases = v
                .iter()
                .filter_map(|p| std::fs::canonicalize(&p.path).ok().filter(|r| *r != p.path).map(|r| (r, p.path.clone())))
                .collect();
            *ALIASES.lock().unwrap() = aliases;
            *CACHE.lock().unwrap() = Some((Instant::now(), v));
            REFRESHING.store(false, Ordering::SeqCst);
            wake_ui();
        });
    }
    match cached {
        Some((_, v)) => v,
        None => {
            // First call: root and home are always there.
            let mut v = vec![Place { label: "/".into(), path: "/".into(), kind: PlaceKind::Root }];
            if let Some(home) = dirs::home_dir() {
                v.push(Place { label: "Home".into(), path: home, kind: PlaceKind::Home });
            }
            v
        }
    }
}

/// The place a path belongs to (longest matching prefix).
pub fn place_of(path: &Path) -> Option<Place> {
    let aliases = ALIASES.lock().unwrap().clone();
    locations()
        .into_iter()
        .filter_map(|p| {
            // Longest matching prefix wins, via the place's path or its real path.
            let direct = path.starts_with(&p.path).then(|| p.path.as_os_str().len());
            let via_alias = aliases
                .iter()
                .filter(|(real, place)| *place == p.path && path.starts_with(real))
                .map(|(real, _)| real.as_os_str().len())
                .max();
            direct.max(via_alias).map(|len| (len, p))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, p)| p)
}

/// (real path, place path) for places whose path is a symlink.
static ALIASES: std::sync::Mutex<Vec<(PathBuf, PathBuf)>> = std::sync::Mutex::new(Vec::new());

/// Convert a Total-Commander style mask ("*.txt;*.md", "a?c*") into a regex.
pub fn wildcard_regex(mask: &str) -> Option<regex::Regex> {
    let parts: Vec<String> = mask
        .split([';', ' '])
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut r = String::new();
            for c in p.chars() {
                match c {
                    '*' => r.push_str(".*"),
                    '?' => r.push('.'),
                    c => r.push_str(&regex::escape(&c.to_string())),
                }
            }
            r
        })
        .collect();
    if parts.is_empty() {
        return regex::Regex::new(".*").ok();
    }
    regex::RegexBuilder::new(&format!("^(?:{})$", parts.join("|")))
        .case_insensitive(true)
        .build()
        .ok()
}

pub fn open_default(path: &Path) -> Result<(), String> {
    open::that_detached(path).map_err(|e| e.to_string())
}

fn split_cmd(cmd: &str) -> Vec<String> {
    // Minimal shell-like split honoring double quotes.
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in cmd.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn open_with(cmd: &str, path: &Path) -> Result<(), String> {
    if cmd.trim().is_empty() {
        return open_default(path);
    }
    let p = path.to_string_lossy();
    let mut args = split_cmd(cmd);
    if args.iter().any(|a| a.contains("{}")) {
        for a in &mut args {
            *a = a.replace("{}", &p);
        }
    } else {
        args.push(p.into_owned());
    }
    let prog = args.remove(0);
    Command::new(prog)
        .args(args)
        .current_dir(path.parent().unwrap_or(Path::new("/")))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn open_terminal(custom: &str, dir: &Path) -> Result<(), String> {
    if !custom.trim().is_empty() {
        let mut args = split_cmd(custom);
        let prog = args.remove(0);
        return Command::new(prog)
            .args(args)
            .current_dir(dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string());
    }
    #[cfg(target_os = "macos")]
    {
        return Command::new("open")
            .args(["-a", "Terminal"])
            .arg(dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string());
    }
    #[allow(unreachable_code)]
    {
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(t) = std::env::var("TERMINAL") {
            candidates.push(t);
        }
        for t in [
            "ptyxis",
            "konsole",
            "gnome-terminal",
            "kgx",
            "xfce4-terminal",
            "alacritty",
            "kitty",
            "wezterm",
            "foot",
            "xterm",
        ] {
            candidates.push(t.to_string());
        }
        for t in candidates {
            if Command::new(&t).current_dir(dir).spawn().is_ok() {
                return Ok(());
            }
        }
        Err(l!("Kein Terminal gefunden (Einstellungen → Terminal)", "No terminal found (Settings → Terminal)").into())
    }
}

/// Run a command line in `dir` through the user's shell, detached.
pub fn run_shell(cmdline: &str, dir: &Path) -> Result<(), String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    Command::new(shell)
        .arg("-c")
        .arg(cmdline)
        .current_dir(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Recursively sum the size of a directory (does not follow symlinks).
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}

/// A name that does not exist yet in `dir`: "name (2).ext", "name (3).ext", ...
pub fn unique_name(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = split_ext(name, false);
    for i in 2.. {
        let candidate = if ext.is_empty() {
            format!("{stem} ({i})")
        } else {
            format!("{stem} ({i}).{ext}")
        };
        let p = dir.join(candidate);
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod place_tests {
    use super::*;

    #[test]
    fn mount_names() {
        assert_eq!(unescape_mount("/mnt/Mein\\040NAS"), "/mnt/Mein NAS");
        assert_eq!(gvfs_label("smb-share:server=nas,share=daten"), "nas/daten (smb)");
        assert_eq!(gvfs_label("sftp:host=server.local,user=anna"), "server.local (sftp)");
    }

    #[test]
    fn drive_button_labels() {
        let p = |label: &str, kind| Place { label: label.into(), path: "/x".into(), kind };
        assert_eq!(p("/", PlaceKind::Root).short_label(), "/");
        assert_eq!(p("Home", PlaceKind::Home).short_label(), "Home");
        assert_eq!(p("USB STICK", PlaceKind::Removable).short_label(), "USB STICK");
        assert_eq!(p("nas.local/Media (smb)", PlaceKind::Network).short_label(), "Media");
        assert_eq!(p("nas (nas/daten)", PlaceKind::Network).short_label(), "nas");
        assert_eq!(p("server:/export/media", PlaceKind::Network).short_label(), "media");
    }

    #[test]
    fn network_mounts_are_found_anywhere() {
        let text = "\
/dev/nvme0n1p3 / btrfs rw 0 0
//nas/daten /home/anna/nas cifs rw,vers=3.0 0 0
server:/export/media /var/mnt/media nfs4 rw 0 0
anna@pi:/srv /home/anna/pi fuse.sshfs rw 0 0
/dev/sdb1 /run/media/anna/USB\\040STICK vfat rw 0 0
proc /proc proc rw 0 0
tmpfs /tmp tmpfs rw 0 0
";
        let p = parse_mounts(text);
        let got: Vec<(&str, &str, PlaceKind)> =
            p.iter().map(|p| (p.label.as_str(), p.path.to_str().unwrap(), p.kind)).collect();
        assert_eq!(
            got,
            [
                ("nas (nas/daten)", "/home/anna/nas", PlaceKind::Network),
                ("server:/export/media", "/var/mnt/media", PlaceKind::Network),
                ("pi (anna@pi:/srv)", "/home/anna/pi", PlaceKind::Network),
                ("USB STICK", "/run/media/anna/USB STICK", PlaceKind::Removable),
            ]
        );
    }
}


//! "Open with": the applications that can open a file.
//!
//! - Linux: the desktop entries (`*.desktop`) of the installed programs,
//!   matched by MIME type (from the shared MIME database), the default
//!   application first – the same list Dolphin or Nautilus show.
//! - macOS: Launch Services (`LSCopyApplicationURLsForURL`), opened with `open -a`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// An application that can open files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    pub name: String,
    /// Linux: the `Exec` line of the desktop entry; macOS: path of the .app.
    pub exec: String,
    /// The system's default application for this file type.
    pub default: bool,
}

/// Applications for these files (by the first file's type), default first.
pub fn apps_for(path: &Path) -> Vec<App> {
    #[cfg(target_os = "macos")]
    {
        mac::apps_for(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mime = if path.is_dir() { "inode/directory".to_string() } else { mime_db().mime_of(path) };
        static CACHE: OnceLock<Mutex<HashMap<String, Vec<App>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        if let Some(v) = cache.lock().unwrap().get(&mime) {
            return v.clone();
        }
        let v = linux_apps_for(&mime);
        cache.lock().unwrap().insert(mime, v.clone());
        v
    }
}

/// All installed applications (for "Other application…").
pub fn all_apps() -> Vec<App> {
    #[cfg(target_os = "macos")]
    {
        mac::all_apps()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut v: Vec<App> = desktop_entries()
            .iter()
            .map(|e| App { name: e.name.clone(), exec: e.exec.clone(), default: false })
            .collect();
        v.sort_by_key(|a| a.name.to_lowercase());
        v.dedup_by(|a, b| a.name == b.name && a.exec == b.exec);
        v
    }
}

/// Load the application list in the background, so the first right-click is fast.
pub fn warm_up() {
    std::thread::spawn(|| {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = desktop_entries();
            let _ = mime_db();
        }
    });
}

/// Start `app` with the files.
pub fn launch(app: &App, files: &[PathBuf]) -> Result<(), String> {
    if files.is_empty() {
        return Ok(());
    }
    let dir = files[0].parent().unwrap_or(Path::new("/")).to_path_buf();
    let runs = if cfg!(target_os = "macos") && app.exec.ends_with(".app") {
        let mut args = vec!["open".to_string(), "-a".to_string(), app.exec.clone()];
        args.extend(files.iter().map(|f| f.to_string_lossy().into_owned()));
        vec![args]
    } else {
        exec_commands(&app.exec, &app.name, files)
    };
    for mut args in runs {
        if args.is_empty() {
            return Err(l!("Ungültiger Programmaufruf", "Invalid program command").into());
        }
        let prog = args.remove(0);
        std::process::Command::new(&prog)
            .args(args)
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("{prog}: {e}"))?;
    }
    Ok(())
}

/// A command typed by the user ("gimp", "code -n"): the files are appended,
/// or replace `{}`.
pub fn custom(cmd: &str) -> App {
    let exec = if cmd.contains("{}") { cmd.replace("{}", "%F") } else { format!("{} %F", cmd.trim()) };
    App { name: cmd.trim().to_string(), exec, default: false }
}

// ---------------------------------------------------------------------------
// Linux: desktop entries + shared MIME database
// ---------------------------------------------------------------------------

#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Clone, Debug)]
struct DesktopEntry {
    id: String,
    name: String,
    exec: String,
    mime_types: Vec<String>,
}

/// `$XDG_DATA_HOME` first, then `$XDG_DATA_DIRS` (earlier ones win).
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn data_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => v.push(PathBuf::from(d)),
        _ => v.extend(dirs::home_dir().map(|h| h.join(".local/share"))),
    }
    let sys = std::env::var("XDG_DATA_DIRS").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    // Flatpak apps, in case the session doesn't list them.
    v.extend(dirs::home_dir().map(|h| h.join(".local/share/flatpak/exports/share")));
    v.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    let mut seen = std::collections::HashSet::new();
    v.retain(|d| seen.insert(d.clone()));
    v
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn desktop_entries() -> &'static Vec<DesktopEntry> {
    static ENTRIES: OnceLock<Vec<DesktopEntry>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        let mut by_id: HashMap<String, Option<DesktopEntry>> = HashMap::new();
        for dir in data_dirs() {
            let apps = dir.join("applications");
            let mut files = Vec::new();
            collect_desktop_files(&apps, &mut files);
            for f in files {
                let id = f.strip_prefix(&apps).unwrap_or(&f).to_string_lossy().replace('/', "-");
                if by_id.contains_key(&id) {
                    continue; // an earlier directory overrides this one
                }
                let entry = std::fs::read_to_string(&f).ok().and_then(|text| parse_desktop(&id, &text));
                by_id.insert(id, entry);
            }
        }
        let mut v: Vec<DesktopEntry> = by_id.into_values().flatten().collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        v
    })
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn collect_desktop_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_desktop_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "desktop") {
            out.push(p);
        }
    }
}

/// The `[Desktop Entry]` group of a desktop file; `None` for hidden entries
/// and entries that can't open files.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn parse_desktop(id: &str, text: &str) -> Option<DesktopEntry> {
    let german = crate::i18n::system_is_german();
    let mut in_main = false;
    let (mut name, mut name_de, mut exec, mut mime) = (None, None, None, Vec::new());
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Type" if v != "Application" => return None,
            "Hidden" | "NoDisplay" if v == "true" => return None,
            "Name" => name = Some(v.to_string()),
            "Name[de]" => name_de = Some(v.to_string()),
            "Exec" => exec = Some(v.to_string()),
            "MimeType" => mime = v.split(';').filter(|m| !m.is_empty()).map(str::to_string).collect(),
            _ => {}
        }
    }
    let exec = exec?;
    let name = if german { name_de.or(name) } else { name }?;
    Some(DesktopEntry { id: id.to_string(), name, exec, mime_types: mime })
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn linux_apps_for(mime: &str) -> Vec<App> {
    let db = mime_db();
    let types = db.with_parents(mime);
    let (defaults, added, removed) = mimeapps(&types);
    let entries = desktop_entries();
    let by_id: HashMap<&str, &DesktopEntry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut out: Vec<App> = Vec::new();
    let push = |e: &DesktopEntry, default: bool, out: &mut Vec<App>| {
        if !out.iter().any(|a| a.exec == e.exec) {
            out.push(App { name: e.name.clone(), exec: e.exec.clone(), default });
        }
    };
    for id in &defaults {
        if let Some(e) = by_id.get(id.as_str()) {
            push(e, out.is_empty(), &mut out);
        }
    }
    for id in &added {
        if let Some(e) = by_id.get(id.as_str()) {
            push(e, false, &mut out);
        }
    }
    // Exact type first, then the more general ones (text/plain for source code …).
    for t in &types {
        for e in entries {
            if e.mime_types.iter().any(|m| m == t) && !removed.contains(&e.id) {
                push(e, false, &mut out);
            }
        }
    }
    out
}

/// Default, added and removed applications from the `mimeapps.list` files.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn mimeapps(types: &[String]) -> (Vec<String>, Vec<String>, Vec<String>) {
    // In each directory the desktop's own list ("kde-mimeapps.list") comes first.
    let desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| format!("{}-mimeapps.list", d.to_lowercase()))
        .chain(["mimeapps.list".to_string()])
        .collect();
    let mut dirs_in_order: Vec<PathBuf> = Vec::new();
    dirs_in_order.extend(dirs::config_dir());
    for d in std::env::var("XDG_CONFIG_DIRS").unwrap_or_else(|_| "/etc/xdg".into()).split(':') {
        dirs_in_order.push(PathBuf::from(d));
    }
    for d in data_dirs() {
        dirs_in_order.push(d.join("applications"));
    }
    let files = dirs_in_order.iter().flat_map(|d| desktops.iter().map(move |f| d.join(f)));
    let (mut defaults, mut added, mut removed) = (Vec::new(), Vec::new(), Vec::new());
    for f in files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        parse_mimeapps(&text, types, &mut defaults, &mut added, &mut removed);
    }
    (defaults, added, removed)
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn parse_mimeapps(text: &str, types: &[String], defaults: &mut Vec<String>, added: &mut Vec<String>, removed: &mut Vec<String>) {
    let mut group = "";
    // Only the most specific type's default counts as "the" default.
    for t in types {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                group = line;
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            if k.trim() != t {
                continue;
            }
            let target = match group {
                "[Default Applications]" => &mut *defaults,
                "[Added Associations]" => &mut *added,
                "[Removed Associations]" => &mut *removed,
                _ => continue,
            };
            for id in v.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                if !target.iter().any(|x| x == id) {
                    target.push(id.to_string());
                }
            }
        }
    }
}

/// File name → MIME type, from the shared MIME database's glob patterns.
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Default)]
struct MimeDb {
    /// "tar.gz" → (weight, type); the longest matching suffix wins.
    suffixes: HashMap<String, (u32, String)>,
    /// Literal file names: "makefile" → type.
    names: HashMap<String, String>,
    /// type → parent types (text/x-rust → text/plain).
    parents: HashMap<String, Vec<String>>,
    /// alias → canonical type.
    aliases: HashMap<String, String>,
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn mime_db() -> &'static MimeDb {
    static DB: OnceLock<MimeDb> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = MimeDb::default();
        // Later (system) directories first, so user definitions override them.
        for dir in data_dirs().iter().rev() {
            let m = dir.join("mime");
            if let Ok(t) = std::fs::read_to_string(m.join("globs2")) {
                db.add_globs(&t);
            }
            if let Ok(t) = std::fs::read_to_string(m.join("subclasses")) {
                db.add_pairs(&t, false);
            }
            if let Ok(t) = std::fs::read_to_string(m.join("aliases")) {
                db.add_pairs(&t, true);
            }
        }
        db
    })
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
impl MimeDb {
    fn add_globs(&mut self, text: &str) {
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let mut parts = line.split(':');
            let (Some(w), Some(mime), Some(glob)) = (parts.next(), parts.next(), parts.next()) else { continue };
            let weight: u32 = w.parse().unwrap_or(50);
            let glob = glob.to_lowercase();
            if let Some(suffix) = glob.strip_prefix("*.") {
                if !suffix.contains(['*', '?', '[']) {
                    let keep = self.suffixes.get(suffix).is_some_and(|(old, _)| *old > weight);
                    if !keep {
                        self.suffixes.insert(suffix.to_string(), (weight, mime.to_string()));
                    }
                }
            } else if !glob.contains(['*', '?', '[']) {
                self.names.insert(glob, mime.to_string());
            }
        }
    }

    fn add_pairs(&mut self, text: &str, aliases: bool) {
        for line in text.lines() {
            let Some((a, b)) = line.split_once(' ') else { continue };
            if aliases {
                self.aliases.insert(a.to_string(), b.to_string());
            } else {
                let v = self.parents.entry(a.to_string()).or_default();
                if !v.iter().any(|x| x == b) {
                    v.push(b.to_string());
                }
            }
        }
    }

    fn mime_of(&self, path: &Path) -> String {
        let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        if let Some(m) = self.names.get(&name) {
            return m.clone();
        }
        // Longest suffix first: "x.tar.gz" → "tar.gz" before "gz".
        let mut rest = name.as_str();
        while let Some(i) = rest.find('.') {
            rest = &rest[i + 1..];
            if let Some((_, m)) = self.suffixes.get(rest) {
                return m.clone();
            }
        }
        if looks_like_text(path) { "text/plain".into() } else { "application/octet-stream".into() }
    }

    /// The type itself, then its parents (breadth first).
    fn with_parents(&self, mime: &str) -> Vec<String> {
        let canonical = self.aliases.get(mime).cloned().unwrap_or_else(|| mime.to_string());
        let mut out = vec![canonical];
        let mut i = 0;
        while i < out.len() && out.len() < 16 {
            let mut next: Vec<String> = self.parents.get(&out[i]).cloned().unwrap_or_default();
            // Every text file can also be opened as plain text.
            if out[i].starts_with("text/") && out[i] != "text/plain" {
                next.push("text/plain".into());
            }
            for p in next {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
            i += 1;
        }
        out
    }
}

/// No NUL bytes and valid UTF-8 in the first 4 KB.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn looks_like_text(path: &Path) -> bool {
    use std::io::Read;
    let mut buf = [0u8; 4096];
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let n = f.read(&mut buf).unwrap_or(0);
    let head = &buf[..n];
    !head.contains(&0)
        && match std::str::from_utf8(head) {
            Ok(_) => true,
            // A multi-byte character cut off at the end is fine.
            Err(e) => e.error_len().is_none(),
        }
}

/// Split an `Exec` line into arguments (desktop entry quoting rules).
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn split_exec(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ' ' | '\t' if !quoted => {
                if in_arg {
                    out.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            c => {
                cur.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        out.push(cur);
    }
    out
}

/// The command line(s) for the files: `%F`/`%U` take all files at once,
/// `%f`/`%u` start the program once per file; without a field code the
/// files are appended.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn exec_commands(exec: &str, name: &str, files: &[PathBuf]) -> Vec<Vec<String>> {
    let args = split_exec(exec);
    let many = args.iter().any(|a| a == "%F" || a == "%U");
    let single = args.iter().any(|a| a.contains("%f") || a.contains("%u"));
    let expand = |files: &[PathBuf]| -> Vec<String> {
        let mut out = Vec::new();
        let mut used = false;
        for a in &args {
            match a.as_str() {
                "%F" | "%U" => {
                    out.extend(files.iter().map(|f| f.to_string_lossy().into_owned()));
                    used = true;
                }
                "%i" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" | "%k" => {}
                _ => {
                    let mut s = a.clone();
                    if s.contains("%f") || s.contains("%u") {
                        let f = files.first().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                        s = s.replace("%f", &f).replace("%u", &f);
                        used = true;
                    }
                    out.push(s.replace("%c", name).replace("%%", "%"));
                }
            }
        }
        if !used {
            out.extend(files.iter().map(|f| f.to_string_lossy().into_owned()));
        }
        out
    };
    if single && !many {
        files.iter().map(|f| expand(std::slice::from_ref(f))).collect()
    } else {
        vec![expand(files)]
    }
}

// ---------------------------------------------------------------------------
// macOS: Launch Services
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod mac {
    use super::App;
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    type CFRef = *const c_void;
    const ROLES_ALL: u32 = 0xFFFF_FFFF;

    #[link(name = "CoreFoundation", kind = "framework")]
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(alloc: CFRef, buf: *const u8, len: isize, is_dir: u8) -> CFRef;
        fn CFURLGetFileSystemRepresentation(url: CFRef, resolve: u8, buf: *mut u8, max: isize) -> u8;
        fn CFArrayGetCount(array: CFRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFRef, i: isize) -> CFRef;
        fn CFRelease(r: CFRef);
        fn LSCopyApplicationURLsForURL(url: CFRef, roles: u32) -> CFRef;
        fn LSCopyDefaultApplicationURLForURL(url: CFRef, roles: u32, err: *mut CFRef) -> CFRef;
    }

    fn url_path(url: CFRef) -> Option<PathBuf> {
        let mut buf = vec![0u8; 4096];
        // SAFETY: `url` is a valid CFURL and `buf` is writable for its length.
        let ok = unsafe { CFURLGetFileSystemRepresentation(url, 1, buf.as_mut_ptr(), buf.len() as isize) };
        if ok == 0 {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..end])))
    }

    fn app(path: PathBuf, default: bool) -> App {
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        App { name, exec: path.to_string_lossy().into_owned(), default }
    }

    pub fn apps_for(path: &Path) -> Vec<App> {
        let bytes = path.as_os_str().as_bytes();
        let mut out: Vec<App> = Vec::new();
        // SAFETY: plain CoreFoundation calls; every object we create or copy is released.
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, path.is_dir() as u8);
            if url.is_null() {
                return out;
            }
            let def = LSCopyDefaultApplicationURLForURL(url, ROLES_ALL, std::ptr::null_mut());
            if !def.is_null() {
                if let Some(p) = url_path(def) {
                    out.push(app(p, true));
                }
                CFRelease(def);
            }
            let arr = LSCopyApplicationURLsForURL(url, ROLES_ALL);
            if !arr.is_null() {
                for i in 0..CFArrayGetCount(arr) {
                    if let Some(p) = url_path(CFArrayGetValueAtIndex(arr, i)) {
                        let a = app(p, false);
                        if !out.iter().any(|x| x.exec == a.exec) {
                            out.push(a);
                        }
                    }
                }
                CFRelease(arr);
            }
            CFRelease(url);
        }
        if out.len() > 1 {
            out[1..].sort_by_key(|a| a.name.to_lowercase());
        }
        out
    }

    pub fn all_apps() -> Vec<App> {
        let mut dirs = vec![PathBuf::from("/Applications"), PathBuf::from("/Applications/Utilities"), PathBuf::from("/System/Applications"), PathBuf::from("/System/Applications/Utilities")];
        dirs.extend(::dirs::home_dir().map(|h| h.join("Applications")));
        let mut out: Vec<App> = dirs
            .iter()
            .filter_map(|d| std::fs::read_dir(d).ok())
            .flat_map(|rd| rd.flatten())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "app"))
            .map(|p| app(p, false))
            .collect();
        out.sort_by_key(|a| a.name.to_lowercase());
        out.dedup_by(|a, b| a.name == b.name);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_lines() {
        let f = |v: &[&str]| v.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(exec_commands("kate -b %U", "Kate", &f(&["/a b.txt", "/c.txt"])), vec![vec!["kate", "-b", "/a b.txt", "/c.txt"]]);
        assert_eq!(exec_commands("gimp %f", "GIMP", &f(&["/1.png", "/2.png"])), vec![vec!["gimp", "/1.png"], vec!["gimp", "/2.png"]]);
        assert_eq!(exec_commands("\"/opt/my app/run\" --name=%c", "App", &f(&["/x"])), vec![vec!["/opt/my app/run", "--name=App", "/x"]]);
        assert_eq!(exec_commands("vlc --started-from-file %i %U", "VLC", &f(&["/v.mkv"])), vec![vec!["vlc", "--started-from-file", "/v.mkv"]]);
        assert_eq!(
            exec_commands("/usr/bin/flatpak run --file-forwarding org.gimp.GIMP @@ %F @@", "GIMP", &f(&["/p.png"])),
            vec![vec!["/usr/bin/flatpak", "run", "--file-forwarding", "org.gimp.GIMP", "@@", "/p.png", "@@"]]
        );
        assert_eq!(custom("code -n").exec, "code -n %F");
        assert_eq!(exec_commands(&custom("cp {} /tmp").exec, "", &f(&["/q"])), vec![vec!["cp", "/q", "/tmp"]]);
    }

    #[test]
    fn desktop_files_and_mime_types() {
        let e = parse_desktop("org.kde.kate.desktop", "[Desktop Entry]\nType=Application\nName=Kate\nName[de]=Kate DE\nExec=kate -b %U\nMimeType=text/plain;text/x-rust;\n\n[Desktop Action new]\nExec=kate --new\n").unwrap();
        assert_eq!((e.name.as_str(), e.exec.as_str()), ("Kate DE", "kate -b %U")); // tests run "German"
        assert_eq!(e.mime_types, ["text/plain", "text/x-rust"]);
        assert!(parse_desktop("x.desktop", "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n").is_none());
        assert!(parse_desktop("l.desktop", "[Desktop Entry]\nType=Link\nName=L\n").is_none());

        let mut db = MimeDb::default();
        db.add_globs("# comment\n50:text/x-rust:*.rs\n50:application/gzip:*.gz\n50:application/x-compressed-tar:*.tar.gz\n50:text/x-makefile:makefile\n");
        db.add_pairs("text/x-rust text/plain\napplication/x-compressed-tar application/gzip\n", false);
        db.add_pairs("application/x-gzip application/gzip\n", true);
        assert_eq!(db.mime_of(Path::new("/x/main.RS")), "text/x-rust");
        assert_eq!(db.mime_of(Path::new("/x/a.b.tar.gz")), "application/x-compressed-tar");
        assert_eq!(db.mime_of(Path::new("/x/Makefile")), "text/x-makefile");
        assert_eq!(db.with_parents("application/x-compressed-tar"), ["application/x-compressed-tar", "application/gzip"]);
        assert_eq!(db.with_parents("text/x-makefile"), ["text/x-makefile", "text/plain"]);
        assert_eq!(db.with_parents("application/x-gzip"), ["application/gzip"]);

        let (mut d, mut a, mut r) = (Vec::new(), Vec::new(), Vec::new());
        let types = vec!["text/x-rust".to_string(), "text/plain".to_string()];
        parse_mimeapps("[Default Applications]\ntext/plain=org.kde.kate.desktop;\ntext/x-rust=code.desktop\n[Added Associations]\ntext/x-rust=gvim.desktop;\n[Removed Associations]\ntext/plain=nano.desktop\n", &types, &mut d, &mut a, &mut r);
        assert_eq!(d, ["code.desktop", "org.kde.kate.desktop"]);
        assert_eq!(a, ["gvim.desktop"]);
        assert_eq!(r, ["nano.desktop"]);
    }

    /// Shows what this machine offers: cargo test real_apps -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_apps() {
        for f in ["src/main.rs", "assets/icon.png", "README.md", "Cargo.toml", "src"] {
            let apps: Vec<String> = apps_for(Path::new(f)).into_iter().map(|a| format!("{}{}", a.name, if a.default { "★" } else { "" })).collect();
            println!("{f}: {}", apps.join(", "));
        }
        println!("all: {}", all_apps().len());
    }

    #[test]
    fn text_detection() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("notes");
        std::fs::write(&t, "Grüße\n").unwrap();
        let b = dir.path().join("blob");
        std::fs::write(&b, [0u8, 1, 2, 3]).unwrap();
        assert!(looks_like_text(&t));
        assert!(!looks_like_text(&b));
    }
}

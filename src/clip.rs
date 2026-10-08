//! File clipboard (Ctrl+C / Ctrl+X / Ctrl+V).
//!
//! Inside the app we remember exactly what was copied (also from servers and
//! archives). Local files are additionally offered to the system clipboard as
//! `text/uri-list`, so Dolphin, Nautilus & co. can paste them – and files
//! copied there can be pasted here (Wayland via wl-clipboard).

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub enum ClipSource {
    Local,
    /// FTP/SFTP connection id.
    Remote(usize),
    /// Inside a zip: archive file and the directory inside it.
    Archive { file: PathBuf, inner: String },
}

#[derive(Clone, Debug)]
pub struct FileClip {
    pub paths: Vec<PathBuf>,
    /// Which of `paths` are directories (needed for remote downloads).
    pub dirs: Vec<PathBuf>,
    pub cut: bool,
    pub source: ClipSource,
}

fn percent_encode_path(p: &str) -> String {
    let mut out = String::new();
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "file:///a%20b" lines → paths (other schemes are ignored).
pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("file://"))
        .map(|rest| {
            // "file://host/path" – drop a host part if present.
            let path = if rest.starts_with('/') { rest } else { rest.find('/').map(|i| &rest[i..]).unwrap_or(rest) };
            PathBuf::from(percent_decode(path))
        })
        .collect()
}

pub fn to_uri_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("file://{}\r\n", percent_encode_path(&p.to_string_lossy())))
        .collect()
}

/// Offer local files to the system clipboard (best effort).
pub fn to_system(paths: &[PathBuf]) {
    if cfg!(test) {
        return; // never touch the developer's real clipboard from tests
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        if let Ok(mut child) = std::process::Command::new("wl-copy")
            .args(["--type", "text/uri-list"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(to_uri_list(paths).as_bytes());
            }
            // wl-copy forks and keeps serving the clipboard; reap the parent.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = paths;
    }
}

/// Run a short command, give up after `secs`.
#[cfg(target_os = "linux")]
fn run_with_timeout(cmd: &str, args: &[&str], secs: u64) -> Option<String> {
    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed().as_secs() >= secs => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Files currently in the system clipboard and whether they were cut there.
pub fn from_system() -> Option<(Vec<PathBuf>, bool)> {
    if cfg!(test) {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let types = run_with_timeout("wl-paste", &["--list-types"], 1)?;
        if !types.lines().any(|t| t.trim() == "text/uri-list") {
            return None;
        }
        let list = run_with_timeout("wl-paste", &["--no-newline", "--type", "text/uri-list"], 1)?;
        let paths = parse_uri_list(&list);
        if paths.is_empty() {
            return None;
        }
        // KDE marks "cut" with application/x-kde-cutselection = 1, GNOME with "cut" in its list.
        let cut = if types.lines().any(|t| t.trim() == "application/x-kde-cutselection") {
            run_with_timeout("wl-paste", &["--no-newline", "--type", "application/x-kde-cutselection"], 1)
                .is_some_and(|v| v.trim() == "1")
        } else if types.lines().any(|t| t.trim() == "x-special/gnome-copied-files") {
            run_with_timeout("wl-paste", &["--no-newline", "--type", "x-special/gnome-copied-files"], 1)
                .is_some_and(|v| v.lines().next() == Some("cut"))
        } else {
            false
        };
        Some((paths, cut))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists() {
        let paths = vec![PathBuf::from("/home/l/Mein Film (2024).mp4"), PathBuf::from("/tmp/日本語.txt")];
        let list = to_uri_list(&paths);
        assert!(list.starts_with("file:///home/l/Mein%20Film%20%282024%29.mp4\r\n"));
        assert_eq!(parse_uri_list(&list), paths);
        // Comments, other schemes and a host part are handled.
        assert_eq!(
            parse_uri_list("# kommentar\nfile://localhost/etc/hosts\nsmb://nas/x\n"),
            [PathBuf::from("/etc/hosts")]
        );
    }
}

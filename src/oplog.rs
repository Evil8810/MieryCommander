//! Operations log: what was copied, moved, deleted … – when, where to and
//! with which result. Kept in memory for the log window and appended to a
//! plain text file, so it can still be read after a restart.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Debug, Default)]
pub struct Record {
    pub time: String,
    pub title: String,
    /// "3 Elemente: a.txt, b.txt, …"
    pub what: String,
    pub dest: String,
    pub result: String,
    pub ok: bool,
    pub errors: Vec<String>,
}

/// Most records the window shows (the file keeps more).
const MAX_RECORDS: usize = 500;
/// The file is cut down to its newer half beyond this size.
const MAX_FILE: u64 = 1 << 20;

static LOG: Mutex<Vec<Record>> = Mutex::new(Vec::new());

/// Where the log file lives (none in tests: they never touch the user's files).
pub fn file() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    Some(dirs::data_local_dir()?.join("MieryCommander").join("operations.log"))
}

pub fn records() -> Vec<Record> {
    LOG.lock().unwrap().clone()
}

pub fn clear() {
    LOG.lock().unwrap().clear();
    if let Some(f) = file() {
        std::thread::spawn(move || {
            let _ = std::fs::remove_file(f);
        });
    }
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn to_lines(r: &Record) -> String {
    let mut s = format!(
        "{}\t{}\t{}\t{}\t{}\t{}\n",
        clean(&r.time),
        clean(&r.title),
        clean(&r.what),
        clean(&r.dest),
        clean(&r.result),
        if r.ok { "ok" } else { "error" }
    );
    for e in &r.errors {
        s.push_str(&format!("\t! {}\n", clean(e)));
    }
    s
}

fn parse(text: &str) -> Vec<Record> {
    let mut v: Vec<Record> = Vec::new();
    for line in text.lines() {
        if let Some(e) = line.strip_prefix("\t! ") {
            if let Some(last) = v.last_mut() {
                last.errors.push(e.to_string());
            }
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() == 6 {
            v.push(Record {
                time: f[0].into(),
                title: f[1].into(),
                what: f[2].into(),
                dest: f[3].into(),
                result: f[4].into(),
                ok: f[5] == "ok",
                errors: Vec::new(),
            });
        }
    }
    v
}

/// Read the records of earlier sessions (in the background, at start).
pub fn load_in_background() {
    let Some(f) = file() else { return };
    std::thread::spawn(move || {
        let Ok(text) = std::fs::read_to_string(&f) else { return };
        let mut old = parse(&text);
        let skip = old.len().saturating_sub(MAX_RECORDS);
        old.drain(..skip);
        let mut log = LOG.lock().unwrap();
        // Anything logged meanwhile is newer.
        old.append(&mut log);
        *log = old;
    });
}

/// Describe the sources of a job: count and the first few names.
pub fn describe(sources: &[PathBuf]) -> String {
    let names: Vec<String> = sources
        .iter()
        .take(3)
        .map(|p| if p.as_os_str().is_empty() { "*".into() } else { p.to_string_lossy().into_owned() })
        .collect();
    let more = if sources.len() > 3 { ", …" } else { "" };
    match sources.len() {
        0 => String::new(),
        1 => names[0].clone(),
        n => format!("{n}: {}{more}", names.join(", ")),
    }
}

pub fn add(mut r: Record) {
    r.time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let lines = to_lines(&r);
    {
        let mut log = LOG.lock().unwrap();
        log.push(r);
        let skip = log.len().saturating_sub(MAX_RECORDS);
        log.drain(..skip);
    }
    let Some(f) = file() else { return };
    // Never on the UI thread: the disk may be slow.
    std::thread::spawn(move || {
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(&f).is_ok_and(|m| m.len() > MAX_FILE)
            && let Ok(text) = std::fs::read_to_string(&f)
        {
            let half = text.len() / 2;
            let cut = text[half..].find('\n').map(|i| half + i + 1).unwrap_or(text.len());
            // (a boundary inside a multi-byte character: keep the file as it is)
            if text.is_char_boundary(cut) {
                let _ = std::fs::write(&f, &text[cut..]);
            }
        }
        if let Ok(mut out) = std::fs::File::options().create(true).append(true).open(&f) {
            let _ = out.write_all(lines.as_bytes());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_survive_the_text_format() {
        let r = Record {
            time: "2026-01-02 03:04:05".into(),
            title: "Kopieren".into(),
            what: "2: /a/x\ty, /a/z".into(),
            dest: "/b".into(),
            result: "1 Fehler".into(),
            ok: false,
            errors: vec!["x: kaputt\nzweite Zeile".into()],
        };
        let back = parse(&(to_lines(&r) + &to_lines(&Record { ok: true, ..Default::default() })));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].what, "2: /a/x y, /a/z");
        assert_eq!(back[0].errors, ["x: kaputt zweite Zeile"]);
        assert!(!back[0].ok && back[1].ok);
    }
}

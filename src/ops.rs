//! File operations that run on a worker thread and report progress.

use crate::archive;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::remote::{self, Remote};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum JobKind {
    Copy,
    Move,
    Delete { to_trash: bool },
    /// Extract `sources` (paths inside the archive) from `archive`,
    /// stripping `base` (the archive dir the user is looking at).
    Extract { archive: PathBuf, base: String },
    /// Pack `sources` into the zip file `dest`.
    Pack,
    /// Upload local `sources` into the remote directory `dest`.
    /// `overwrite`: replace existing files without asking (re-upload after editing).
    Upload { conn: usize, delete_source: bool, overwrite: bool },
    /// Download remote `sources` (dirs listed in `dirs`) into the local dir `dest`.
    Download { conn: usize, dirs: Vec<PathBuf>, delete_source: bool },
    /// Delete remote `sources` (dirs listed in `dirs`).
    FtpDelete { conn: usize, dirs: Vec<PathBuf> },
    /// Unpack whole archives into `dest`; `smart` = own folder when needed.
    Unpack { smart: bool },
    /// Synchronize directories: run the planned copy/delete operations.
    Sync { ops: Vec<crate::sync::Op>, to_trash: bool },
    /// Add local `sources` to `archive`, into its folder `inner` ("" or "dir/").
    ArchiveAdd { archive: PathBuf, inner: String, delete_source: bool },
    /// Delete `sources` (paths inside the archive) from `archive`.
    ArchiveDelete { archive: PathBuf },
    /// Copy remote `sources` of connection `from` into the remote dir `dest`
    /// of connection `to` (`dirs`: which sources are directories).
    RemoteCopy { from: usize, to: usize, dirs: Vec<PathBuf>, delete_source: bool },
}

impl JobKind {
    pub fn title(&self) -> &'static str {
        match self {
            JobKind::Copy => l!("Kopieren", "Copy"),
            JobKind::Move => l!("Verschieben", "Move"),
            JobKind::Delete { .. } => l!("Löschen", "Delete"),
            JobKind::Extract { .. } => l!("Entpacken", "Unpack"),
            JobKind::Pack => l!("Packen", "Pack"),
            JobKind::Upload { .. } => l!("Hochladen", "Upload"),
            JobKind::Download { .. } => l!("Herunterladen", "Download"),
            JobKind::FtpDelete { .. } => l!("Löschen (FTP)", "Delete (FTP)"),
            JobKind::Sync { .. } => l!("Synchronisieren", "Synchronize"),
            JobKind::Unpack { .. } => l!("Entpacken", "Unpack"),
            JobKind::ArchiveAdd { delete_source: false, .. } => l!("In Archiv kopieren", "Copy into archive"),
            JobKind::ArchiveAdd { delete_source: true, .. } => l!("In Archiv verschieben", "Move into archive"),
            JobKind::ArchiveDelete { .. } => l!("Aus Archiv löschen", "Delete from archive"),
            JobKind::RemoteCopy { delete_source: false, .. } => l!("Server → Server kopieren", "Copy server → server"),
            JobKind::RemoteCopy { delete_source: true, .. } => l!("Server → Server verschieben", "Move server → server"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OverwriteAnswer {
    Overwrite,
    OverwriteAll,
    OverwriteOlder,
    Skip,
    SkipAll,
    Rename,
    Cancel,
}

#[derive(Default)]
pub struct Progress {
    pub current: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub finished: bool,
    pub errors: Vec<String>,
    /// Set by the worker when it needs an overwrite decision: (source, target).
    pub question: Option<(PathBuf, PathBuf)>,
}

pub struct Job {
    pub kind: JobKind,
    pub progress: Arc<Mutex<Progress>>,
    pub cancel: Arc<AtomicBool>,
    pub answer_tx: Sender<OverwriteAnswer>,
    pub started: std::time::Instant,
}

impl Job {
    pub fn start(kind: JobKind, sources: Vec<PathBuf>, dest: PathBuf, ctx: eframe::egui::Context) -> Job {
        Self::start_with_policy(kind, sources, dest, ctx, None)
    }

    /// Like `start`, with a preset answer for existing targets
    /// (e.g. `Rename` when pasting a copy into the same folder).
    pub fn start_with_policy(
        kind: JobKind,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        ctx: eframe::egui::Context,
        policy: Option<OverwriteAnswer>,
    ) -> Job {
        let progress = Arc::new(Mutex::new(Progress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (answer_tx, answer_rx) = channel();
        let mut worker = Worker {
            progress: progress.clone(),
            cancel: cancel.clone(),
            answers: answer_rx,
            policy,
            ctx,
        };
        let k = kind.clone();
        std::thread::spawn(move || {
            worker.run(&k, &sources, &dest);
            worker.progress.lock().unwrap().finished = true;
            worker.ctx.request_repaint();
        });
        Job {
            kind,
            progress,
            cancel,
            answer_tx,
            started: std::time::Instant::now(),
        }
    }
}

struct Worker {
    progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    answers: Receiver<OverwriteAnswer>,
    /// Sticky answer after "all" choices.
    policy: Option<OverwriteAnswer>,
    ctx: eframe::egui::Context,
}

enum Target {
    Write(PathBuf),
    Skip,
}

enum Decision {
    Write,
    Skip,
    Rename,
}

impl Worker {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn err(&self, msg: String) {
        self.progress.lock().unwrap().errors.push(msg);
    }

    fn set_current(&self, s: &Path) {
        self.progress.lock().unwrap().current = s.to_string_lossy().into_owned();
        self.ctx.request_repaint();
    }

    fn add_bytes(&self, n: u64) {
        self.progress.lock().unwrap().bytes_done += n;
    }

    fn file_done(&self) {
        self.progress.lock().unwrap().files_done += 1;
        self.ctx.request_repaint();
    }

    fn run(&mut self, kind: &JobKind, sources: &[PathBuf], dest: &Path) {
        match kind {
            JobKind::Copy | JobKind::Move => {
                let (files, bytes) = count(sources);
                {
                    let mut p = self.progress.lock().unwrap();
                    p.files_total = files;
                    p.bytes_total = bytes;
                }
                let is_move = matches!(kind, JobKind::Move);
                // A single source and a destination that isn't an existing dir = rename/copy-as.
                let single_target = sources.len() == 1 && !dest.is_dir();
                for src in sources {
                    if self.cancelled() {
                        break;
                    }
                    let target = if single_target {
                        dest.to_path_buf()
                    } else {
                        dest.join(src.file_name().unwrap_or_default())
                    };
                    // Copy onto itself with "rename" preset (paste into the same
                    // folder): make a "name (2)" copy instead of refusing.
                    let target = if target == *src && !is_move && self.policy == Some(OverwriteAnswer::Rename) {
                        let name = src.file_name().unwrap_or_default().to_string_lossy();
                        crate::fsutil::unique_name(target.parent().unwrap_or(Path::new("/")), &name)
                    } else {
                        target
                    };
                    if target == *src || target.starts_with(src) && src.is_dir() {
                        self.err(lf!("{}: Ziel liegt in der Quelle", "{}: target is inside the source",
                            src.to_string_lossy()
                        ));
                        continue;
                    }
                    if is_move {
                        self.move_item(src, &target);
                    } else {
                        self.copy_item(src, &target);
                    }
                }
            }
            JobKind::Delete { to_trash } => {
                let mut p = self.progress.lock().unwrap();
                p.files_total = sources.len() as u64;
                drop(p);
                for src in sources {
                    if self.cancelled() {
                        break;
                    }
                    self.set_current(src);
                    let r = if *to_trash {
                        trash::delete(src).map_err(|e| e.to_string())
                    } else {
                        remove_any(src).map_err(|e| e.to_string())
                    };
                    if let Err(e) = r {
                        self.err(format!("{}: {e}", src.to_string_lossy()));
                    }
                    self.file_done();
                }
            }
            JobKind::Extract { archive, base } => self.extract(archive, base, sources, dest),
            JobKind::Pack => self.pack(sources, dest),
            JobKind::Sync { ops, to_trash } => self.sync(ops, *to_trash),
            JobKind::Unpack { smart } => self.unpack(sources, dest, *smart),
            JobKind::Upload { conn, delete_source, overwrite } => match remote::get(*conn) {
                Some(c) => {
                    if *overwrite {
                        self.policy = Some(OverwriteAnswer::OverwriteAll);
                    }
                    self.upload(c.as_ref(), sources, &dest.to_string_lossy(), *delete_source)
                }
                None => self.err(l!("Verbindung zum Server ist getrennt", "The connection to the server is closed").into()),
            },
            JobKind::Download { conn, dirs, delete_source } => match remote::get(*conn) {
                Some(c) => self.download(c.as_ref(), sources, dirs, dest, *delete_source),
                None => self.err(l!("Verbindung zum Server ist getrennt", "The connection to the server is closed").into()),
            },
            JobKind::FtpDelete { conn, dirs } => match remote::get(*conn) {
                Some(c) => self.ftp_delete(c.as_ref(), sources, dirs),
                None => self.err(l!("Verbindung zum Server ist getrennt", "The connection to the server is closed").into()),
            },
            JobKind::ArchiveAdd { archive, inner, delete_source } => self.archive_add(archive, inner, sources, *delete_source),
            JobKind::ArchiveDelete { archive } => self.archive_delete(archive, sources),
            JobKind::RemoteCopy { from, to, dirs, delete_source } => match (remote::get(*from), remote::get(*to)) {
                (Some(a), Some(b)) => self.remote_copy(a.as_ref(), b.as_ref(), sources, dirs, &dest.to_string_lossy(), *delete_source),
                _ => self.err(l!("Verbindung zum Server ist getrennt", "The connection to the server is closed").into()),
            },
        }
    }

    /// Ask (or apply the sticky "all" answer) what to do with an existing target.
    fn decide(&mut self, src: &Path, target: &Path, src_is_newer: bool) -> Decision {
        let answer = match self.policy {
            Some(p) => p,
            None => {
                self.progress.lock().unwrap().question =
                    Some((src.to_path_buf(), target.to_path_buf()));
                self.ctx.request_repaint();
                let a = self.answers.recv().unwrap_or(OverwriteAnswer::Cancel);
                self.progress.lock().unwrap().question = None;
                a
            }
        };
        match answer {
            OverwriteAnswer::Overwrite => Decision::Write,
            OverwriteAnswer::OverwriteAll => {
                self.policy = Some(OverwriteAnswer::OverwriteAll);
                Decision::Write
            }
            OverwriteAnswer::OverwriteOlder => {
                self.policy = Some(OverwriteAnswer::OverwriteOlder);
                if src_is_newer { Decision::Write } else { Decision::Skip }
            }
            OverwriteAnswer::Skip => Decision::Skip,
            OverwriteAnswer::SkipAll => {
                self.policy = Some(OverwriteAnswer::SkipAll);
                Decision::Skip
            }
            OverwriteAnswer::Rename => Decision::Rename,
            OverwriteAnswer::Cancel => {
                self.cancel.store(true, Ordering::Relaxed);
                Decision::Skip
            }
        }
    }

    /// Decide what to do with a local target. `src_mtime` defaults to the local source's mtime.
    fn resolve_local(&mut self, src: &Path, target: &Path, src_mtime: Option<SystemTime>) -> Target {
        if fs::symlink_metadata(target).is_err() {
            return Target::Write(target.to_path_buf());
        }
        let src_mtime = src_mtime.or_else(|| fs::metadata(src).and_then(|m| m.modified()).ok());
        let dst_mtime = fs::metadata(target).and_then(|m| m.modified()).ok();
        let newer = matches!((src_mtime, dst_mtime), (Some(a), Some(b)) if a > b);
        match self.decide(src, target, newer) {
            Decision::Write => Target::Write(target.to_path_buf()),
            Decision::Skip => Target::Skip,
            Decision::Rename => {
                let dir = target.parent().unwrap_or(Path::new("/"));
                let name = target.file_name().unwrap_or_default().to_string_lossy();
                Target::Write(crate::fsutil::unique_name(dir, &name))
            }
        }
    }

    fn resolve_target(&mut self, src: &Path, target: &Path) -> Target {
        self.resolve_local(src, target, None)
    }

    fn copy_item(&mut self, src: &Path, target: &Path) {
        if self.cancelled() {
            return;
        }
        let Ok(meta) = fs::symlink_metadata(src) else {
            self.err(lf!("{}: nicht lesbar", "{}: not readable", src.to_string_lossy()));
            return;
        };
        if meta.file_type().is_symlink() {
            #[cfg(unix)]
            if let Target::Write(t) = self.resolve_target(src, target) {
                let _ = remove_any(&t);
                let r = fs::read_link(src).and_then(|l| std::os::unix::fs::symlink(l, &t));
                if let Err(e) = r {
                    self.err(format!("{}: {e}", src.to_string_lossy()));
                }
            }
            self.file_done();
        } else if meta.is_dir() {
            if !target.is_dir()
                && let Err(e) = fs::create_dir_all(target)
            {
                self.err(format!("{}: {e}", target.to_string_lossy()));
                return;
            }
            let _ = fs::set_permissions(target, meta.permissions());
            match fs::read_dir(src) {
                Ok(rd) => {
                    for e in rd.flatten() {
                        self.copy_item(&e.path(), &target.join(e.file_name()));
                    }
                }
                Err(e) => self.err(format!("{}: {e}", src.to_string_lossy())),
            }
        } else {
            if let Target::Write(t) = self.resolve_target(src, target)
                && let Err(e) = self.copy_file(src, &t, &meta)
            {
                self.err(format!("{}: {e}", src.to_string_lossy()));
                let _ = fs::remove_file(&t);
            }
            self.file_done();
        }
    }

    fn copy_file(&self, src: &Path, target: &Path, meta: &fs::Metadata) -> std::io::Result<()> {
        self.set_current(src);
        let mut input = File::open(src)?;
        let mut output = File::create(target)?;
        self.pump(&mut input, &mut output)?;
        drop(output);
        let _ = fs::set_permissions(target, meta.permissions());
        if let Ok(mtime) = meta.modified() {
            let _ = File::options()
                .write(true)
                .open(target)
                .and_then(|f| f.set_modified(mtime));
        }
        Ok(())
    }

    fn pump(&self, input: &mut dyn Read, output: &mut dyn Write) -> std::io::Result<()> {
        let mut buf = vec![0u8; 1 << 20];
        loop {
            if self.cancelled() {
                return Err(std::io::Error::other(l!("abgebrochen", "cancelled")));
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
            self.add_bytes(n as u64);
            self.ctx.request_repaint();
        }
        Ok(())
    }

    fn move_item(&mut self, src: &Path, target: &Path) {
        self.set_current(src);
        if fs::symlink_metadata(target).is_err() {
            // Fast path: same filesystem.
            if fs::rename(src, target).is_ok() {
                let (files, bytes) = count(&[target.to_path_buf()]);
                let mut p = self.progress.lock().unwrap();
                p.files_done += files;
                p.bytes_done += bytes;
                return;
            }
        } else if src.is_file()
            && let Target::Write(t) = self.resolve_target(src, target)
            && fs::rename(src, &t).is_ok()
        {
            self.file_done();
            return;
        }
        // Cross-device or merging into an existing dir: copy, then delete what was copied.
        let errors_before = self.progress.lock().unwrap().errors.len();
        self.copy_item(src, target);
        let failed = self.progress.lock().unwrap().errors.len() > errors_before;
        if !failed && !self.cancelled() && self.policy != Some(OverwriteAnswer::SkipAll) {
            if let Err(e) = remove_any(src) {
                self.err(format!("{}: {e}", src.to_string_lossy()));
            }
        }
    }

    /// Unpack complete archives (Alt+F9, l!("Hier entpacken", "Unpack here"), "Smart entpacken").
    fn unpack(&mut self, archives: &[PathBuf], dest: &Path, smart: bool) {
        // Read all indexes first for the totals.
        let mut indexes = Vec::new();
        for a in archives {
            match archive::index(a) {
                Ok(i) => indexes.push((a.clone(), i)),
                Err(e) => self.err(e),
            }
        }
        let files = indexes.iter().map(|(_, i)| i.iter().filter(|it| !it.is_dir).count() as u64).sum();
        let bytes = indexes.iter().map(|(_, i)| i.iter().map(|it| it.size).sum::<u64>()).sum();
        self.set_totals(files, bytes);
        for (archive_path, index) in indexes {
            if self.cancelled() {
                break;
            }
            let mut target = dest.to_path_buf();
            if smart && archive::needs_own_folder(&index) {
                // Loose files go into a folder named like the archive ("name (2)" if taken).
                let name = archive::stem(&archive_path);
                target = if dest.join(&name).exists() { crate::fsutil::unique_name(dest, &name) } else { dest.join(&name) };
                if let Err(e) = fs::create_dir_all(&target) {
                    self.err(format!("{}: {e}", target.display()));
                    continue;
                }
            }
            self.extract_members(&archive_path, "", &[String::new()], &target);
        }
    }

    fn extract(&mut self, archive_path: &Path, base: &str, items: &[PathBuf], dest: &Path) {
        let items: Vec<String> = items.iter().map(|p| p.to_string_lossy().trim_matches('/').to_string()).collect();
        let index = match archive::index(archive_path) {
            Ok(i) => i,
            Err(e) => return self.err(e),
        };
        let wanted_items: Vec<&archive::Item> =
            index.iter().filter(|it| archive::selected(&it.path, &items)).collect();
        self.set_totals(wanted_items.len() as u64, wanted_items.iter().map(|it| it.size).sum());
        self.extract_members(archive_path, base, &items, dest);
    }

    /// Extract the members selected by `items` ("" = all), stripping `base`.
    fn extract_members(&mut self, archive_path: &Path, base: &str, items: &[String], dest: &Path) {
        let base = base.trim_matches('/').to_string();
        let wanted = |p: &str| archive::selected(p, &items);
        let mut sink = ExtractSink { worker: self, dest, base: &base, archive: archive_path };
        if let Err(e) = archive::extract(archive_path, &wanted, &mut sink) {
            self.err(format!("{}: {e}", archive_path.display()));
        }
    }

    fn sync(&mut self, ops: &[crate::sync::Op], to_trash: bool) {
        use crate::sync::Op;
        let bytes = ops
            .iter()
            .filter_map(|o| match o {
                Op::Copy { from, is_dir: false, .. } => fs::metadata(from).ok().map(|m| m.len()),
                _ => None,
            })
            .sum();
        self.set_totals(ops.len() as u64, bytes);
        // The user already decided per file: no overwrite questions.
        self.policy = Some(OverwriteAnswer::OverwriteAll);
        for op in ops {
            if self.cancelled() {
                break;
            }
            match op {
                Op::Copy { from: _, to, is_dir: true } => {
                    if let Err(e) = fs::create_dir_all(to) {
                        self.err(format!("{}: {e}", to.display()));
                    }
                    self.file_done();
                }
                Op::Copy { from, to, is_dir: false } => {
                    if let Some(parent) = to.parent()
                        && let Err(e) = fs::create_dir_all(parent)
                    {
                        self.err(format!("{}: {e}", parent.display()));
                        continue;
                    }
                    self.copy_item(from, to); // counts the file itself
                }
                Op::Delete(p) => {
                    self.set_current(p);
                    let r = if to_trash { trash::delete(p).map_err(|e| e.to_string()) } else { remove_any(p).map_err(|e| e.to_string()) };
                    if let Err(e) = r {
                        self.err(format!("{}: {e}", p.display()));
                    }
                    self.file_done();
                }
            }
        }
    }

    /// Packs `sources` into `dest`; the format follows the file extension.
    fn pack(&mut self, sources: &[PathBuf], dest: &Path) {
        // Collect (path, name inside archive, is_dir), relative to each source's parent.
        let mut list: archive::PackList = Vec::new();
        for src in sources {
            let base = src.parent().unwrap_or(Path::new("/")).to_path_buf();
            let mut stack = vec![src.clone()];
            while let Some(p) = stack.pop() {
                let rel = p.strip_prefix(&base).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                let Ok(meta) = fs::symlink_metadata(&p) else { continue };
                if meta.is_dir() {
                    list.push((p.clone(), rel, true));
                    if let Ok(rd) = fs::read_dir(&p) {
                        let mut children: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                        children.sort();
                        stack.extend(children.into_iter().rev());
                    }
                } else if meta.is_file() {
                    list.push((p, rel, false));
                }
            }
        }
        let files = list.iter().filter(|(_, _, d)| !d).count() as u64;
        let bytes = list.iter().filter(|(_, _, d)| !d).filter_map(|(p, _, _)| fs::metadata(p).ok()).map(|m| m.len()).sum();
        self.set_totals(files, bytes);
        let Target::Write(dest) = self.resolve_target(Path::new(l!("Neues Archiv", "New archive")), dest) else {
            return;
        };
        let worker: &Worker = self;
        let mut open = |p: &Path| -> std::io::Result<Box<dyn Read + '_>> {
            worker.set_current(p);
            Ok(Box::new(ProgressReader { inner: File::open(p)?, worker }))
        };
        let r = archive::pack(&dest, &list, &mut open);
        if let Err(e) = r {
            self.err(e);
            let _ = fs::remove_file(&dest);
        } else if self.cancelled() {
            let _ = fs::remove_file(&dest);
        } else {
            self.progress.lock().unwrap().files_done = files;
        }
    }

    /// Copy (or move) local files and folders into an existing archive.
    fn archive_add(&mut self, archive_path: &Path, inner: &str, sources: &[PathBuf], delete_source: bool) {
        // What is in the archive already: path → is it a folder?
        let existing: HashMap<String, (bool, Option<SystemTime>)> = match archive::index(archive_path) {
            Ok(items) => items.iter().map(|i| (i.path.clone(), (i.is_dir, i.modified))).collect(),
            Err(e) => return self.err(e),
        };
        // A folder also exists when only files below it are stored ("a/b.txt" → "a").
        let is_folder = |path: &str| {
            existing.get(path).is_some_and(|(d, _)| *d) || existing.keys().any(|k| k.starts_with(&format!("{path}/")))
        };
        // Collect (path, name inside the archive, is_dir) like for packing.
        let mut list: archive::PackList = Vec::new();
        for src in sources {
            let base = src.parent().unwrap_or(Path::new("/")).to_path_buf();
            let mut stack = vec![src.clone()];
            while let Some(p) = stack.pop() {
                let rel = p.strip_prefix(&base).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                let name = format!("{inner}{rel}");
                let Ok(meta) = fs::symlink_metadata(&p) else {
                    self.err(lf!("{}: nicht lesbar", "{}: not readable", p.to_string_lossy()));
                    continue;
                };
                if meta.is_dir() {
                    if existing.get(&name).is_some_and(|(d, _)| !*d) {
                        self.err(lf!("{name}: im Archiv gibt es schon eine Datei mit diesem Namen", "{name}: the archive already has a file with this name"));
                        continue; // and nothing below it
                    }
                    // An existing folder is merged: no second folder entry.
                    if !existing.contains_key(&name) {
                        list.push((p.clone(), name, true));
                    }
                    if let Ok(rd) = fs::read_dir(&p) {
                        let mut children: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                        children.sort();
                        stack.extend(children.into_iter().rev());
                    }
                } else if meta.is_file() {
                    if is_folder(&name) {
                        self.err(lf!("{name}: im Archiv gibt es schon einen Ordner mit diesem Namen", "{name}: the archive already has a folder with this name"));
                        continue;
                    }
                    list.push((p, name, false));
                }
            }
        }
        // Files that exist in the archive: ask (overwrite / skip / rename).
        let mut taken: std::collections::HashSet<String> = existing.keys().cloned().collect();
        taken.extend(list.iter().map(|(_, n, _)| n.clone()));
        let mut kept: archive::PackList = Vec::new();
        for (path, name, is_dir) in list {
            if self.cancelled() {
                return;
            }
            let Some((false, old_mtime)) = existing.get(&name).copied().filter(|_| !is_dir) else {
                kept.push((path, name, is_dir));
                continue;
            };
            let new_mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
            let newer = matches!((new_mtime, old_mtime), (Some(a), Some(b)) if a > b);
            let label = PathBuf::from(format!("{}/{name}", archive_path.display()));
            match self.decide(&path, &label, newer) {
                Decision::Skip => {}
                Decision::Write => kept.push((path, name, false)), // replaces the old member
                Decision::Rename => {
                    let (dir, file) = match name.rfind('/') {
                        Some(i) => (&name[..=i], &name[i + 1..]),
                        None => ("", name.as_str()),
                    };
                    let (stem, ext) = crate::fsutil::split_ext(file, false);
                    let free = (2..)
                        .map(|i| if ext.is_empty() { format!("{dir}{stem} ({i})") } else { format!("{dir}{stem} ({i}).{ext}") })
                        .find(|n| !taken.contains(n))
                        .unwrap();
                    taken.insert(free.clone());
                    kept.push((path, free, false));
                }
            }
        }
        let list = kept;
        let files = list.iter().filter(|(_, _, d)| !d).count() as u64;
        let bytes = list.iter().filter(|(_, _, d)| !d).filter_map(|(p, _, _)| fs::metadata(p).ok()).map(|m| m.len()).sum();
        self.set_totals(files, bytes);
        if list.is_empty() {
            return; // everything skipped: the archive is not touched at all
        }
        let worker: &Worker = self;
        let mut open = |p: &Path| -> std::io::Result<Box<dyn Read + '_>> {
            worker.set_current(p);
            Ok(Box::new(ProgressReader { inner: File::open(p)?, worker }))
        };
        let r = archive::rewrite(archive_path, &[], &list, &mut open, &|| worker.cancelled());
        match r {
            Ok(true) => {
                self.progress.lock().unwrap().files_done = files;
                if delete_source {
                    // Only what really went into the archive; folders only when they ended up empty.
                    let mut dirs: Vec<&PathBuf> = Vec::new();
                    for (p, _, is_dir) in &list {
                        if *is_dir {
                            dirs.push(p);
                        } else if let Err(e) = fs::remove_file(p) {
                            self.err(format!("{}: {e}", p.to_string_lossy()));
                        }
                    }
                    let mut all_dirs: Vec<PathBuf> = sources.iter().filter(|s| s.is_dir()).cloned().collect();
                    all_dirs.extend(dirs.into_iter().cloned());
                    all_dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
                    all_dirs.dedup();
                    for d in all_dirs {
                        let _ = fs::remove_dir(&d);
                    }
                }
            }
            Ok(false) => {}
            // (a cancel in the middle of a file comes back as a read error)
            Err(_) if self.cancelled() => {}
            Err(e) => self.err(e),
        }
    }

    /// Delete members (files, or folders with everything in them) from an archive.
    fn archive_delete(&mut self, archive_path: &Path, sources: &[PathBuf]) {
        let remove: Vec<String> = sources
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/").trim_matches('/').to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if remove.is_empty() {
            return;
        }
        self.set_totals(remove.len() as u64, 0);
        self.set_current(archive_path);
        let worker: &Worker = self;
        let mut open = |p: &Path| -> std::io::Result<Box<dyn Read + '_>> { Ok(Box::new(File::open(p)?)) };
        match archive::rewrite(archive_path, &remove, &Vec::new(), &mut open, &|| worker.cancelled()) {
            Ok(true) => self.progress.lock().unwrap().files_done = remove.len() as u64,
            Ok(false) => {}
            Err(e) => self.err(e),
        }
    }

    // ------------------------------------------------------------------------
    // FTP
    // ------------------------------------------------------------------------

    fn set_totals(&self, files: u64, bytes: u64) {
        let mut p = self.progress.lock().unwrap();
        p.files_total = files;
        p.bytes_total = bytes;
    }

    fn upload(&mut self, conn: &dyn Remote, sources: &[PathBuf], dest: &str, delete_source: bool) {
        // Plan: remote dirs to create and (local, remote) file pairs.
        let mut dirs: Vec<(PathBuf, String)> = Vec::new();
        let mut files: Vec<(PathBuf, String, u64)> = Vec::new();
        for src in sources {
            let base = src.parent().unwrap_or(Path::new("/")).to_path_buf();
            let mut stack = vec![src.clone()];
            while let Some(p) = stack.pop() {
                let rel = p.strip_prefix(&base).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                let remote = remote::join(dest, &rel);
                let Ok(meta) = fs::metadata(&p) else {
                    self.err(lf!("{}: nicht lesbar", "{}: not readable", p.to_string_lossy()));
                    continue;
                };
                if meta.is_dir() {
                    dirs.push((p.clone(), remote));
                    if let Ok(rd) = fs::read_dir(&p) {
                        stack.extend(rd.flatten().map(|e| e.path()));
                    }
                } else {
                    files.push((p.clone(), remote, meta.len()));
                }
            }
        }
        self.set_totals(files.len() as u64, files.iter().map(|f| f.2).sum());

        if let Err(e) = conn.mkdir_all(dest) {
            return self.err(e);
        }
        for (_, d) in &dirs {
            if !conn.is_dir(d)
                && let Err(e) = conn.mkdir(d)
            {
                self.err(format!("{d}: {e}"));
            }
        }

        // Remote listings per directory, for overwrite checks.
        let mut listings: HashMap<String, HashMap<String, Option<SystemTime>>> = HashMap::new();
        for (local, remote, _) in files {
            if self.cancelled() {
                break;
            }
            let rdir = remote::parent(&remote).unwrap_or_else(|| "/".into());
            let names = listings.entry(rdir.clone()).or_insert_with(|| {
                conn.list(&rdir)
                    .map(|v| v.into_iter().map(|e| (e.name, e.modified)).collect())
                    .unwrap_or_default()
            });
            let mut target = remote.clone();
            let name = remote::file_name(&remote).to_string();
            if let Some(remote_mtime) = names.get(&name).copied() {
                let local_mtime = fs::metadata(&local).and_then(|m| m.modified()).ok();
                let newer = matches!((local_mtime, remote_mtime), (Some(a), Some(b)) if a > b);
                let label = PathBuf::from(format!("{}{}", conn.url(), remote));
                match self.decide(&local, &label, newer) {
                    Decision::Skip => {
                        self.file_done();
                        continue;
                    }
                    Decision::Write => {}
                    Decision::Rename => {
                        let names = listings.get(&rdir).unwrap();
                        let (stem, ext) = crate::fsutil::split_ext(&name, false);
                        target = (2..)
                            .map(|i| if ext.is_empty() { format!("{stem} ({i})") } else { format!("{stem} ({i}).{ext}") })
                            .find(|n| !names.contains_key(n))
                            .map(|n| remote::join(&rdir, &n))
                            .unwrap();
                    }
                }
            }
            self.set_current(&local);
            let r = conn.upload(&local, &target, &|r, w| self.pump(r, w));
            match r {
                Ok(()) => {
                    if let Some(n) = listings.get_mut(&rdir) {
                        n.insert(remote::file_name(&target).to_string(), None);
                    }
                    if delete_source && let Err(e) = fs::remove_file(&local) {
                        self.err(format!("{}: {e}", local.to_string_lossy()));
                    }
                }
                Err(e) => self.err(format!("{}: {e}", local.to_string_lossy())),
            }
            self.file_done();
        }
        if delete_source && !self.cancelled() {
            // Deepest first; only removes dirs that ended up empty.
            dirs.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
            for (p, _) in dirs {
                let _ = fs::remove_dir(&p);
            }
        }
    }

    fn download(&mut self, conn: &dyn Remote, sources: &[PathBuf], dirs: &[PathBuf], dest: &Path, delete_source: bool) {
        // Plan: (remote, local, size, mtime) for files, local dirs to create, remote dirs for cleanup.
        let mut files: Vec<(String, PathBuf, u64, Option<SystemTime>)> = Vec::new();
        let mut local_dirs: Vec<PathBuf> = Vec::new();
        let mut remote_dirs: Vec<String> = Vec::new();
        let mut parent_listing: HashMap<String, Vec<crate::fsutil::Entry>> = HashMap::new();
        for src in sources {
            let remote = src.to_string_lossy().into_owned();
            let name = remote::file_name(&remote).to_string();
            let local = dest.join(&name);
            if dirs.contains(src) {
                local_dirs.push(local.clone());
                remote_dirs.push(remote.clone());
                let mut all = Vec::new();
                if let Err(e) = conn.walk(&remote, &mut all) {
                    self.err(format!("{remote}: {e}"));
                    continue;
                }
                for e in all {
                    let rp = e.path.to_string_lossy().into_owned();
                    let rel = rp.strip_prefix(&remote).unwrap_or(&rp).trim_start_matches('/');
                    let lp = local.join(rel);
                    if e.is_dir {
                        local_dirs.push(lp);
                        remote_dirs.push(rp);
                    } else {
                        files.push((rp, lp, e.size, e.modified));
                    }
                }
            } else {
                let rdir = remote::parent(&remote).unwrap_or_else(|| "/".into());
                let listing = parent_listing
                    .entry(rdir.clone())
                    .or_insert_with(|| conn.list(&rdir).unwrap_or_default());
                let info = listing.iter().find(|e| e.name == name);
                files.push((remote, local, info.map(|e| e.size).unwrap_or(0), info.and_then(|e| e.modified)));
            }
        }
        self.set_totals(files.len() as u64, files.iter().map(|f| f.2).sum());
        for d in &local_dirs {
            if let Err(e) = fs::create_dir_all(d) {
                self.err(format!("{}: {e}", d.to_string_lossy()));
            }
        }
        for (remote, local, _, mtime) in files {
            if self.cancelled() {
                break;
            }
            let label = PathBuf::from(format!("{}{}", conn.url(), remote));
            let Target::Write(t) = self.resolve_local(&label, &local, mtime) else {
                self.file_done();
                continue;
            };
            self.set_current(&label);
            let r = conn.download(&remote, &t, &|r, w| self.pump(r, w));
            match r {
                Ok(()) => {
                    if let Some(m) = mtime {
                        let _ = File::options().write(true).open(&t).and_then(|f| f.set_modified(m));
                    }
                    if delete_source && let Err(e) = conn.remove_file(&remote) {
                        self.err(format!("{remote}: {e}"));
                    }
                }
                Err(e) => {
                    self.err(format!("{remote}: {e}"));
                    let _ = fs::remove_file(&t);
                }
            }
            self.file_done();
        }
        if delete_source && !self.cancelled() {
            remote_dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
            for d in remote_dirs {
                let _ = conn.remove_dir(&d);
            }
        }
    }

    /// Server to server: every file is fetched into a temporary local file
    /// and sent on from there (FTP and SFTP can't talk to each other directly).
    /// Moving within one server is a plain rename.
    fn remote_copy(&mut self, from: &dyn Remote, to: &dyn Remote, sources: &[PathBuf], dirs: &[PathBuf], dest: &str, delete_source: bool) {
        let same_server = from.id() == to.id();
        if let Err(e) = to.mkdir_all(dest) {
            return self.err(e);
        }
        // Names in a target folder, for overwrite checks (listed once per folder).
        type Names = HashMap<String, (bool, Option<SystemTime>)>;
        fn names_in(cache: &mut HashMap<String, Names>, conn: &dyn Remote, dir: &str) -> Names {
            cache
                .entry(dir.to_string())
                .or_insert_with(|| conn.list(dir).map(|v| v.into_iter().map(|e| (e.name, (e.is_dir, e.modified))).collect()).unwrap_or_default())
                .clone()
        }
        let mut listings: HashMap<String, Names> = HashMap::new();
        let free_name = |names: &Names, name: &str| -> String {
            let (stem, ext) = crate::fsutil::split_ext(name, false);
            (2..)
                .map(|i| if ext.is_empty() { format!("{stem} ({i})") } else { format!("{stem} ({i}).{ext}") })
                .find(|n| !names.contains_key(n))
                .unwrap()
        };

        if same_server && delete_source {
            self.set_totals(sources.len() as u64, 0);
            for src in sources {
                if self.cancelled() {
                    return;
                }
                let remote = src.to_string_lossy().into_owned();
                let name = remote::file_name(&remote).to_string();
                let mut target = remote::join(dest, &name);
                self.set_current(Path::new(&remote));
                if target == remote || target.starts_with(&format!("{remote}/")) {
                    self.err(lf!("{remote}: Ziel liegt in der Quelle", "{remote}: the target is inside the source"));
                    self.file_done();
                    continue;
                }
                let names = names_in(&mut listings, to, dest);
                if let Some((is_dir, _)) = names.get(&name) {
                    let label = PathBuf::from(format!("{}{}", to.url(), target));
                    match self.decide(Path::new(&remote), &label, false) {
                        Decision::Skip => {
                            self.file_done();
                            continue;
                        }
                        Decision::Rename => target = remote::join(dest, &free_name(&names, &name)),
                        Decision::Write if *is_dir => {
                            self.err(lf!("{target}: dort liegt schon ein Ordner – er wird nicht überschrieben", "{target}: there is a folder already – it is not overwritten"));
                            self.file_done();
                            continue;
                        }
                        Decision::Write => {
                            if let Err(e) = to.remove_file(&target) {
                                self.err(format!("{target}: {e}"));
                                self.file_done();
                                continue;
                            }
                        }
                    }
                }
                if let Err(e) = to.rename(&remote, &target) {
                    self.err(format!("{remote}: {e}"));
                }
                self.file_done();
            }
            return;
        }

        // Plan: (source file, target file, size, mtime), target dirs to create, source dirs for cleanup.
        let mut files: Vec<(String, String, u64, Option<SystemTime>)> = Vec::new();
        let mut target_dirs: Vec<String> = Vec::new();
        let mut source_dirs: Vec<String> = Vec::new();
        let mut parent_listing: HashMap<String, Vec<crate::fsutil::Entry>> = HashMap::new();
        for src in sources {
            let remote = src.to_string_lossy().into_owned();
            let name = remote::file_name(&remote).to_string();
            let target = remote::join(dest, &name);
            if dirs.contains(src) {
                if same_server && (target == remote || target.starts_with(&format!("{remote}/"))) {
                    self.err(lf!("{remote}: Ziel liegt in der Quelle", "{remote}: the target is inside the source"));
                    continue;
                }
                target_dirs.push(target.clone());
                source_dirs.push(remote.clone());
                let mut all = Vec::new();
                if let Err(e) = from.walk(&remote, &mut all) {
                    self.err(format!("{remote}: {e}"));
                    continue;
                }
                for e in all {
                    let rp = e.path.to_string_lossy().into_owned();
                    let rel = rp.strip_prefix(&remote).unwrap_or(&rp).trim_start_matches('/');
                    let tp = remote::join(&target, rel);
                    if e.is_dir {
                        target_dirs.push(tp);
                        source_dirs.push(rp);
                    } else {
                        files.push((rp, tp, e.size, e.modified));
                    }
                }
            } else {
                let rdir = remote::parent(&remote).unwrap_or_else(|| "/".into());
                let listing = parent_listing.entry(rdir.clone()).or_insert_with(|| from.list(&rdir).unwrap_or_default());
                let info = listing.iter().find(|e| e.name == name);
                files.push((remote, target, info.map(|e| e.size).unwrap_or(0), info.and_then(|e| e.modified)));
            }
        }
        // Every byte travels twice: down to this computer and up again.
        self.set_totals(files.len() as u64, files.iter().map(|f| f.2).sum::<u64>() * 2);
        for d in &target_dirs {
            if !to.is_dir(d)
                && let Err(e) = to.mkdir(d)
            {
                self.err(format!("{d}: {e}"));
            }
        }
        let temp_dir = remote::cache_dir(from.id()).join(format!("transfer-{}", to.id()));
        if let Err(e) = fs::create_dir_all(&temp_dir) {
            return self.err(format!("{}: {e}", temp_dir.to_string_lossy()));
        }
        let temp = temp_dir.join("part");
        for (source, mut target, size, mtime) in files {
            if self.cancelled() {
                break;
            }
            let tdir = remote::parent(&target).unwrap_or_else(|| "/".into());
            let name = remote::file_name(&target).to_string();
            let names = names_in(&mut listings, to, &tdir);
            if same_server && source == target {
                // Copy into the same folder: a second file "name (2)".
                target = remote::join(&tdir, &free_name(&names, &name));
            } else if let Some((is_dir, target_mtime)) = names.get(&name) {
                let label = PathBuf::from(format!("{}{}", to.url(), target));
                let newer = matches!((mtime, *target_mtime), (Some(a), Some(b)) if a > b);
                match self.decide(Path::new(&format!("{}{}", from.url(), source)), &label, newer) {
                    Decision::Skip => {
                        self.add_bytes(size * 2);
                        self.file_done();
                        continue;
                    }
                    Decision::Rename => target = remote::join(&tdir, &free_name(&names, &name)),
                    Decision::Write if *is_dir => {
                        self.err(lf!("{target}: dort liegt schon ein Ordner – er wird nicht überschrieben", "{target}: there is a folder already – it is not overwritten"));
                        self.file_done();
                        continue;
                    }
                    Decision::Write => {}
                }
            }
            self.set_current(Path::new(&format!("{}{}", from.url(), source)));
            let r = from
                .download(&source, &temp, &|r, w| self.pump(r, w))
                .map_err(|e| format!("{}{source}: {e}", from.url()))
                .and_then(|_| to.upload(&temp, &target, &|r, w| self.pump(r, w)).map_err(|e| format!("{}{target}: {e}", to.url())));
            let _ = fs::remove_file(&temp);
            match r {
                Ok(()) => {
                    if let Some(n) = listings.get_mut(&tdir) {
                        n.insert(remote::file_name(&target).to_string(), (false, None));
                    }
                    if delete_source && let Err(e) = from.remove_file(&source) {
                        self.err(format!("{source}: {e}"));
                    }
                }
                Err(_) if self.cancelled() => {
                    // A half uploaded file must not stay behind.
                    let _ = to.remove_file(&target);
                }
                Err(e) => self.err(e),
            }
            self.file_done();
        }
        let _ = fs::remove_dir_all(&temp_dir);
        if delete_source && !self.cancelled() {
            // Deepest first; only removes dirs that ended up empty.
            source_dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
            for d in source_dirs {
                let _ = from.remove_dir(&d);
            }
        }
    }

    fn ftp_delete(&mut self, conn: &dyn Remote, sources: &[PathBuf], dirs: &[PathBuf]) {
        let mut files: Vec<String> = Vec::new();
        let mut rdirs: Vec<String> = Vec::new();
        for src in sources {
            let remote = src.to_string_lossy().into_owned();
            if dirs.contains(src) {
                let mut all = Vec::new();
                if let Err(e) = conn.walk(&remote, &mut all) {
                    self.err(format!("{remote}: {e}"));
                    continue;
                }
                for e in all {
                    let p = e.path.to_string_lossy().into_owned();
                    if e.is_dir { rdirs.push(p) } else { files.push(p) }
                }
                rdirs.push(remote);
            } else {
                files.push(remote);
            }
        }
        self.set_totals((files.len() + rdirs.len()) as u64, 0);
        for f in files {
            if self.cancelled() {
                return;
            }
            self.set_current(Path::new(&f));
            if let Err(e) = conn.remove_file(&f) {
                self.err(format!("{f}: {e}"));
            }
            self.file_done();
        }
        rdirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
        for d in rdirs {
            if self.cancelled() {
                return;
            }
            self.set_current(Path::new(&d));
            if let Err(e) = conn.remove_dir(&d) {
                self.err(format!("{d}: {e}"));
            }
            self.file_done();
        }
    }
}

pub fn remove_any(p: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(p)?;
    if meta.is_dir() {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    }
}

/// (number of files, total bytes) below the given paths.
pub fn count(paths: &[PathBuf]) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(p) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if m.is_dir() {
            if let Ok(rd) = fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        } else {
            files += 1;
            bytes += m.len();
        }
    }
    (files, bytes)
}

/// Counts bytes read while packing and stops on cancel.
struct ProgressReader<'a> {
    inner: File,
    worker: &'a Worker,
}

impl Read for ProgressReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.worker.cancelled() {
            return Err(std::io::Error::other(l!("abgebrochen", "cancelled")));
        }
        let n = self.inner.read(buf)?;
        if n == 0 {
            self.worker.file_done();
        }
        self.worker.add_bytes(n as u64);
        self.worker.ctx.request_repaint();
        Ok(n)
    }
}

/// Connects archive extraction with the worker (targets, overwrite questions, progress).
struct ExtractSink<'a> {
    worker: &'a mut Worker,
    dest: &'a Path,
    /// The archive folder the user is in; stripped from member paths.
    base: &'a str,
    archive: &'a Path,
}

impl archive::Sink for ExtractSink<'_> {
    fn target(&mut self, item: &archive::Item) -> Option<PathBuf> {
        let rel = if self.base.is_empty() {
            item.path.as_str()
        } else {
            item.path.strip_prefix(self.base).map(|r| r.trim_start_matches('/')).unwrap_or(&item.path)
        };
        let target = self.dest.join(rel);
        self.worker.set_current(&target);
        if item.is_dir {
            return Some(target);
        }
        let label = self.archive.join(&item.path);
        match self.worker.resolve_local(&label, &target, item.modified) {
            Target::Write(t) => Some(t),
            Target::Skip => {
                self.worker.file_done();
                None
            }
        }
    }

    fn write(&mut self, data: &mut dyn Read, out: &mut dyn Write) -> std::io::Result<()> {
        self.worker.pump(data, out)
    }

    fn done(&mut self, item: &archive::Item, result: Result<(), String>) {
        if let Err(e) = result {
            self.worker.err(format!("{}: {e}", item.path));
        }
        if !item.is_dir {
            self.worker.file_done();
        }
    }

    fn cancelled(&self) -> bool {
        self.worker.cancelled()
    }
}

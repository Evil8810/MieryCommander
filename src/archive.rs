//! Archives as folders: ZIP, TAR (+gz/bz2/xz/zst), 7z, RAR (read-only) and
//! single compressed files (.gz/.bz2/.xz/.zst).
//!
//! Every format is turned into one flat index of `Item`s (cached), from which
//! the panel lists a directory level; extraction streams the wanted members.

use crate::fsutil::Entry;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Comp {
    None,
    Gz,
    Bz2,
    Xz,
    Zst,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Zip,
    Tar(Comp),
    SevenZ,
    /// Read-only (creating RAR files is reserved to RARLab's tools).
    Rar,
    /// A single compressed file, e.g. "notes.txt.gz".
    Single(Comp),
}

pub fn kind_of(path: &Path) -> Option<Kind> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let ends = |exts: &[&str]| exts.iter().any(|e| name.ends_with(e));
    Some(if ends(&[".tar.gz", ".tgz"]) {
        Kind::Tar(Comp::Gz)
    } else if ends(&[".tar.bz2", ".tbz2", ".tbz"]) {
        Kind::Tar(Comp::Bz2)
    } else if ends(&[".tar.xz", ".txz"]) {
        Kind::Tar(Comp::Xz)
    } else if ends(&[".tar.zst", ".tzst"]) {
        Kind::Tar(Comp::Zst)
    } else if ends(&[".tar"]) {
        Kind::Tar(Comp::None)
    } else if ends(&[".zip", ".jar", ".apk", ".cbz", ".epub", ".odt", ".ods", ".odp", ".docx", ".xlsx", ".pptx"]) {
        Kind::Zip
    } else if ends(&[".7z", ".cb7"]) {
        Kind::SevenZ
    } else if ends(&[".rar", ".cbr"]) {
        Kind::Rar
    } else if ends(&[".gz"]) {
        Kind::Single(Comp::Gz)
    } else if ends(&[".bz2"]) {
        Kind::Single(Comp::Bz2)
    } else if ends(&[".xz"]) {
        Kind::Single(Comp::Xz)
    } else if ends(&[".zst"]) {
        Kind::Single(Comp::Zst)
    } else {
        return None;
    })
}

/// Archive name without its archive extension: "urlaub.tar.gz" → "urlaub".
pub fn stem(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let lower = name.to_lowercase();
    const EXTS: &[&str] = &[
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tgz", ".tbz2", ".tbz", ".txz", ".tzst", ".tar", ".zip", ".jar", ".apk",
        ".cbz", ".epub", ".odt", ".ods", ".odp", ".docx", ".xlsx", ".pptx", ".7z", ".cb7", ".rar", ".cbr", ".gz", ".bz2", ".xz",
        ".zst",
    ];
    match EXTS.iter().find(|e| lower.ends_with(*e)) {
        Some(e) if name.len() > e.len() => name[..name.len() - e.len()].to_string(),
        _ => name,
    }
}

/// Smart extraction: does the archive need its own folder? Yes, unless
/// everything sits in one top-level folder (or it is a single file).
pub fn needs_own_folder(items: &[Item]) -> bool {
    let mut tops = std::collections::HashSet::new();
    for it in items {
        tops.insert(it.path.split('/').next().unwrap_or(""));
        if tops.len() > 1 {
            return true;
        }
    }
    false
}

pub fn is_archive(path: &Path) -> bool {
    kind_of(path).is_some()
}

/// Formats Alt+F5 can create (by target file extension).
pub fn can_create(path: &Path) -> bool {
    matches!(kind_of(path), Some(Kind::Zip | Kind::Tar(_) | Kind::SevenZ))
}

/// One member of an archive. `path` uses '/', no leading slash, dirs without trailing slash.
#[derive(Clone, Debug)]
pub struct Item {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub mode: Option<u32>,
}

/// Normalize a member path; `None` for unsafe paths ("../", absolute) that
/// could escape the extraction folder.
fn clean_path(raw: &str) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

pub fn decompress(file: File, comp: Comp) -> io::Result<Box<dyn Read>> {
    let r = io::BufReader::new(file);
    Ok(match comp {
        Comp::None => Box::new(r),
        Comp::Gz => Box::new(flate2::read::MultiGzDecoder::new(r)),
        Comp::Bz2 => Box::new(bzip2::read::MultiBzDecoder::new(r)),
        Comp::Xz => Box::new(lzma_rust2::XzReader::new(r, true)),
        Comp::Zst => Box::new(zstd::Decoder::with_buffer(r)?),
    })
}

fn zip_time(dt: Option<zip::DateTime>) -> Option<SystemTime> {
    let dt = dt?;
    let naive = chrono::NaiveDate::from_ymd_opt(dt.year() as i32, dt.month() as u32, dt.day() as u32)?
        .and_hms_opt(dt.hour() as u32, dt.minute() as u32, dt.second() as u32)?;
    let local = naive.and_local_timezone(chrono::Local).single()?;
    Some(local.into())
}

/// MS-DOS date/time as used by RAR.
fn dos_time(t: u32) -> Option<SystemTime> {
    let (date, time) = ((t >> 16) as u16, t as u16);
    let naive = chrono::NaiveDate::from_ymd_opt(1980 + (date >> 9) as i32, ((date >> 5) & 15) as u32, (date & 31) as u32)?
        .and_hms_opt((time >> 11) as u32, ((time >> 5) & 63) as u32, ((time & 31) * 2) as u32)?;
    Some(naive.and_local_timezone(chrono::Local).single()?.into())
}

/// Name of the content of a single compressed file: "a.txt.gz" → "a.txt".
fn single_name(archive: &Path) -> String {
    let name = archive.file_name().unwrap_or_default().to_string_lossy().into_owned();
    match name.rfind('.') {
        Some(i) if i > 0 => name[..i].to_string(),
        _ => format!("{name}.out"),
    }
}

fn read_index(archive: &Path, kind: Kind) -> Result<Vec<Item>, String> {
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", archive.display());
    let mut items = Vec::new();
    match kind {
        Kind::Zip => {
            let mut zip = zip::ZipArchive::new(File::open(archive).map_err(|e| err(&e))?).map_err(|e| err(&e))?;
            for i in 0..zip.len() {
                let Ok(f) = zip.by_index_raw(i) else { continue };
                let Some(path) = clean_path(f.name()) else { continue };
                items.push(Item {
                    path,
                    is_dir: f.is_dir(),
                    size: f.size(),
                    modified: zip_time(f.last_modified()),
                    mode: f.unix_mode(),
                });
            }
        }
        Kind::Tar(comp) => {
            let mut tar = tar::Archive::new(decompress(File::open(archive).map_err(|e| err(&e))?, comp).map_err(|e| err(&e))?);
            for entry in tar.entries().map_err(|e| err(&e))? {
                let entry = entry.map_err(|e| err(&e))?;
                let h = entry.header();
                let Ok(raw) = entry.path() else { continue };
                let Some(path) = clean_path(&raw.to_string_lossy()) else { continue };
                let t = h.entry_type();
                if !(t.is_file() || t.is_dir() || t.is_symlink() || t.is_hard_link()) {
                    continue;
                }
                items.push(Item {
                    path,
                    is_dir: t.is_dir(),
                    size: h.size().unwrap_or(0),
                    modified: h.mtime().ok().map(|s| UNIX_EPOCH + Duration::from_secs(s)),
                    mode: h.mode().ok(),
                });
            }
        }
        Kind::SevenZ => {
            let reader = sevenz_rust2::ArchiveReader::new(File::open(archive).map_err(|e| err(&e))?, sevenz_rust2::Password::empty())
                .map_err(|e| err(&e))?;
            for f in &reader.archive().files {
                let Some(path) = clean_path(&f.name) else { continue };
                items.push(Item {
                    path,
                    is_dir: f.is_directory,
                    size: f.size,
                    modified: f.has_last_modified_date.then(|| f.last_modified_date.into()),
                    mode: None,
                });
            }
        }
        Kind::Rar => {
            let list = unrar::Archive::new(archive).open_for_listing().map_err(|e| err(&e))?;
            for h in list {
                let h = h.map_err(|e| err(&e))?;
                let Some(path) = clean_path(&h.filename.to_string_lossy()) else { continue };
                items.push(Item {
                    path,
                    is_dir: h.is_directory(),
                    size: h.unpacked_size,
                    modified: dos_time(h.file_time),
                    mode: None,
                });
            }
        }
        Kind::Single(_) => {
            let meta = std::fs::metadata(archive).map_err(|e| err(&e))?;
            items.push(Item {
                path: single_name(archive),
                is_dir: false,
                size: 0, // only known after decompressing
                modified: meta.modified().ok(),
                mode: None,
            });
        }
    }
    Ok(items)
}

/// Cached index; re-read when the archive file changes.
pub fn index(archive: &Path) -> Result<Arc<Vec<Item>>, String> {
    type Key = (PathBuf, Option<SystemTime>, u64);
    static CACHE: LazyLock<Mutex<HashMap<Key, Arc<Vec<Item>>>>> = LazyLock::new(Default::default);
    let kind = kind_of(archive).ok_or_else(|| "Kein unterstütztes Archiv".to_string())?;
    let meta = std::fs::metadata(archive).map_err(|e| e.to_string())?;
    let key = (archive.to_path_buf(), meta.modified().ok(), meta.len());
    if let Some(v) = CACHE.lock().unwrap().get(&key) {
        return Ok(v.clone());
    }
    let v = Arc::new(read_index(archive, kind)?);
    let mut cache = CACHE.lock().unwrap();
    if cache.len() > 16 {
        cache.clear();
    }
    cache.insert(key, v.clone());
    Ok(v)
}

/// List the direct children of `inner` ("" = root, otherwise "dir/sub/").
/// `Entry::path` holds the path inside the archive (dirs without trailing slash).
pub fn list(archive: &Path, inner: &str) -> Result<Vec<Entry>, String> {
    let items = index(archive)?;
    let mut children: BTreeMap<String, Entry> = BTreeMap::new();
    for it in items.iter() {
        let Some(rest) = it.path.strip_prefix(inner) else { continue };
        if rest.is_empty() {
            continue;
        }
        // "a/b/c" below "" → child "a" (an implicit directory).
        let (child, implicit_dir) = match rest.find('/') {
            Some(pos) => (&rest[..pos], true),
            None => (rest, false),
        };
        let is_dir = implicit_dir || it.is_dir;
        let entry = children.entry(child.to_string()).or_insert_with(|| Entry {
            name: child.to_string(),
            path: PathBuf::from(format!("{inner}{child}")),
            is_dir,
            is_link: false,
            is_parent: false,
            size: 0,
            modified: it.modified,
            mode: if is_dir { 0o755 } else { 0o644 },
        });
        if !implicit_dir {
            entry.is_dir = it.is_dir;
            entry.size = if it.is_dir { 0 } else { it.size };
            entry.modified = it.modified;
            if let Some(m) = it.mode {
                entry.mode = m & 0o7777;
            }
        }
    }
    Ok(children.into_values().collect())
}

/// Does `path` belong to one of the selected `items` ("" = everything)?
pub fn selected(path: &str, items: &[String]) -> bool {
    items.iter().any(|it| it.is_empty() || path == it || path.starts_with(&format!("{it}/")))
}

/// Receives extracted members. Implemented by the job worker (overwrite
/// questions, progress, cancel).
pub trait Sink {
    /// Where to write this member, or `None` to skip it.
    fn target(&mut self, item: &Item) -> Option<PathBuf>;
    fn write(&mut self, data: &mut dyn Read, out: &mut dyn Write) -> io::Result<()>;
    /// Called after each member (file written or failed).
    fn done(&mut self, item: &Item, result: Result<(), String>);
    fn cancelled(&self) -> bool;
}

fn write_member(sink: &mut dyn Sink, item: &Item, data: &mut dyn Read) {
    let Some(target) = sink.target(item) else { return };
    if item.is_dir {
        let r = std::fs::create_dir_all(&target).map_err(|e| e.to_string());
        sink.done(item, r);
        return;
    }
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let r = File::create(&target).and_then(|mut out| sink.write(data, &mut out));
    #[cfg(unix)]
    if r.is_ok()
        && let Some(m) = item.mode
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(m & 0o7777));
    }
    if r.is_ok()
        && let Some(t) = item.modified
    {
        let _ = File::options().write(true).open(&target).and_then(|f| f.set_modified(t));
    }
    if r.is_err() {
        let _ = std::fs::remove_file(&target);
    }
    sink.done(item, r.map_err(|e| e.to_string()));
}

/// Extract all members for which `wanted(path)` is true.
pub fn extract(archive: &Path, wanted: &dyn Fn(&str) -> bool, sink: &mut dyn Sink) -> Result<(), String> {
    let kind = kind_of(archive).ok_or_else(|| "Kein unterstütztes Archiv".to_string())?;
    let open = || File::open(archive).map_err(|e| e.to_string());
    match kind {
        Kind::Zip => {
            let mut zip = zip::ZipArchive::new(open()?).map_err(|e| e.to_string())?;
            for i in 0..zip.len() {
                if sink.cancelled() {
                    break;
                }
                let mut f = match zip.by_index(i) {
                    Ok(f) => f,
                    Err(e) => return Err(e.to_string()),
                };
                let Some(path) = clean_path(f.name()) else { continue };
                if !wanted(&path) {
                    continue;
                }
                let item = Item {
                    path,
                    is_dir: f.is_dir(),
                    size: f.size(),
                    modified: zip_time(f.last_modified()),
                    mode: f.unix_mode(),
                };
                write_member(sink, &item, &mut f);
            }
        }
        Kind::Tar(comp) => {
            let mut tar = tar::Archive::new(decompress(open()?, comp).map_err(|e| e.to_string())?);
            for entry in tar.entries().map_err(|e| e.to_string())? {
                if sink.cancelled() {
                    break;
                }
                let mut entry = entry.map_err(|e| e.to_string())?;
                let Some(path) = entry.path().ok().and_then(|p| clean_path(&p.to_string_lossy())) else { continue };
                let t = entry.header().entry_type();
                if !wanted(&path) || !(t.is_file() || t.is_dir()) {
                    continue;
                }
                let item = Item {
                    path,
                    is_dir: t.is_dir(),
                    size: entry.header().size().unwrap_or(0),
                    modified: entry.header().mtime().ok().map(|s| UNIX_EPOCH + Duration::from_secs(s)),
                    mode: entry.header().mode().ok(),
                };
                write_member(sink, &item, &mut entry);
            }
        }
        Kind::SevenZ => {
            let mut reader = sevenz_rust2::ArchiveReader::new(open()?, sevenz_rust2::Password::empty()).map_err(|e| e.to_string())?;
            reader
                .for_each_entries(|f, data| {
                    if sink.cancelled() {
                        return Ok(false);
                    }
                    if let Some(path) = clean_path(&f.name)
                        && wanted(&path)
                    {
                        let item = Item {
                            path,
                            is_dir: f.is_directory,
                            size: f.size,
                            modified: f.has_last_modified_date.then(|| f.last_modified_date.into()),
                            mode: None,
                        };
                        write_member(sink, &item, data);
                    }
                    Ok(true)
                })
                .map_err(|e| e.to_string())?;
        }
        Kind::Rar => {
            let mut cursor = unrar::Archive::new(archive).open_for_processing().map_err(|e| e.to_string())?;
            while let Some(header) = cursor.read_header().map_err(|e| e.to_string())? {
                let h = header.entry();
                let wanted_item = clean_path(&h.filename.to_string_lossy()).filter(|p| wanted(p)).map(|path| Item {
                    path,
                    is_dir: h.is_directory(),
                    size: h.unpacked_size,
                    modified: dos_time(h.file_time),
                    mode: None,
                });
                cursor = match wanted_item {
                    Some(item) if !sink.cancelled() => match sink.target(&item) {
                        Some(target) if item.is_dir => {
                            let r = std::fs::create_dir_all(&target).map_err(|e| e.to_string());
                            sink.done(&item, r);
                            header.skip().map_err(|e| e.to_string())?
                        }
                        Some(target) => {
                            if let Some(parent) = target.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            // unrar writes the file itself (no byte progress for RAR).
                            match header.extract_to(&target) {
                                Ok(next) => {
                                    if let Some(t) = item.modified {
                                        let _ = File::options().write(true).open(&target).and_then(|f| f.set_modified(t));
                                    }
                                    sink.done(&item, Ok(()));
                                    next
                                }
                                Err(e) => {
                                    sink.done(&item, Err(e.to_string()));
                                    return Err(e.to_string());
                                }
                            }
                        }
                        None => header.skip().map_err(|e| e.to_string())?,
                    },
                    _ => header.skip().map_err(|e| e.to_string())?,
                };
            }
        }
        Kind::Single(comp) => {
            let item = Item {
                path: single_name(archive),
                is_dir: false,
                size: 0,
                modified: std::fs::metadata(archive).and_then(|m| m.modified()).ok(),
                mode: None,
            };
            if wanted(&item.path) {
                let mut data = decompress(open()?, comp).map_err(|e| e.to_string())?;
                write_member(sink, &item, &mut data);
            }
        }
    }
    Ok(())
}

/// Wraps the output file in the right compressor for `.tar.*`.
fn compressor(out: File, comp: Comp) -> io::Result<Box<dyn Write>> {
    let w = io::BufWriter::new(out);
    Ok(match comp {
        Comp::None => Box::new(w),
        Comp::Gz => Box::new(flate2::write::GzEncoder::new(w, flate2::Compression::default())),
        Comp::Bz2 => Box::new(bzip2::write::BzEncoder::new(w, bzip2::Compression::default())),
        Comp::Xz => Box::new(lzma_rust2::XzWriter::new(w, lzma_rust2::XzOptions::with_preset(6))?.auto_finish()),
        Comp::Zst => Box::new(zstd::Encoder::new(w, 3)?.auto_finish()),
    })
}

/// Files and folders to pack: (path on disk, name inside the archive, is_dir).
pub type PackList = Vec<(PathBuf, String, bool)>;

/// Writes a new archive. `read_file` opens a source file and wraps it for
/// progress/cancel; `next` is called before each member (for the status line).
pub fn pack<'a>(
    dest: &Path,
    list: &PackList,
    read_file: &mut dyn FnMut(&Path) -> io::Result<Box<dyn Read + 'a>>,
) -> Result<(), String> {
    let kind = kind_of(dest).ok_or_else(|| "Unbekanntes Format – Endung .zip, .7z, .tar, .tar.gz, .tar.xz, .tar.bz2 oder .tar.zst".to_string())?;
    let out = File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    match kind {
        Kind::Zip => {
            let mut zw = zip::ZipWriter::new(out);
            for (path, name, is_dir) in list {
                let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{name}: {e}"))?;
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(crate::fsutil::mode_of(&meta) & 0o7777)
                    .large_file(meta.len() > u32::MAX as u64);
                if *is_dir {
                    zw.add_directory(format!("{name}/"), opts).map_err(|e| format!("{name}: {e}"))?;
                } else {
                    zw.start_file(name.clone(), opts).map_err(|e| format!("{name}: {e}"))?;
                    io::copy(&mut read_file(path).map_err(|e| format!("{name}: {e}"))?, &mut zw).map_err(|e| format!("{name}: {e}"))?;
                }
            }
            zw.finish().map_err(|e| e.to_string())?;
        }
        Kind::Tar(comp) => {
            let mut builder = tar::Builder::new(compressor(out, comp).map_err(|e| e.to_string())?);
            builder.follow_symlinks(false);
            for (path, name, is_dir) in list {
                let meta = std::fs::metadata(path).map_err(|e| format!("{name}: {e}"))?;
                let mut header = tar::Header::new_gnu();
                header.set_metadata(&meta);
                if *is_dir {
                    header.set_size(0);
                    builder.append_data(&mut header, format!("{name}/"), io::empty()).map_err(|e| format!("{name}: {e}"))?;
                } else {
                    let data = read_file(path).map_err(|e| format!("{name}: {e}"))?;
                    builder.append_data(&mut header, name, data).map_err(|e| format!("{name}: {e}"))?;
                }
            }
            let mut w = builder.into_inner().map_err(|e| e.to_string())?;
            w.flush().map_err(|e| e.to_string())?;
        }
        Kind::SevenZ => {
            let mut sz = sevenz_rust2::ArchiveWriter::new(out).map_err(|e| e.to_string())?;
            for (path, name, is_dir) in list {
                if *is_dir {
                    sz.push_archive_entry::<File>(sevenz_rust2::ArchiveEntry::new_directory(name), None)
                        .map_err(|e| format!("{name}: {e}"))?;
                } else {
                    let entry = sevenz_rust2::ArchiveEntry::from_path(path, name.clone());
                    let data = read_file(path).map_err(|e| format!("{name}: {e}"))?;
                    sz.push_archive_entry(entry, Some(data)).map_err(|e| format!("{name}: {e}"))?;
                }
            }
            sz.finish().map_err(|e| e.to_string())?;
        }
        Kind::Rar => return Err("RAR-Archive können nur gelesen werden – bitte .zip oder .7z wählen".into()),
        Kind::Single(_) => return Err("Bitte ein Archivformat wählen (.zip, .7z, .tar.gz …)".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_paths() {
        assert_eq!(kind_of(Path::new("a.TAR.GZ")), Some(Kind::Tar(Comp::Gz)));
        assert_eq!(kind_of(Path::new("a.tgz")), Some(Kind::Tar(Comp::Gz)));
        assert_eq!(kind_of(Path::new("a.txt.gz")), Some(Kind::Single(Comp::Gz)));
        assert_eq!(kind_of(Path::new("a.7z")), Some(Kind::SevenZ));
        assert_eq!(kind_of(Path::new("comic.cbr")), Some(Kind::Rar));
        assert_eq!(kind_of(Path::new("a.txt")), None);
        assert_eq!(clean_path("./a//b/"), Some("a/b".into()));
        assert_eq!(clean_path("../../etc/passwd"), None);
        assert_eq!(clean_path("/abs/x"), Some("abs/x".into()));
        assert!(selected("proj/src/a.rs", &["proj/src".into()]));
        assert!(!selected("proj/srcx", &["proj/src".into()]));
        assert!(selected("x", &["".into()]));
        assert_eq!(single_name(Path::new("/t/notes.txt.gz")), "notes.txt");
        assert_eq!(stem(Path::new("/t/Urlaub 2024.tar.gz")), "Urlaub 2024");
        assert_eq!(stem(Path::new("a.ZIP")), "a");
        let it = |p: &str| Item { path: p.into(), is_dir: false, size: 0, modified: None, mode: None };
        assert!(!needs_own_folder(&[it("proj"), it("proj/a"), it("proj/b/c")]));
        assert!(!needs_own_folder(&[it("eine_datei.txt")]));
        assert!(needs_own_folder(&[it("a.txt"), it("b.txt")]));
        assert!(needs_own_folder(&[it("proj/a"), it("readme")]));
    }
}

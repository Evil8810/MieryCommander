//! Looking for a new version on GitHub and installing it from inside the app.
//!
//! What can be updated depends on how MieryCommander was installed:
//! - Linux AppImage: the .AppImage file is replaced (`$APPIMAGE`).
//! - Linux tar.gz + install-desktop.sh: the program in ~/.local/bin is replaced.
//! - macOS: the .app bundle is replaced with the one from the new .dmg.
//! - Built from source: only a hint (git pull + cargo build).

use serde::Deserialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const API: &str = "https://api.github.com/repos/Evil8810/MieryCommander/releases/latest";

#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    /// "v0.1.2" → "0.1.2"
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    /// The release notes in the UI language (the notes have a "## Deutsch"
    /// and a "## English" part).
    pub fn notes(&self, english: bool) -> String {
        let body = self.body.replace("\r\n", "\n");
        let (de, en) = match body.split_once("## English") {
            Some((de, en)) => (de.replace("## Deutsch", ""), en.to_string()),
            None => (body.clone(), body.clone()),
        };
        let text = if english { en } else { de };
        text.trim().replace("**", "").replace('`', "")
    }
}

/// How this copy of MieryCommander was installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// Linux AppImage at this path.
    AppImage(PathBuf),
    /// Linux program file installed from the tar.gz (in ~/.local/bin).
    Binary(PathBuf),
    /// macOS app bundle.
    MacApp(PathBuf),
    /// Built from source or unknown: update by hand.
    Manual,
}

impl Install {
    pub fn can_install(&self) -> bool {
        *self != Install::Manual
    }
}

pub fn install_kind() -> Install {
    let exe = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok());
    detect(exe.as_deref(), std::env::var_os("APPIMAGE").map(PathBuf::from), dirs::home_dir())
}

fn detect(exe: Option<&Path>, appimage: Option<PathBuf>, home: Option<PathBuf>) -> Install {
    if cfg!(target_os = "macos") {
        if let Some(app) = exe.and_then(|e| e.ancestors().find(|a| a.extension().is_some_and(|x| x == "app"))) {
            return Install::MacApp(app.to_path_buf());
        }
        return Install::Manual;
    }
    if let Some(a) = appimage.filter(|a| a.is_file()) {
        return Install::AppImage(a);
    }
    if let (Some(exe), Some(home)) = (exe, home) {
        let bin = home.join(".local/bin");
        let bin = bin.canonicalize().unwrap_or(bin);
        if exe.parent() == Some(bin.as_path()) {
            return Install::Binary(exe.to_path_buf());
        }
    }
    Install::Manual
}

/// Is `latest` newer than `current`? ("0.1.10" > "0.1.9")
pub fn newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v').split(['.', '-']).map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parse(latest), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(600)))
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .user_agent(format!("MieryCommander/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// The newest release, if it is newer than this program. Call off the UI thread.
pub fn check() -> Result<Option<Release>, String> {
    let release: Release = agent()
        .get(API)
        .header("Accept", "application/vnd.github+json")
        .call()
        .and_then(|mut r| r.body_mut().read_json())
        .map_err(|e| lf!("Update-Prüfung fehlgeschlagen: {e}", "Update check failed: {e}"))?;
    Ok(newer(release.version(), env!("CARGO_PKG_VERSION")).then_some(release))
}

/// The download that fits this installation.
pub fn pick_asset<'a>(rel: &'a Release, kind: &Install) -> Option<&'a Asset> {
    let arch = std::env::consts::ARCH; // "x86_64", "aarch64"
    rel.assets.iter().find(|a| match kind {
        Install::AppImage(_) => a.name.ends_with(&format!("-{arch}.AppImage")),
        Install::Binary(_) => a.name.ends_with(&format!("-linux-{arch}.tar.gz")),
        Install::MacApp(_) => a.name.ends_with(".dmg"),
        Install::Manual => false,
    })
}

/// Download `asset` into `dest`, counting the bytes in `progress`.
fn download(asset: &Asset, dest: &Path, progress: &AtomicU64) -> Result<(), String> {
    let resp = agent().get(&asset.url).call().map_err(|e| lf!("Download fehlgeschlagen: {e}", "Download failed: {e}"))?;
    let mut reader = resp.into_body().into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| lf!("Download fehlgeschlagen: {e}", "Download failed: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > asset.size {
            return Err(l!("Download ist größer als angekündigt – abgebrochen", "Download is bigger than announced – cancelled").into());
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        progress.store(total, Ordering::Relaxed);
        crate::fsutil::wake_ui();
    }
    file.sync_all().map_err(|e| e.to_string())?;
    if total != asset.size {
        return Err(lf!("Download unvollständig ({total} von {} Bytes)", "Download incomplete ({total} of {} bytes)", asset.size));
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

/// A temporary file next to `target` (same file system, so the final rename is atomic).
fn sibling(target: &Path, suffix: &str) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(".{name}.{suffix}"))
}

/// Download and install the update. Blocks; call off the UI thread.
pub fn install(rel: &Release, kind: &Install, progress: Arc<AtomicU64>) -> Result<(), String> {
    let asset = pick_asset(rel, kind).ok_or_else(|| l!("Kein passender Download in diesem Release", "No matching download in this release").to_string())?;
    match kind {
        Install::AppImage(target) => {
            let tmp = sibling(target, "download");
            let r = download(asset, &tmp, &progress).and_then(|_| make_executable(&tmp)).and_then(|_| std::fs::rename(&tmp, target).map_err(|e| e.to_string()));
            if r.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            r
        }
        Install::Binary(target) => {
            let archive = sibling(target, "tar.gz");
            let tmp = sibling(target, "download");
            let r = download(asset, &archive, &progress)
                .and_then(|_| extract_program(&archive, &tmp))
                .and_then(|_| make_executable(&tmp))
                .and_then(|_| std::fs::rename(&tmp, target).map_err(|e| e.to_string()));
            let _ = std::fs::remove_file(&archive);
            if r.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            r
        }
        Install::MacApp(app) => mac_install(asset, app, &progress),
        Install::Manual => Err(l!("Selbst gebaut – bitte mit git pull und cargo build aktualisieren", "Built from source – please update with git pull and cargo build").into()),
    }
}

/// The program file `…/miery_commander` from the tar.gz.
fn extract_program(archive: &Path, dest: &Path) -> Result<(), String> {
    let f = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if path.file_name().is_some_and(|n| n == "miery_commander") && entry.header().entry_type().is_file() {
            let mut out = std::fs::File::create(dest).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err(l!("Programm nicht im Archiv gefunden", "Program not found in the archive").into())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn run(cmd: &mut std::process::Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn mac_install(asset: &Asset, app: &Path, progress: &AtomicU64) -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        Err("macOS only".into())
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let work = std::env::temp_dir().join(format!("miery-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let dmg = work.join(&asset.name);
        let mnt = work.join("mnt");
        let result = (|| {
            download(asset, &dmg, progress)?;
            std::fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;
            run(Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"]).arg(&mnt).arg(&dmg))?;
            let r = (|| {
                let src = std::fs::read_dir(&mnt)
                    .map_err(|e| e.to_string())?
                    .flatten()
                    .map(|e| e.path())
                    .find(|p| p.extension().is_some_and(|x| x == "app"))
                    .ok_or_else(|| l!("Keine App in der .dmg gefunden", "No app found in the .dmg").to_string())?;
                let staged = sibling(app, "new");
                let old = sibling(app, "old");
                let _ = std::fs::remove_dir_all(&staged);
                let _ = std::fs::remove_dir_all(&old);
                run(Command::new("ditto").arg(&src).arg(&staged)).map_err(|e| {
                    lf!("Kein Schreibrecht für {}: {e}", "No write permission for {}: {e}", app.parent().unwrap_or(app).display())
                })?;
                std::fs::rename(app, &old).map_err(|e| e.to_string())?;
                if let Err(e) = std::fs::rename(&staged, app) {
                    let _ = std::fs::rename(&old, app); // put the old version back
                    return Err(e.to_string());
                }
                let _ = std::fs::remove_dir_all(&old);
                Ok(())
            })();
            let _ = Command::new("hdiutil").args(["detach", "-quiet"]).arg(&mnt).output();
            r
        })();
        let _ = std::fs::remove_dir_all(&work);
        result
    }
}

/// Start the freshly installed version (after this one has closed).
pub fn restart(kind: &Install) -> Result<(), String> {
    let (script, target) = match kind {
        Install::AppImage(p) | Install::Binary(p) => ("sleep 1; exec \"$0\"", p),
        Install::MacApp(app) => ("sleep 1; open -n \"$0\"", app),
        Install::Manual => return Ok(()),
    };
    std::process::Command::new("sh")
        .args(["-c", script])
        .arg(target)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("0.1.2", "0.1.1"));
        assert!(newer("v0.1.10", "0.1.9"));
        assert!(newer("1.0.0", "0.9.9"));
        assert!(!newer("0.1.1", "0.1.1"));
        assert!(!newer("0.1.0", "0.1.1"));
    }

    fn release() -> Release {
        serde_json::from_str(
            r###"{"tag_name":"v0.2.0","html_url":"https://github.com/x/y/releases/tag/v0.2.0",
            "body":"## Deutsch\r\n\r\n**Neu:** Updater\r\n\r\n## English\r\n\r\n**New:** `updater`\r\n",
            "assets":[
              {"name":"MieryCommander-0.2.0-linux-x86_64.tar.gz","browser_download_url":"https://e/t","size":10},
              {"name":"MieryCommander-0.2.0-macos-universal.dmg","browser_download_url":"https://e/d","size":20},
              {"name":"MieryCommander-0.2.0-x86_64.AppImage","browser_download_url":"https://e/a","size":30}]}"###,
        )
        .unwrap()
    }

    #[test]
    fn release_json_and_assets() {
        let r = release();
        assert_eq!(r.version(), "0.2.0");
        assert_eq!(r.notes(false), "Neu: Updater");
        assert_eq!(r.notes(true), "New: updater");
        if std::env::consts::ARCH == "x86_64" {
            assert_eq!(pick_asset(&r, &Install::AppImage("/a".into())).unwrap().size, 30);
            assert_eq!(pick_asset(&r, &Install::Binary("/b".into())).unwrap().size, 10);
        }
        assert_eq!(pick_asset(&r, &Install::MacApp("/A.app".into())).unwrap().size, 20);
        assert!(pick_asset(&r, &Install::Manual).is_none());
    }

    #[test]
    fn install_detection() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_path_buf();
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        let image = home.join("MieryCommander.AppImage");
        std::fs::write(&image, "").unwrap();
        let bin = home.join(".local/bin/miery_commander").canonicalize().unwrap_or(home.join(".local/bin/miery_commander"));
        if cfg!(target_os = "macos") {
            let exe = Path::new("/Applications/MieryCommander.app/Contents/MacOS/miery_commander");
            assert_eq!(detect(Some(exe), None, None), Install::MacApp("/Applications/MieryCommander.app".into()));
        } else {
            assert_eq!(detect(Some(Path::new("/tmp/.mount_x/usr/bin/miery_commander")), Some(image.clone()), Some(home.clone())), Install::AppImage(image));
            assert_eq!(detect(Some(&bin), None, Some(home.clone())), Install::Binary(bin.clone()));
            assert_eq!(detect(Some(Path::new("/src/target/release/miery_commander")), None, Some(home)), Install::Manual);
        }
    }

    /// Against the real GitHub release (network): cargo test real_update -- --ignored
    #[test]
    #[ignore]
    fn real_update() {
        let rel: Release = agent().get(API).call().unwrap().body_mut().read_json().unwrap();
        println!("latest: {} ({} assets)", rel.version(), rel.assets.len());
        assert!(check().is_ok());
        let d = tempfile::tempdir().unwrap();
        for kind in [Install::AppImage(d.path().join("MieryCommander.AppImage")), Install::Binary(d.path().join("miery_commander"))] {
            let (Install::AppImage(p) | Install::Binary(p)) = &kind else { unreachable!() };
            std::fs::write(p, "old").unwrap();
            let progress = Arc::new(AtomicU64::new(0));
            install(&rel, &kind, progress.clone()).unwrap();
            let meta = std::fs::metadata(p).unwrap();
            use std::os::unix::fs::PermissionsExt;
            println!("{kind:?}: {} bytes, mode {:o}, progress {}", meta.len(), meta.permissions().mode() & 0o777, progress.load(Ordering::Relaxed));
            assert!(meta.len() > 1_000_000 && meta.permissions().mode() & 0o111 != 0);
        }
        let leftovers: Vec<_> = std::fs::read_dir(d.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 2, "no temporary files left: {leftovers:?}");
    }

    #[test]
    fn program_from_tarball() {
        let d = tempfile::tempdir().unwrap();
        let archive = d.path().join("pkg.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(std::fs::File::create(&archive).unwrap(), flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            for (name, data) in [("pkg/README.md", &b"readme"[..]), ("pkg/miery_commander", &b"ELF-new"[..])] {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o755);
                h.set_cksum();
                tar.append_data(&mut h, name, data).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        let out = d.path().join("prog");
        extract_program(&archive, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"ELF-new");
    }
}

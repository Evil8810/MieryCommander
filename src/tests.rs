//! Headless UI tests: drive the real app through egui_kittest.

use crate::app::MieryApp;
use crate::panel::{Location, natural_cmp};
use eframe::egui::{Event, Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn harness() -> Harness<'static, MieryApp> {
    Harness::builder()
        .with_size([1200.0, 800.0])
        .build_eframe(|cc| MieryApp::new(cc))
}

fn steps(h: &mut Harness<'_, MieryApp>, n: usize) {
    for _ in 0..n {
        h.step();
    }
    // Directories load in the background: wait until both panels are settled.
    let start = Instant::now();
    while h.state().left.tab().loading || h.state().right.tab().loading {
        assert!(start.elapsed() < Duration::from_secs(10), "directory never finished loading");
        std::thread::sleep(Duration::from_millis(2));
        h.step();
    }
}

/// Simulate a key press the way `raw_input_hook` delivers it to the panels.
fn panel_key(h: &mut Harness<'_, MieryApp>, key: Key, mods: Modifiers) {
    h.state_mut().keys.push((key, mods));
    steps(h, 2);
}

/// Type text and press Enter inside the open dialog.
fn dialog_input(h: &mut Harness<'_, MieryApp>, text: Option<&str>) {
    if let Some(t) = text {
        // Replace the prefilled value.
        h.key_press_modifiers(Modifiers::COMMAND, Key::A);
        steps(h, 1);
        h.event(Event::Text(t.into()));
        steps(h, 1);
    }
    h.key_press(Key::Enter);
    steps(h, 2);
}

fn wait_for_job(h: &mut Harness<'_, MieryApp>) {
    let start = Instant::now();
    while h.state().job.is_some() {
        assert!(start.elapsed() < Duration::from_secs(10), "job did not finish");
        std::thread::sleep(Duration::from_millis(20));
        h.step();
    }
    steps(h, 2);
}

fn setup(h: &mut Harness<'_, MieryApp>, left: &Path, right: &Path) {
    let app = h.state_mut();
    app.navigate_active(Location::Dir(left.to_path_buf()));
    app.right_active = true;
    app.navigate_active(Location::Dir(right.to_path_buf()));
    app.right_active = false;
    steps(h, 2);
}

fn select(h: &mut Harness<'_, MieryApp>, name: &str) {
    h.state_mut().active().tab_mut().select_name(name);
    steps(h, 1);
}

#[test]
fn natural_sort() {
    let mut v = vec!["file10", "File2", "file1", "a"];
    v.sort_by(|a, b| natural_cmp(a, b));
    assert_eq!(v, ["a", "file1", "File2", "file10"]);
}

#[test]
fn tab_switches_panel() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    assert!(!h.state().right_active);
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    assert!(h.state().right_active);
}

#[test]
fn f7_creates_folder_and_enter_opens_it() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    panel_key(&mut h, Key::F7, Modifiers::NONE);
    assert!(h.state().dialog.is_some());
    dialog_input(&mut h, Some("neu/unter"));
    assert!(l.path().join("neu/unter").is_dir());
    assert!(h.state().dialog.is_none());
    // The cursor is on "neu" now; Enter walks into it, Backspace back out.
    panel_key(&mut h, Key::Enter, Modifiers::NONE);
    assert_eq!(h.state().active_dir(), l.path().join("neu"));
    panel_key(&mut h, Key::Backspace, Modifiers::NONE);
    assert_eq!(h.state().active_dir(), l.path());
    assert_eq!(h.state().active_ref().tab().current().unwrap().name, "neu");
}

#[test]
fn f5_copies_marked_files_and_dirs() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(l.path().join("a.txt"), "hallo").unwrap();
    fs::write(l.path().join("b.txt"), "welt").unwrap();
    fs::create_dir_all(l.path().join("dir/sub")).unwrap();
    fs::write(l.path().join("dir/sub/c.txt"), "tief").unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "a.txt");
    panel_key(&mut h, Key::Insert, Modifiers::NONE);
    select(&mut h, "dir");
    panel_key(&mut h, Key::Insert, Modifiers::NONE);
    panel_key(&mut h, Key::F5, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(r.path().join("a.txt")).unwrap(), "hallo");
    assert_eq!(fs::read_to_string(r.path().join("dir/sub/c.txt")).unwrap(), "tief");
    assert!(!r.path().join("b.txt").exists());
    assert!(l.path().join("a.txt").exists(), "copy must keep the source");
}

#[test]
fn f6_moves_and_overwrite_question_appears() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(l.path().join("x.txt"), "neu").unwrap();
    fs::write(r.path().join("x.txt"), "alt").unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "x.txt");
    panel_key(&mut h, Key::F6, Modifiers::NONE);
    dialog_input(&mut h, None);
    // Wait until the worker asks.
    let start = Instant::now();
    loop {
        let asked = h
            .state()
            .job
            .as_ref()
            .is_some_and(|j| j.progress.lock().unwrap().question.is_some());
        if asked {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "no overwrite question");
        std::thread::sleep(Duration::from_millis(20));
        h.step();
    }
    steps(&mut h, 2); // a new modal is not clickable in its first (sizing) frame
    h.get_by_label("Überschreiben").click();
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(r.path().join("x.txt")).unwrap(), "neu");
    assert!(!l.path().join("x.txt").exists());
}

#[test]
fn delete_permanent_with_confirmation() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(l.path().join("weg.txt"), "x").unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "weg.txt");
    panel_key(&mut h, Key::Delete, Modifiers::SHIFT);
    assert!(h.state().dialog.is_some());
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert!(!l.path().join("weg.txt").exists());
}

#[test]
fn pack_browse_and_extract_zip() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::create_dir_all(l.path().join("proj/src")).unwrap();
    fs::write(l.path().join("proj/src/main.rs"), "fn main() {}").unwrap();
    fs::write(l.path().join("proj/README"), "lies mich").unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "proj");
    panel_key(&mut h, Key::F5, Modifiers::ALT);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    let zip = r.path().join("proj.zip");
    assert!(zip.is_file());

    // Open the archive in the right panel like a folder.
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    select(&mut h, "proj.zip");
    panel_key(&mut h, Key::Enter, Modifiers::NONE);
    assert!(matches!(h.state().active_ref().tab().loc, Location::Archive { .. }));
    select(&mut h, "proj");
    panel_key(&mut h, Key::Enter, Modifiers::NONE);
    let names: Vec<String> = h.state().active_ref().tab().entries.iter().map(|e| e.name.clone()).collect();
    assert!(names.contains(&"src".to_string()) && names.contains(&"README".to_string()), "{names:?}");

    // Copy "src" out of the archive into the left panel.
    select(&mut h, "src");
    panel_key(&mut h, Key::F5, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(l.path().join("src/main.rs")).unwrap(), "fn main() {}");
}

#[test]
fn quick_search_and_filter() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for n in ["alpha.txt", "beta.txt", "gamma.md"] {
        fs::write(l.path().join(n), "").unwrap();
    }
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    h.state_mut().texts.push("g".into());
    steps(&mut h, 2);
    assert_eq!(h.state().active_ref().tab().current().unwrap().name, "gamma.md");
    panel_key(&mut h, Key::Escape, Modifiers::NONE);

    h.state_mut().active().tab_mut().filter = "txt".into();
    h.state_mut().active().tab_mut().apply_view(true);
    let names: Vec<String> = h.state().active_ref().tab().entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, ["..", "alpha.txt", "beta.txt"]);
}

#[test]
fn select_pattern_marks_group() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for n in ["a.jpg", "b.JPG", "c.png", "d.txt"] {
        fs::write(l.path().join(n), "").unwrap();
    }
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    h.state_mut().texts.push("+".into());
    steps(&mut h, 2);
    dialog_input(&mut h, Some("*.jpg;*.png"));
    let mut marked: Vec<String> = h.state().active_ref().tab().marked.iter().cloned().collect();
    marked.sort();
    assert_eq!(marked, ["a.jpg", "b.JPG", "c.png"]);
}

// ----------------------------------------------------------------------------
// FTP tests: need a server, see README ("Tests"). Run with
//   MIERY_FTP_TEST=127.0.0.1 MIERY_FTP_ROOT=/srv/root cargo test ftp_
// Plain FTP on port 2121, explicit FTPS (self-signed) on 2122, user test/geheim.
// ----------------------------------------------------------------------------

use crate::dialogs::{Dialog, FtpForm};
use crate::remote::{Protocol, Security, Site};

fn ftp_env() -> Option<(String, std::path::PathBuf)> {
    let host = std::env::var("MIERY_FTP_TEST").ok()?;
    let root = std::env::var("MIERY_FTP_ROOT").ok()?;
    Some((host, root.into()))
}

fn site(host: &str, port: u16, security: Security, accept_invalid: bool) -> Site {
    Site {
        host: host.into(),
        port,
        user: "test".into(),
        security,
        accept_invalid_certs: accept_invalid,
        ..Default::default()
    }
}

fn wait_until(h: &mut Harness<'_, MieryApp>, what: &str, mut cond: impl FnMut(&MieryApp) -> bool) {
    let start = Instant::now();
    while !cond(h.state()) {
        assert!(start.elapsed() < Duration::from_secs(15), "timeout waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
        h.step();
    }
    steps(h, 2);
}

/// Connect through the real dialog (Strg+F → form → Enter).
fn connect_via_dialog(h: &mut Harness<'_, MieryApp>, site: Site) {
    panel_key(h, Key::F, Modifiers::COMMAND);
    assert!(matches!(h.state().dialog, Some(Dialog::FtpConnect(_))));
    let mut form = FtpForm::new(&[]);
    form.site = site;
    form.password = "geheim".into();
    h.state_mut().dialog = Some(Dialog::FtpConnect(form));
    steps(h, 2);
    h.key_press(Key::Enter);
    steps(h, 2);
    wait_until(h, "connect", |a| a.ftp_connecting.is_none());
    if let Some(Dialog::FtpConnect(f)) = &h.state().dialog {
        panic!("connect failed: {:?}", f.status);
    }
    wait_until(h, "listing", |a| !a.active_ref().tab().loading);
}

fn remote_names(app: &MieryApp) -> Vec<String> {
    app.active_ref().tab().entries.iter().map(|e| e.name.clone()).collect()
}

#[test]
fn ftp_plain_full_workflow() {
    let Some((host, root)) = ftp_env() else { return };
    let base = root.join("plain");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();

    let local = tempfile::tempdir().unwrap();
    fs::write(local.path().join("hallo.txt"), "Hallo FTP").unwrap();
    fs::create_dir_all(local.path().join("ordner/tief")).unwrap();
    fs::write(local.path().join("ordner/tief/x.bin"), vec![7u8; 300_000]).unwrap();
    let back = tempfile::tempdir().unwrap();

    let mut h = harness();
    setup(&mut h, local.path(), back.path());
    // Right panel → FTP.
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    let mut s = site(&host, 2121, Security::None, false);
    s.remote_dir = "/plain".into();
    connect_via_dialog(&mut h, s);
    assert!(h.state().active_ref().tab().loc.ftp().is_some());

    // Upload hallo.txt + ordner from the left panel with F5.
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    select(&mut h, "hallo.txt");
    panel_key(&mut h, Key::Insert, Modifiers::NONE);
    select(&mut h, "ordner");
    panel_key(&mut h, Key::Insert, Modifiers::NONE);
    panel_key(&mut h, Key::F5, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(base.join("hallo.txt")).unwrap(), "Hallo FTP");
    assert_eq!(fs::read(base.join("ordner/tief/x.bin")).unwrap().len(), 300_000);

    // Remote listing shows them.
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    wait_until(&mut h, "relist", |a| !a.active_ref().tab().loading);
    let names = remote_names(h.state());
    assert!(names.contains(&"hallo.txt".into()) && names.contains(&"ordner".into()), "{names:?}");

    // F7 on the server.
    panel_key(&mut h, Key::F7, Modifiers::NONE);
    dialog_input(&mut h, Some("neu/sub"));
    wait_until(&mut h, "mkdir", |a| a.ftp_ops.is_empty());
    assert!(base.join("neu/sub").is_dir());

    // Shift+F6 rename on the server.
    wait_until(&mut h, "relist", |a| !a.active_ref().tab().loading);
    select(&mut h, "hallo.txt");
    panel_key(&mut h, Key::F6, Modifiers::SHIFT);
    dialog_input(&mut h, Some("umbenannt.txt"));
    wait_until(&mut h, "rename", |a| a.ftp_ops.is_empty());
    assert!(base.join("umbenannt.txt").is_file() && !base.join("hallo.txt").exists());

    // Enter walks into a remote folder, Backspace goes back.
    wait_until(&mut h, "relist", |a| !a.active_ref().tab().loading);
    select(&mut h, "ordner");
    panel_key(&mut h, Key::Enter, Modifiers::NONE);
    wait_until(&mut h, "enter", |a| !a.active_ref().tab().loading);
    assert_eq!(remote_names(h.state()), ["..", "tief"]);
    panel_key(&mut h, Key::Backspace, Modifiers::NONE);
    wait_until(&mut h, "back", |a| !a.active_ref().tab().loading);
    assert_eq!(h.state().active_ref().tab().current().unwrap().name, "ordner");

    // Download the folder into the left panel's other dir via F5.
    h.state_mut().left.tab_mut().navigate(Location::Dir(back.path().to_path_buf()), false, true);
    panel_key(&mut h, Key::F5, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read(back.path().join("ordner/tief/x.bin")).unwrap(), vec![7u8; 300_000]);

    // Uploading again asks before overwriting; "Alle überspringen" keeps the server file.
    fs::write(base.join("umbenannt.txt"), "Server-Version").unwrap();
    fs::write(back.path().join("umbenannt.txt"), "lokal").unwrap();
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    panel_key(&mut h, Key::R, Modifiers::COMMAND);
    select(&mut h, "umbenannt.txt");
    assert_eq!(h.state().active_ref().tab().current().unwrap().name, "umbenannt.txt");
    panel_key(&mut h, Key::F5, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_until(&mut h, "question", |a| {
        a.job.as_ref().is_some_and(|j| j.progress.lock().unwrap().question.is_some())
    });
    h.get_by_label("Alle überspringen").click();
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(base.join("umbenannt.txt")).unwrap(), "Server-Version");

    // F8 deletes recursively on the server.
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    wait_until(&mut h, "relist", |a| !a.active_ref().tab().loading);
    select(&mut h, "ordner");
    panel_key(&mut h, Key::F8, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert!(!base.join("ordner").exists());

    // Disconnect returns the panel to a local folder.
    panel_key(&mut h, Key::F, Modifiers::COMMAND | Modifiers::SHIFT);
    assert!(h.state().active_ref().tab().loc.dir().is_some());
}

#[test]
fn ftp_explicit_tls() {
    let Some((host, root)) = ftp_env() else { return };
    let base = root.join("tls");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("geheim.txt"), "verschlüsselt").unwrap();

    // The self-signed test certificate must be rejected by default…
    let err = crate::remote::connect(site(&host, 2122, Security::Explicit, false), "geheim".into(), None);
    assert!(err.is_err(), "untrusted certificate was accepted");

    // …and accepted when the user allows it.
    let local = tempfile::tempdir().unwrap();
    let mut h = harness();
    setup(&mut h, local.path(), local.path());
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    let mut s = site(&host, 2122, Security::Explicit, true);
    s.remote_dir = "/tls".into();
    connect_via_dialog(&mut h, s);
    assert_eq!(remote_names(h.state()), ["..", "geheim.txt"]);
    select(&mut h, "geheim.txt");
    panel_key(&mut h, Key::F5, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(local.path().join("geheim.txt")).unwrap(), "verschlüsselt");

    // F3 on a remote file downloads it into the cache and opens the lister.
    panel_key(&mut h, Key::F3, Modifiers::NONE);
    wait_for_job(&mut h);
    assert_eq!(h.state().viewers.len(), 1);
    h.state_mut().viewers.clear();

    // F4 flow: the cached copy is edited → app offers re-upload → server file replaced.
    let cached = crate::remote::cache_dir(h.state().active_ref().tab().loc.ftp().unwrap().0).join("tls/geheim.txt");
    assert!(cached.is_file());
    let conn = h.state().active_ref().tab().loc.ftp().unwrap().0;
    h.state_mut().ftp_edits.push(crate::app::FtpEdit {
        local: cached.clone(),
        conn,
        remote: "/tls/geheim.txt".into(),
        mtime: fs::metadata(&cached).and_then(|m| m.modified()).ok(),
    });
    std::thread::sleep(Duration::from_millis(20));
    fs::write(&cached, "bearbeitet").unwrap();
    steps(&mut h, 2);
    assert!(matches!(h.state().dialog, Some(Dialog::FtpReupload { .. })));
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(base.join("geheim.txt")).unwrap(), "bearbeitet");
}

#[test]
fn ftp_wrong_password_shows_error_in_dialog() {
    let Some((host, _)) = ftp_env() else { return };
    let mut h = harness();
    panel_key(&mut h, Key::F, Modifiers::COMMAND);
    let mut form = FtpForm::new(&[]);
    form.site = site(&host, 2121, Security::None, false);
    form.password = "falsch".into();
    h.state_mut().dialog = Some(Dialog::FtpConnect(form));
    steps(&mut h, 2);
    h.key_press(Key::Enter);
    steps(&mut h, 2);
    wait_until(&mut h, "connect", |a| a.ftp_connecting.is_none());
    match &h.state().dialog {
        Some(Dialog::FtpConnect(f)) => assert!(f.status.as_ref().is_some_and(|(e, m)| *e && m.contains("Anmeldung"))),
        _ => panic!("dialog should stay open with an error"),
    }
}

// ----------------------------------------------------------------------------
// SFTP test: needs an SSH server on port 2222 (user test/geheim, the keys in
// MIERY_SFTP_KEYS: host_key, other_host_key, client_key, client_key_enc with
// passphrase "geheimsatz"). Uses its own known_hosts file, never ~/.ssh.
//   MIERY_SFTP_TEST=127.0.0.1 MIERY_SFTP_ROOT=… MIERY_SFTP_KEYS=… cargo test sftp
// ----------------------------------------------------------------------------

fn sftp_site(host: &str) -> Site {
    Site {
        protocol: Protocol::Sftp,
        host: host.into(),
        port: 2222,
        user: "test".into(),
        auto_keys: false, // never touch the developer's agent or ~/.ssh in tests
        ..Default::default()
    }
}

fn host_fingerprint(keys: &Path) -> String {
    let text = fs::read_to_string(keys.join("host_key.pub")).unwrap();
    let key = russh::keys::PublicKey::from_openssh(text.trim()).unwrap();
    crate::sftp::fingerprint(&key)
}

/// Fill the connect dialog and press Enter; returns once connecting finished.
fn sftp_dialog_connect(h: &mut Harness<'_, MieryApp>, site: Site, password: &str) {
    panel_key(h, Key::F, Modifiers::COMMAND);
    let mut form = FtpForm::new(&[]);
    form.site = site;
    form.password = password.into();
    h.state_mut().dialog = Some(Dialog::FtpConnect(form));
    steps(h, 2);
    h.key_press(Key::Enter);
    steps(h, 2);
    wait_until(h, "connect", |a| a.ftp_connecting.is_none());
}

fn dialog_form<'a>(h: &'a Harness<'_, MieryApp>) -> Option<&'a FtpForm> {
    match &h.state().dialog {
        Some(Dialog::FtpConnect(f)) => Some(f),
        _ => None,
    }
}

#[test]
fn sftp_workflow() {
    let (Ok(host), Ok(root), Ok(keys)) = (
        std::env::var("MIERY_SFTP_TEST"),
        std::env::var("MIERY_SFTP_ROOT"),
        std::env::var("MIERY_SFTP_KEYS"),
    ) else {
        return;
    };
    let (root, keys) = (std::path::PathBuf::from(root), std::path::PathBuf::from(keys));
    let base = root.join("work");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let kh_dir = tempfile::tempdir().unwrap();
    let known_hosts = kh_dir.path().join("known_hosts");
    crate::sftp::set_known_hosts_path(known_hosts.clone());

    let local = tempfile::tempdir().unwrap();
    fs::write(local.path().join("notiz.txt"), "über SSH").unwrap();
    fs::create_dir_all(local.path().join("projekt/src")).unwrap();
    fs::write(local.path().join("projekt/src/main.rs"), vec![b'x'; 700_000]).unwrap();
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    fs::File::options().write(true).open(local.path().join("notiz.txt")).unwrap().set_modified(old).unwrap();
    let back = tempfile::tempdir().unwrap();

    let mut h = harness();
    setup(&mut h, local.path(), back.path());
    panel_key(&mut h, Key::Tab, Modifiers::NONE);

    // 1. Unknown host: the dialog shows the fingerprint and waits.
    let mut s = sftp_site(&host);
    s.remote_dir = "/work".into();
    sftp_dialog_connect(&mut h, s.clone(), "geheim");
    let form = dialog_form(&h).expect("dialog must stay open for an unknown host");
    assert_eq!(form.host_key.as_deref(), Some(host_fingerprint(&keys).as_str()));
    assert!(!known_hosts.exists(), "nothing may be trusted before the user confirms");

    // 2. "Vertrauen und verbinden" stores the key and connects (password auth).
    h.get_by_label("Vertrauen und verbinden").click();
    steps(&mut h, 2);
    wait_until(&mut h, "connect", |a| a.ftp_connecting.is_none());
    assert!(dialog_form(&h).is_none(), "connect failed: {:?}", dialog_form(&h).map(|f| &f.status));
    wait_until(&mut h, "listing", |a| !a.active_ref().tab().loading);
    assert!(fs::read_to_string(&known_hosts).unwrap().contains("[127.0.0.1]:2222 ssh-ed25519 "));
    assert_eq!(h.state().active_ref().tab().loc.display(), format!("sftp://test@{host}/work"));

    // 3. Upload file + folder (F5), mtime is preserved on the server.
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    select(&mut h, "notiz.txt");
    panel_key(&mut h, Key::Insert, Modifiers::NONE);
    select(&mut h, "projekt");
    panel_key(&mut h, Key::Insert, Modifiers::NONE);
    panel_key(&mut h, Key::F5, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(base.join("notiz.txt")).unwrap(), "über SSH");
    assert_eq!(fs::read(base.join("projekt/src/main.rs")).unwrap().len(), 700_000);
    assert_eq!(fs::metadata(base.join("notiz.txt")).unwrap().modified().unwrap(), old);

    // 4. Remote panel: listing, F7, Shift+F6, Enter/Backspace.
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    wait_until(&mut h, "relist", |a| !a.active_ref().tab().loading);
    assert_eq!(remote_names(h.state()), ["..", "projekt", "notiz.txt"]);
    panel_key(&mut h, Key::F7, Modifiers::NONE);
    dialog_input(&mut h, Some("a/b/c"));
    wait_until(&mut h, "mkdir", |a| a.ftp_ops.is_empty());
    assert!(base.join("a/b/c").is_dir());
    wait_until(&mut h, "relist", |a| !a.active_ref().tab().loading);
    select(&mut h, "notiz.txt");
    panel_key(&mut h, Key::F6, Modifiers::SHIFT);
    dialog_input(&mut h, Some("notiz-neu.txt"));
    wait_until(&mut h, "rename", |a| a.ftp_ops.is_empty());
    assert!(base.join("notiz-neu.txt").is_file());
    wait_until(&mut h, "relist", |a| !a.active_ref().tab().loading);
    select(&mut h, "projekt");
    panel_key(&mut h, Key::Enter, Modifiers::NONE);
    wait_until(&mut h, "enter", |a| !a.active_ref().tab().loading);
    assert_eq!(remote_names(h.state()), ["..", "src"]);
    panel_key(&mut h, Key::Backspace, Modifiers::NONE);
    wait_until(&mut h, "back", |a| !a.active_ref().tab().loading);

    // 5. F6 download-move: folder ends up locally and disappears from the server.
    h.state_mut().left.tab_mut().navigate(Location::Dir(back.path().to_path_buf()), false, true);
    select(&mut h, "projekt");
    panel_key(&mut h, Key::F6, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read(back.path().join("projekt/src/main.rs")).unwrap().len(), 700_000);
    assert!(!base.join("projekt").exists(), "move must delete on the server");

    // 6. F8 deletes recursively on the server; then disconnect.
    wait_until(&mut h, "relist", |a| !a.active_ref().tab().loading);
    select(&mut h, "a");
    panel_key(&mut h, Key::F8, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert!(!base.join("a").exists());
    panel_key(&mut h, Key::F, Modifiers::COMMAND | Modifiers::SHIFT);
    assert!(h.state().active_ref().tab().loc.dir().is_some());

    // 7. Key file auth (known host now, no prompt) and encrypted key with passphrase.
    let mut s = sftp_site(&host);
    s.key_file = keys.join("client_key").to_string_lossy().into_owned();
    let c = crate::remote::connect(s, String::new(), None).expect("key auth");
    assert_eq!(c.home(), "/");
    crate::remote::disconnect(c.id());
    let mut s = sftp_site(&host);
    s.key_file = keys.join("client_key_enc").to_string_lossy().into_owned();
    let c = crate::remote::connect(s.clone(), "geheimsatz".into(), None).expect("encrypted key auth");
    crate::remote::disconnect(c.id());
    let err = crate::remote::connect(s, "falsch".into(), None).err().unwrap().to_string();
    assert!(err.contains("Schlüssel"), "{err}");

    // 8. Wrong password → error in the dialog.
    sftp_dialog_connect(&mut h, sftp_site(&host), "falsch");
    let st = dialog_form(&h).and_then(|f| f.status.clone()).unwrap();
    assert!(st.0 && st.1.contains("Anmeldung fehlgeschlagen"), "{st:?}");
    h.state_mut().dialog = None;

    // 9. Changed host key → hard error, no prompt, nothing learned.
    let other = fs::read_to_string(keys.join("other_host_key.pub")).unwrap();
    let mut parts = other.split_whitespace();
    fs::write(&known_hosts, format!("[127.0.0.1]:2222 {} {}\n", parts.next().unwrap(), parts.next().unwrap())).unwrap();
    match crate::remote::connect(sftp_site(&host), "geheim".into(), None) {
        Err(crate::remote::ConnectError::Failed(m)) => assert!(m.contains("geändert"), "{m}"),
        other => panic!("expected key-changed error, got {:?}", other.map(|c| c.url())),
    }
    // …even if a (stale) fingerprint is passed as trusted.
    assert!(crate::remote::connect(sftp_site(&host), "geheim".into(), Some(host_fingerprint(&keys))).is_err());
}


#[test]
fn slow_directory_shows_progress_and_can_be_cancelled() {
    use egui_kittest::kittest::Queryable;
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::create_dir(l.path().join("__slow__")).unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());

    // Navigate into the "hanging" folder without waiting for it.
    h.state_mut().navigate_active(Location::Dir(l.path().join("__slow__")));
    for _ in 0..5 {
        h.step(); // the UI keeps running while the folder loads
    }
    assert!(h.state().active_ref().tab().loading);
    // Shown twice: in the panel and in the status corner at the bottom.
    assert_eq!(h.query_all_by_label_contains("Lade Verzeichnis").count(), 2);
    assert!(h.query_by_label("Abbrechen").is_none(), "no cancel button during the first seconds");

    // After 3 s the panel explains the wait and offers to cancel.
    std::thread::sleep(Duration::from_millis(3100));
    h.step();
    h.step();
    assert!(h.state().active_ref().tab().loading);
    h.get_by_label("Abbrechen").click();
    h.step();
    h.step();
    assert_eq!(h.state().active_dir(), l.path(), "cancel goes back");
    steps(&mut h, 2);
    assert!(!h.state().active_ref().tab().loading);
    // The late result of the hanging read must not replace the current listing.
    std::thread::sleep(Duration::from_millis(700));
    steps(&mut h, 2);
    assert_eq!(h.state().active_dir(), l.path());
    assert!(remote_names(h.state()).contains(&"__slow__".to_string()));
}


/// The connect dialog must always show its whole form and buttons, also after
/// a smaller dialog, after the host-key prompt and after switching protocol.
#[test]
fn connect_dialog_keeps_full_size() {
    use egui_kittest::kittest::Queryable;
    // Widget rects are reported even when clipped away, so look at real pixels:
    // the button's own fill colour must be visible where the button should be.
    fn assert_buttons_visible(h: &mut Harness<'_, MieryApp>, case: &str) {
        let fill = h.ctx.global_style().visuals.widgets.inactive.weak_bg_fill;
        let img = h.render().expect("render");
        for label in ["Verbinden", "Speichern", "Abbrechen"] {
            let r = h.get_by_label(label).rect();
            let (x, y) = ((r.min.x + 3.0) as u32, r.center().y as u32);
            let px = img.get_pixel(x, y).0;
            let diff = (px[0] as i32 - fill.r() as i32).abs()
                + (px[1] as i32 - fill.g() as i32).abs()
                + (px[2] as i32 - fill.b() as i32).abs();
            assert!(diff < 12, "{case}: button {label} is not visible (pixel {px:?}, expected {fill:?})");
        }
    }
    fn form<'a>(h: &'a mut Harness<'_, MieryApp>) -> &'a mut FtpForm {
        match &mut h.state_mut().dialog {
            Some(Dialog::FtpConnect(f)) => f,
            _ => panic!("connect dialog not open"),
        }
    }
    let mut h = harness();
    panel_key(&mut h, Key::F7, Modifiers::NONE);
    h.key_press(Key::Escape);
    steps(&mut h, 3);
    panel_key(&mut h, Key::F, Modifiers::COMMAND);
    steps(&mut h, 3);
    assert_buttons_visible(&mut h, "after a small dialog");

    form(&mut h).host_key = Some("SHA256:abc".into());
    steps(&mut h, 3);
    form(&mut h).host_key = None;
    steps(&mut h, 3);
    assert_buttons_visible(&mut h, "after the host-key prompt");

    form(&mut h).site.protocol = Protocol::Sftp;
    steps(&mut h, 3);
    assert_buttons_visible(&mut h, "after switching to SFTP");

    form(&mut h).site.protocol = Protocol::Smb;
    steps(&mut h, 3);
    assert_buttons_visible(&mut h, "after switching to SMB");

    // Search results: servers and shares appear as clickable choices.
    let f = form(&mut h);
    f.found_hosts = vec![("TrueNAS".into(), "truenas.local".into()), ("Büro".into(), "buero.local".into())];
    f.found_shares = vec!["Media".into(), "backup".into()];
    steps(&mut h, 3);
    if let Ok(out) = std::env::var("MIERY_SMB_SHOT") {
        h.render().unwrap().save(out).unwrap();
    }
    assert_buttons_visible(&mut h, "with SMB search results");
    h.get_by_label("📁 Media").click();
    steps(&mut h, 2);
    assert_eq!(form(&mut h).site.remote_dir, "Media");
    h.get_by_label("🖧 TrueNAS").click();
    steps(&mut h, 2);
    let f = form(&mut h);
    assert_eq!((f.site.host.as_str(), f.site.name.as_str(), f.site.remote_dir.as_str()), ("truenas.local", "TrueNAS", ""));
    // (without smbclient the lookup may already have ended with an error)
    assert!(f.lookup.is_some() || f.status.is_some(), "choosing a server lists its shares");
    f.lookup = None;
    f.found_hosts.clear();
    f.found_shares.clear();
    // A long error message makes the dialog taller – the buttons must stay visible.
    form(&mut h).status = Some((true, "Freigaben können nicht abgefragt werden (smbclient): No such file or directory (os error 2) – ".repeat(3)));
    steps(&mut h, 3);
    assert_buttons_visible(&mut h, "with a long error message");
    form(&mut h).status = None;
    form(&mut h).site.protocol = Protocol::Ftp;
    steps(&mut h, 3);
    assert_buttons_visible(&mut h, "back to FTP");
}

/// Real SMB share through the desktop (kio-fuse). Read-only. Run with
///   MIERY_SMB_TEST=nas.local MIERY_SMB_SHARE=Media cargo test smb_
#[test]
fn smb_share_via_connect_dialog() {
    let (Ok(host), Ok(share)) = (std::env::var("MIERY_SMB_TEST"), std::env::var("MIERY_SMB_SHARE")) else {
        return;
    };
    for (share, expect) in [(share.as_str(), None), ("", Some(share.clone()))] {
        let mut h = harness();
        panel_key(&mut h, Key::F, Modifiers::COMMAND);
        let mut form = FtpForm::new(&[]);
        form.site = Site { protocol: Protocol::Smb, host: host.clone(), remote_dir: share.into(), user: String::new(), ..Default::default() };
        h.state_mut().dialog = Some(Dialog::FtpConnect(form));
        steps(&mut h, 2);
        h.key_press(Key::Enter);
        steps(&mut h, 2);
        wait_until(&mut h, "smb mount", |a| a.smb_mounting.is_none());
        assert!(dialog_form(&h).is_none(), "SMB failed: {:?}", dialog_form(&h).map(|f| &f.status));
        steps(&mut h, 2);
        let dir = h.state().active_dir();
        let names = remote_names(h.state());
        eprintln!("SMB {share:?} -> {} : {names:?}", dir.display());
        assert!(names.len() > 1, "share listing is empty");
        if let Some(s) = expect {
            assert!(names.contains(&s), "share list should contain {s}");
        }
    }
}



// ----------------------------------------------------------------------------
// Clipboard and context menu
// ----------------------------------------------------------------------------

#[test]
fn ctrl_c_ctrl_v_copies_into_other_folder() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(l.path().join("a.txt"), "A").unwrap();
    fs::create_dir_all(l.path().join("ordner/sub")).unwrap();
    fs::write(l.path().join("ordner/sub/b.txt"), "B").unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "a.txt");
    panel_key(&mut h, Key::Insert, Modifiers::NONE);
    select(&mut h, "ordner");
    panel_key(&mut h, Key::Insert, Modifiers::NONE);
    panel_key(&mut h, Key::C, Modifiers::COMMAND);
    assert!(h.state().clip.as_ref().is_some_and(|c| !c.cut && c.paths.len() == 2));
    assert!(h.state().active_ref().tab().marked.is_empty(), "marks are cleared after copying");
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    panel_key(&mut h, Key::V, Modifiers::COMMAND);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(r.path().join("a.txt")).unwrap(), "A");
    assert_eq!(fs::read_to_string(r.path().join("ordner/sub/b.txt")).unwrap(), "B");
    assert!(l.path().join("a.txt").exists(), "copy keeps the original");
    assert!(h.state().clip.is_some(), "a copy can be pasted again");
}

#[test]
fn ctrl_x_ctrl_v_moves_and_clears_clipboard() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(l.path().join("weg.txt"), "W").unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "weg.txt");
    panel_key(&mut h, Key::X, Modifiers::COMMAND);
    assert_eq!(h.state().cut_paths(), [l.path().join("weg.txt")]);
    panel_key(&mut h, Key::Tab, Modifiers::NONE);
    panel_key(&mut h, Key::V, Modifiers::COMMAND);
    wait_for_job(&mut h);
    assert!(r.path().join("weg.txt").exists() && !l.path().join("weg.txt").exists());
    assert!(h.state().clip.is_none(), "moved files are not in the clipboard anymore");
}

#[test]
fn paste_copy_into_same_folder_renames() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(l.path().join("bericht.txt"), "1").unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "bericht.txt");
    panel_key(&mut h, Key::C, Modifiers::COMMAND);
    panel_key(&mut h, Key::V, Modifiers::COMMAND);
    wait_for_job(&mut h);
    panel_key(&mut h, Key::V, Modifiers::COMMAND);
    wait_for_job(&mut h);
    assert!(h.state().job.is_none());
    for n in ["bericht.txt", "bericht (2).txt", "bericht (3).txt"] {
        assert_eq!(fs::read_to_string(l.path().join(n)).unwrap(), "1", "{n}");
    }
}

#[test]
fn context_menu_copy_and_right_click_selection() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for n in ["eins.txt", "zwei.txt", "drei.txt"] {
        fs::write(l.path().join(n), n).unwrap();
    }
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    // Two files are marked, then the user right-clicks a third, unmarked one.
    h.state_mut().active().tab_mut().marked.extend(["eins.txt".to_string(), "zwei.txt".to_string()]);
    steps(&mut h, 1);
    h.get_by_label("📄 drei").click_secondary();
    steps(&mut h, 2);
    assert!(h.state().active_ref().tab().marked.is_empty(), "right-click on an unmarked file must not act on others");
    assert_eq!(h.state().active_ref().tab().current().unwrap().name, "drei.txt");
    // The menu shows "Kopieren", "Kopieren nach…" etc.; the clipboard one carries Strg+C.
    h.get_by_label("Kopieren Strg+C").click();
    steps(&mut h, 2);
    let clip = h.state().clip.clone().expect("menu copied");
    assert_eq!(clip.paths, [l.path().join("drei.txt")]);
}

#[test]
fn fallback_fonts_cover_other_scripts() {
    let defs = crate::fonts::with_fallbacks();
    if defs.font_data.len() == FontDefinitionsDefaultCount::get() {
        return; // no system fonts available (e.g. minimal CI)
    }
    let ctx = eframe::egui::Context::default();
    ctx.set_fonts(defs);
    let mut out = ctx.run_ui(Default::default(), |_| {});
    out.textures_delta.clear();
    let font = eframe::egui::FontId::proportional(14.0);
    for text in ["日本語のファイル", "中文文件", "한국어", "ملف", "קובץ", "ไฟล์"] {
        assert!(ctx.fonts_mut(|f| f.has_glyphs(&font, text)), "no glyphs for {text}");
    }
}

struct FontDefinitionsDefaultCount;
impl FontDefinitionsDefaultCount {
    fn get() -> usize {
        eframe::egui::FontDefinitions::default().font_data.len()
    }
}


#[test]
fn take_folder_from_other_panel() {
    use egui_kittest::kittest::Queryable;
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    // Strg+G: the active (left) panel goes where the right one is.
    panel_key(&mut h, Key::G, Modifiers::COMMAND);
    assert_eq!(h.state().active_dir(), r.path());
    // Back, then the "=" button of the RIGHT panel takes the left panel's folder.
    panel_key(&mut h, Key::ArrowLeft, Modifiers::ALT);
    assert_eq!(h.state().active_dir(), l.path());
    let buttons: Vec<_> = h.query_all_by_label("=").collect();
    assert_eq!(buttons.len(), 2, "one '=' button per panel");
    let right_button = buttons.into_iter().max_by(|a, b| a.rect().min.x.total_cmp(&b.rect().min.x)).unwrap();
    right_button.click();
    steps(&mut h, 2);
    assert!(h.state().right_active);
    assert_eq!(h.state().right.tab().loc.real_dir(), l.path());
    // Strg+= with Shift (German keyboard): the other panel takes this folder.
    h.state_mut().right.tab_mut().navigate(Location::Dir(r.path().to_path_buf()), false, true);
    h.state_mut().right_active = false;
    panel_key(&mut h, Key::Equals, Modifiers::COMMAND | Modifiers::SHIFT);
    assert_eq!(h.state().right.tab().loc.real_dir(), l.path());
}

// ----------------------------------------------------------------------------
// Archives
// ----------------------------------------------------------------------------

fn make_tree(root: &Path) {
    fs::create_dir_all(root.join("projekt/src/tief")).unwrap();
    fs::write(root.join("projekt/README.md"), "lies mich").unwrap();
    fs::write(root.join("projekt/src/main.rs"), vec![b'x'; 200_000]).unwrap();
    fs::write(root.join("projekt/src/tief/日本語.txt"), "こんにちは").unwrap();
}

/// Alt+F5 with the given archive name, then open it in the right panel.
fn pack_and_open(h: &mut Harness<'_, MieryApp>, r: &Path, archive_name: &str) {
    select(h, "projekt");
    panel_key(h, Key::F5, Modifiers::ALT);
    match &mut h.state_mut().dialog {
        Some(Dialog::Pack { target, .. }) => *target = r.join(archive_name).to_string_lossy().into_owned(),
        _ => panic!("pack dialog not open"),
    }
    dialog_input(h, None);
    wait_for_job(h);
    assert!(r.join(archive_name).is_file(), "{archive_name} was not created");
    panel_key(h, Key::Tab, Modifiers::NONE);
    panel_key(h, Key::R, Modifiers::COMMAND);
    select(h, archive_name);
    panel_key(h, Key::Enter, Modifiers::NONE);
    assert!(matches!(h.state().active_ref().tab().loc, Location::Archive { .. }), "{archive_name} not opened");
}

#[test]
fn archive_round_trip_all_writable_formats() {
    for fmt in ["zip", "7z", "tar", "tar.gz", "tar.bz2", "tar.xz", "tar.zst"] {
        let (l, r, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        make_tree(l.path());
        let mut h = harness();
        setup(&mut h, l.path(), r.path());
        let name = format!("paket.{fmt}");
        pack_and_open(&mut h, r.path(), &name);
        assert_eq!(remote_names(h.state()), ["..", "projekt"], "{fmt}");
        select(&mut h, "projekt");
        panel_key(&mut h, Key::Enter, Modifiers::NONE);
        assert_eq!(remote_names(h.state()), ["..", "src", "README.md"], "{fmt}");
        // F5 the "src" folder out of the archive into a third folder.
        h.state_mut().left.tab_mut().navigate(Location::Dir(out.path().to_path_buf()), false, true);
        select(&mut h, "src");
        panel_key(&mut h, Key::F5, Modifiers::NONE);
        dialog_input(&mut h, None);
        wait_for_job(&mut h);
        assert_eq!(fs::read(out.path().join("src/main.rs")).unwrap().len(), 200_000, "{fmt}");
        assert_eq!(fs::read_to_string(out.path().join("src/tief/日本語.txt")).unwrap(), "こんにちは", "{fmt}");
        assert!(!out.path().join("README.md").exists(), "{fmt}: only the selected folder");
    }
}

#[test]
fn archives_from_other_tools_open_and_unpack() {
    let tools: [(&str, &str, &[&str]); 4] = [
        ("rar", "fremd.rar", &["a", "-r", "-inul"]),
        ("7z", "fremd.7z", &["a", "-bd", "-bso0"]),
        ("tar", "fremd.tar.gz", &["czf"]),
        ("tar", "fremd.tar.xz", &["cJf"]),
    ];
    for (tool, name, args) in tools {
        if std::process::Command::new("which").arg(tool).output().map(|o| !o.status.success()).unwrap_or(true) {
            eprintln!("{tool} nicht installiert – übersprungen");
            continue;
        }
        let (src, r, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        make_tree(src.path());
        let status = std::process::Command::new(tool)
            .args(args)
            .arg(r.path().join(name))
            .arg("projekt")
            .current_dir(src.path())
            .status()
            .unwrap();
        assert!(status.success(), "{tool} failed");
        let mut h = harness();
        setup(&mut h, r.path(), out.path());
        select(&mut h, name);
        panel_key(&mut h, Key::Enter, Modifiers::NONE);
        assert_eq!(remote_names(h.state()), ["..", "projekt"], "{name}");
        panel_key(&mut h, Key::Backspace, Modifiers::NONE);
        // Alt+F9: unpack everything into the other panel.
        select(&mut h, name);
        panel_key(&mut h, Key::F9, Modifiers::ALT);
        dialog_input(&mut h, None);
        wait_for_job(&mut h);
        assert_eq!(fs::read(out.path().join("projekt/src/main.rs")).unwrap().len(), 200_000, "{name}");
        assert_eq!(fs::read_to_string(out.path().join("projekt/src/tief/日本語.txt")).unwrap(), "こんにちは", "{name}");
    }
}

#[test]
fn single_gz_file_and_view_inside_archive() {
    use std::io::Write as _;
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut gz = flate2::write::GzEncoder::new(fs::File::create(l.path().join("notiz.txt.gz")).unwrap(), Default::default());
    gz.write_all(b"komprimierter Text").unwrap();
    gz.finish().unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "notiz.txt.gz");
    panel_key(&mut h, Key::Enter, Modifiers::NONE);
    assert_eq!(remote_names(h.state()), ["..", "notiz.txt"]);
    // F3 inside the archive: extracted to the cache, then shown in the lister.
    select(&mut h, "notiz.txt");
    panel_key(&mut h, Key::F3, Modifiers::NONE);
    wait_for_job(&mut h);
    assert_eq!(h.state().viewers.len(), 1);
    panel_key(&mut h, Key::Escape, Modifiers::NONE); // close the lister again
    assert!(h.state().viewers.is_empty());
    // F5 out of the single-file archive.
    panel_key(&mut h, Key::F5, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(r.path().join("notiz.txt")).unwrap(), "komprimierter Text");
}

#[test]
fn malicious_archive_paths_stay_inside_target() {
    use std::io::Write as _;
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let outside = r.path().parent().unwrap().join(format!("miery_evil_{}.txt", std::process::id()));
    let _ = fs::remove_file(&outside);
    // A tar with a "../" member, written byte-wise (tar::Builder would refuse it).
    let mut header = tar::Header::new_gnu();
    header.as_gnu_mut().unwrap().name[..18].copy_from_slice(b"../miery_evil_x.tx");
    let evil_name = format!("../{}", outside.file_name().unwrap().to_string_lossy());
    {
        let gnu = header.as_gnu_mut().unwrap();
        gnu.name = [0; 100];
        gnu.name[..evil_name.len()].copy_from_slice(evil_name.as_bytes());
    }
    header.set_size(4);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    let mut data = Vec::new();
    data.extend_from_slice(header.as_bytes());
    data.extend_from_slice(b"BOSE");
    data.resize(1024, 0);
    let mut ok = tar::Header::new_gnu();
    ok.set_path("harmlos.txt").unwrap();
    ok.set_size(2);
    ok.set_mode(0o644);
    ok.set_cksum();
    data.extend_from_slice(ok.as_bytes());
    data.extend_from_slice(b"ok");
    data.resize(2048, 0);
    data.resize(3072, 0);
    fs::File::create(l.path().join("boese.tar")).unwrap().write_all(&data).unwrap();

    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "boese.tar");
    panel_key(&mut h, Key::F9, Modifiers::ALT);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert!(!outside.exists(), "an archive must never write outside the target folder");
    assert_eq!(fs::read_to_string(r.path().join("harmlos.txt")).unwrap(), "ok");
}

#[test]
fn branch_view_lists_all_files_of_all_subfolders() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    make_tree(l.path());
    fs::write(l.path().join("oben.txt"), "o").unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    panel_key(&mut h, Key::B, Modifiers::COMMAND | Modifiers::SHIFT);
    assert!(h.state().active_ref().tab().branch);
    let mut names = remote_names(h.state());
    names.retain(|n| n != "..");
    // (Windows shows "projekt\\README.md")
    let mut names: Vec<String> = names.into_iter().map(|n| n.replace('\\', "/")).collect();
    names.sort();
    assert_eq!(names, ["oben.txt", "projekt/README.md", "projekt/src/main.rs", "projekt/src/tief/日本語.txt"]);
    // Operations work on the real files: F5 copies the deep file.
    select(&mut h, "projekt/src/tief/日本語.txt");
    panel_key(&mut h, Key::F5, Modifiers::NONE);
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(r.path().join("日本語.txt")).unwrap(), "こんにちは");
    // Still in branch view after the job; leaving the folder switches it off.
    assert!(h.state().active_ref().tab().branch);
    panel_key(&mut h, Key::Backspace, Modifiers::NONE);
    assert!(!h.state().active_ref().tab().branch);
}

#[test]
fn compare_files_side_by_side() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(l.path().join("brief.txt"), "Hallo Anna,\nwie geht es dir?\nBis bald\nBen\n").unwrap();
    fs::write(r.path().join("brief.txt"), "Hallo Anna,\nwie geht es Dir heute?\nBis bald\nBen\nPS: Gruß\n").unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    select(&mut h, "brief.txt");
    h.state_mut().right.tab_mut().select_name("brief.txt");
    let ctx = h.ctx.clone();
    h.state_mut().run(crate::app::Cmd::CompareFiles, &ctx);
    wait_until(&mut h, "compare", |a| a.compares.last().is_some_and(|c| c.outcome().is_some()));
    match h.state().compares[0].outcome().unwrap() {
        crate::compare::Outcome::Text { changes, rows } => {
            assert_eq!(changes.len(), 2, "changed line + appended line");
            assert_eq!(rows.len(), 5);
        }
        o => panic!("unexpected {o:?}"),
    }
    if let Ok(out) = std::env::var("MIERY_SHOT") {
        h.render().unwrap().save(out).unwrap();
    }
    panel_key(&mut h, Key::N, Modifiers::NONE);
    panel_key(&mut h, Key::Escape, Modifiers::NONE);
    assert!(h.state().compares.is_empty());

    // Two marked files in one panel work as well.
    fs::write(l.path().join("kopie.txt"), "Hallo Anna,\nwie geht es dir?\nBis bald\nBen\n").unwrap();
    panel_key(&mut h, Key::R, Modifiers::COMMAND);
    h.state_mut().active().tab_mut().marked.extend(["brief.txt".to_string(), "kopie.txt".to_string()]);
    h.state_mut().run(crate::app::Cmd::CompareFiles, &ctx);
    wait_until(&mut h, "compare", |a| a.compares.last().is_some_and(|c| c.outcome().is_some()));
    assert!(matches!(
        h.state().compares[0].outcome(),
        Some(crate::compare::Outcome::Text { changes, .. }) if changes.is_empty()
    ));
}

#[test]
fn synchronize_directories_end_to_end() {
    use egui_kittest::kittest::Queryable;
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    fs::create_dir_all(l.path().join("fotos/2024")).unwrap();
    fs::write(l.path().join("fotos/2024/bild.jpg"), "neues bild").unwrap();
    fs::write(r.path().join("nur_rechts.txt"), "R").unwrap();
    fs::write(l.path().join("beide.txt"), "neu").unwrap();
    fs::write(r.path().join("beide.txt"), "alt").unwrap();
    fs::File::options().write(true).open(r.path().join("beide.txt")).unwrap().set_modified(old).unwrap();
    fs::create_dir_all(r.path().join("leerer_ordner")).unwrap();

    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    let ctx = h.ctx.clone();
    h.state_mut().run(crate::app::Cmd::SyncDirs, &ctx);
    wait_until(&mut h, "scan", |a| a.sync.open && !a.sync.is_scanning());
    let actions: Vec<(String, crate::sync::Action)> =
        h.state().sync.items.iter().map(|i| (i.rel.clone(), i.action)).collect();
    use crate::sync::Action::*;
    assert!(actions.contains(&("fotos/2024/bild.jpg".into(), ToRight)), "{actions:?}");
    assert!(actions.contains(&("beide.txt".into(), ToRight)));
    assert!(actions.contains(&("nur_rechts.txt".into(), ToLeft)));
    assert!(actions.contains(&("leerer_ordner".into(), ToLeft)));

    h.get_by_label("▶ Synchronisieren").click();
    steps(&mut h, 2);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(r.path().join("fotos/2024/bild.jpg")).unwrap(), "neues bild");
    assert_eq!(fs::read_to_string(r.path().join("beide.txt")).unwrap(), "neu");
    assert_eq!(fs::read_to_string(l.path().join("nur_rechts.txt")).unwrap(), "R");
    assert!(l.path().join("leerer_ordner").is_dir());

    // The window rescans automatically: now everything is equal.
    wait_until(&mut h, "rescan", |a| !a.sync.is_scanning());
    assert!(h.state().sync.items.iter().all(|i| i.action == None), "{:?}", h.state().sync.items);
}


/// A zip with loose files ("a.txt", "b.txt") or one folder ("ordner/…").
fn make_zip(path: &Path, members: &[(&str, &str)]) {
    use std::io::Write as _;
    let mut zw = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, content) in members {
        zw.start_file(name.to_string(), zip::write::SimpleFileOptions::default()).unwrap();
        zw.write_all(content.as_bytes()).unwrap();
    }
    zw.finish().unwrap();
}

#[test]
fn unpack_here_smart_and_dialog() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    make_zip(&l.path().join("urlaub.zip"), &[("a.jpg", "A"), ("b.jpg", "B")]);
    make_zip(&l.path().join("projekt.zip"), &[("projekt/main.rs", "M"), ("projekt/lib.rs", "L")]);
    let mut h = harness();
    setup(&mut h, l.path(), r.path());

    // Smart, loose files → own folder "urlaub/".
    select(&mut h, "urlaub.zip");
    panel_key(&mut h, Key::F9, Modifiers::ALT | Modifiers::SHIFT);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(l.path().join("urlaub/a.jpg")).unwrap(), "A");
    assert!(!l.path().join("a.jpg").exists(), "no loose files in the folder");

    // Again: the folder exists → "urlaub (2)".
    panel_key(&mut h, Key::F9, Modifiers::ALT | Modifiers::SHIFT);
    wait_for_job(&mut h);
    assert!(l.path().join("urlaub (2)/b.jpg").is_file());

    // Smart, one top folder → unpacked directly (no "projekt/projekt").
    select(&mut h, "projekt.zip");
    panel_key(&mut h, Key::F9, Modifiers::ALT | Modifiers::SHIFT);
    wait_for_job(&mut h);
    assert_eq!(fs::read_to_string(l.path().join("projekt/main.rs")).unwrap(), "M");
    assert!(!l.path().join("projekt/projekt").exists());

    // "Hier entpacken": loose files right here.
    select(&mut h, "urlaub.zip");
    let ctx = h.ctx.clone();
    h.state_mut().run(crate::app::Cmd::UnpackHere, &ctx);
    wait_for_job(&mut h);
    assert!(l.path().join("a.jpg").is_file() && l.path().join("b.jpg").is_file());

    // Alt+F9 dialog: two marked archives, smart, into the other panel.
    h.state_mut().active().tab_mut().marked.extend(["urlaub.zip".to_string(), "projekt.zip".to_string()]);
    panel_key(&mut h, Key::F9, Modifiers::ALT);
    assert!(matches!(&h.state().dialog, Some(Dialog::Unpack { archives, smart: true, .. }) if archives.len() == 2));
    dialog_input(&mut h, None);
    wait_for_job(&mut h);
    assert!(r.path().join("urlaub/a.jpg").is_file());
    assert!(r.path().join("projekt/lib.rs").is_file());
}


/// Screenshots for the website (docs/img). Run with
///   MIERY_SCREENSHOTS=docs/img cargo test website_screenshots -- --nocapture
#[test]
fn website_screenshots() {
    use egui_kittest::kittest::Queryable;
    let Ok(out) = std::env::var("MIERY_SCREENSHOTS") else { return };
    let out = std::path::PathBuf::from(out);
    fs::create_dir_all(&out).unwrap();
    let demo = std::path::PathBuf::from("/tmp/MieryCommander-Demo");
    let _ = fs::remove_dir_all(&demo);
    let day = 86400;
    let mk = |rel: &str, size: usize, days_ago: u64| {
        let p = demo.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![b'x'; size]).unwrap();
        let t = std::time::SystemTime::now() - Duration::from_secs(days_ago * day + size as u64 % 3000);
        fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
    };
    for (rel, size, age) in [
        ("Dokumente/Bericht 2026.docx", 48_211, 2),
        ("Dokumente/Notizen.md", 3_412, 0),
        ("Dokumente/Rechnung-0815.pdf", 182_334, 14),
        ("Dokumente/Präsentation.pptx", 2_406_118, 30),
        ("Dokumente/日本語のメモ.txt", 1_024, 5),
        ("Fotos/Urlaub 2024/Strand.jpg", 3_211_004, 90),
        ("Fotos/Urlaub 2024/Berge.jpg", 2_801_337, 91),
        ("Fotos/Familie/Geburtstag.jpg", 1_904_221, 200),
        ("Musik/Lieblingssong.mp3", 5_402_118, 400),
        ("Videos/Drohnenflug.mp4", 48_211_552, 60),
        ("Projekte/miery/src/main.rs", 1_280, 1),
        ("Projekte/miery/Cargo.toml", 512, 1),
        ("Projekte/miery/README.md", 4_096, 3),
        ("Backup/Dokumente/Bericht 2026.docx", 47_903, 20),
        ("Backup/Dokumente/Notizen.md", 3_001, 40),
        ("Backup/Fotos/Strand.jpg", 3_211_004, 90),
    ] {
        mk(rel, size, age);
    }
    make_zip(&demo.join("Archiv.zip"), &[("Fotos/a.jpg", "A"), ("Fotos/b.jpg", "B"), ("liesmich.txt", "Hallo")]);
    fs::write(demo.join("Dokumente/Notizen.md"), "# Notizen\n\n- Einkaufen\n- MieryCommander testen\n- NAS aufräumen\n").unwrap();
    fs::write(demo.join("Backup/Dokumente/Notizen.md"), "# Notizen\n\n- Einkaufen\n- NAS aufräumen\n- Steuer\n").unwrap();

    // Made-up drives instead of the real (private) mounts of this machine.
    {
        use crate::fsutil::{Place, PlaceKind};
        let p = |label: &str, path: std::path::PathBuf, kind| Place { label: label.into(), path, kind };
        *crate::fsutil::PLACES_OVERRIDE.lock().unwrap() = Some(vec![
            p("/", "/".into(), PlaceKind::Root),
            p("Home", demo.clone(), PlaceKind::Home),
            p("USB-Stick", demo.join("Musik"), PlaceKind::Removable),
            p("nas.local/Fotos (smb)", demo.join("Fotos"), PlaceKind::Network),
            p("nas.local/Backup (smb)", demo.join("Backup"), PlaceKind::Network),
        ]);
    }
    let shot = |h: &mut Harness<'_, MieryApp>, name: &str| {
        for _ in 0..3 {
            h.step();
        }
        let name = if crate::i18n::en() { name.replace(".png", "-en.png") } else { name.to_string() };
        h.render().unwrap().save(out.join(&name)).unwrap();
        eprintln!("saved {name}");
    };
    let new = |english: bool| {
        let mut h = Harness::builder().with_size([1280.0, 760.0]).build_eframe(|cc| MieryApp::new(cc));
        h.state_mut().cfg.language =
            if english { crate::i18n::LangChoice::English } else { crate::i18n::LangChoice::German };
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(40)); // fonts + drive scan in the background
            h.step();
        }
        h
    };

    for english in [false, true] {
        // 1. Main window: Dokumente | Backup/Dokumente, some marked.
        let mut h = new(english);
        setup(&mut h, &demo.join("Dokumente"), &demo);
        h.state_mut().active().tab_mut().marked.extend(["Rechnung-0815.pdf".to_string(), "Präsentation.pptx".to_string()]);
        select(&mut h, "Bericht 2026.docx");
        shot(&mut h, "main.png");

        // 2. Context menu on a file.
        h.get_by_label_contains("Bericht 2026").click_secondary();
        shot(&mut h, "context-menu.png");
        h.key_press(Key::Escape);
        steps(&mut h, 2);

        // 3. Compare two versions of a file.
        h.state_mut().right.tab_mut().navigate(Location::Dir(demo.join("Backup/Dokumente")), false, true);
        steps(&mut h, 2);
        select(&mut h, "Notizen.md");
        h.state_mut().right.tab_mut().select_name("Notizen.md");
        h.state_mut().active().tab_mut().marked.clear();
        let ctx = h.ctx.clone();
        h.state_mut().run(crate::app::Cmd::CompareFiles, &ctx);
        wait_until(&mut h, "compare", |a| a.compares.last().is_some_and(|c| c.outcome().is_some()));
        shot(&mut h, "compare.png");
        h.state_mut().compares.clear();

        // 4. Synchronize Dokumente ↔ Backup/Dokumente.
        h.state_mut().run(crate::app::Cmd::SyncDirs, &ctx);
        wait_until(&mut h, "sync", |a| !a.sync.is_scanning());
        shot(&mut h, "sync.png");
        h.state_mut().sync.open = false;

        // 5. Inside an archive + quick view.
        let mut h = new(english);
        setup(&mut h, &demo, &demo.join("Fotos/Urlaub 2024"));
        select(&mut h, "Archiv.zip");
        panel_key(&mut h, Key::Enter, Modifiers::NONE);
        shot(&mut h, "archive.png");

        // 6. Connect dialog (SFTP).
        panel_key(&mut h, Key::F, Modifiers::COMMAND);
        if let Some(Dialog::FtpConnect(f)) = &mut h.state_mut().dialog {
            f.site = Site { protocol: Protocol::Sftp, name: "Mein NAS".into(), host: "nas.local".into(), port: 22, user: "anna".into(), remote_dir: "/mnt/tank".into(), ..Default::default() };
        }
        shot(&mut h, "connect.png");
    }
    *crate::fsutil::PLACES_OVERRIDE.lock().unwrap() = None;
    let _ = fs::remove_dir_all(&demo);
}

#[test]
fn english_user_interface() {
    use egui_kittest::kittest::Queryable;
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(l.path().join("report.txt"), vec![b'x'; 1_234_567]).unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());
    // Switch the language in the settings like a user would.
    panel_key(&mut h, Key::Comma, Modifiers::COMMAND);
    h.get_by_label("English").click();
    steps(&mut h, 2);
    assert_eq!(h.state().cfg.language, crate::i18n::LangChoice::English);
    h.key_press(Key::Escape);
    steps(&mut h, 3);

    for label in ["Files", "Commands", "F5 Copy", "F8 Delete", "Exit", "Size", "Date"] {
        assert!(h.query_all_by_label(label).next().is_some(), "English label {label:?} missing");
    }
    assert!(h.query_all_by_label("F5 Kopieren").next().is_none(), "German text left over");
    // English number and date formats.
    assert!(h.query_all_by_label("1,234,567").next().is_some(), "thousands separator");
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert!(h.query_all_by_label_contains(&today).next().is_some(), "ISO date");
    // Context menu with English shortcuts.
    h.get_by_label_contains("report").click_secondary();
    steps(&mut h, 2);
    assert!(h.query_by_label("Copy Ctrl+C").is_some());
    assert!(h.query_by_label("Move to trash F8").is_some());
    if let Ok(out) = std::env::var("MIERY_SHOT") {
        h.render().unwrap().save(out).unwrap();
    }
}

/// Drag a file with the mouse from one panel to the other, and onto a folder.
#[test]
fn drag_and_drop_between_panels() {
    use egui_kittest::kittest::Queryable;
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(l.path().join("bild.png"), "x").unwrap();
    fs::create_dir(l.path().join("Ziel")).unwrap();
    let mut h = harness();
    setup(&mut h, l.path(), r.path());

    let drag = |h: &mut Harness<'_, MieryApp>, from: eframe::egui::Pos2, to: eframe::egui::Pos2, shift: bool| {
        let mods = if shift { Modifiers::SHIFT } else { Modifiers::NONE };
        let button = |pos, pressed| Event::PointerButton { pos, button: eframe::egui::PointerButton::Primary, pressed, modifiers: mods };
        h.event(Event::ModifiersChanged(mods));
        h.event(Event::PointerMoved(from));
        steps(h, 1);
        h.event(button(from, true));
        steps(h, 1);
        for k in 1..=8 {
            h.event(Event::PointerMoved(from + (to - from) * (k as f32 / 8.0)));
            steps(h, 1);
        }
        h.event(button(to, false));
        steps(h, 2);
        h.event(Event::ModifiersChanged(Modifiers::NONE));
        steps(h, 1);
    };

    // File → empty space of the right panel: copy dialog into the right folder.
    let from = h.get_by_label_contains("bild").rect().center();
    let to = eframe::egui::pos2(900.0, 500.0);
    drag(&mut h, from, to, false);
    match &h.state().dialog {
        Some(Dialog::CopyMove { is_move, sources, target, .. }) => {
            assert!(!is_move);
            assert_eq!(sources, &[l.path().join("bild.png")]);
            assert_eq!(target.trim_end_matches('/'), r.path().to_string_lossy());
        }
        _ => panic!("drop on the other panel should open the copy dialog"),
    }
    h.state_mut().dialog = None;
    steps(&mut h, 2);

    // With Shift onto the folder "Ziel" in the same panel: move into it.
    let from = h.get_by_label_contains("bild").rect().center();
    let to = h.get_by_label_contains("Ziel").rect().center();
    drag(&mut h, from, to, true);
    match &h.state().dialog {
        Some(Dialog::CopyMove { is_move, target, .. }) => {
            assert!(is_move, "Shift moves");
            assert_eq!(target.trim_end_matches('/'), l.path().join("Ziel").to_string_lossy());
        }
        _ => panic!("drop on a folder should open the move dialog"),
    }
}

#[test]
fn font_size_steps_and_auto_scale() {
    use crate::app::{auto_scale, step_font};
    assert_eq!(step_font(1.0, 1), 1.1);
    assert_eq!(step_font(1.0, -1), 0.9);
    assert_eq!(step_font(2.5, 1), 2.5);
    assert_eq!(step_font(0.6, -1), 0.6);
    // Full HD and laptops stay at 100 %, 4K at 100 % system scaling grows.
    assert_eq!(auto_scale(eframe::egui::vec2(1920.0, 1080.0)), 1.0);
    assert_eq!(auto_scale(eframe::egui::vec2(1512.0, 982.0)), 1.0);
    assert_eq!(auto_scale(eframe::egui::vec2(3840.0, 2160.0)), 1.75);
    assert_eq!(auto_scale(eframe::egui::vec2(3440.0, 1440.0)), 1.15); // ultrawide: by height
}

/// The update dialog and the "Open with" dialog fit and show their buttons.
#[test]
fn update_and_open_with_dialogs() {
    use egui_kittest::kittest::Queryable;
    let release: crate::update::Release = serde_json::from_str(
        r###"{"tag_name":"v9.0.0","html_url":"https://example.invalid/r","body":"## Deutsch\n\n- Neu: vieles\n\n## English\n\n- New: lots",
            "assets":[{"name":"MieryCommander-9.0.0-x86_64.AppImage","browser_download_url":"https://example.invalid/a","size":15000000}]}"###,
    )
    .unwrap();
    let shot = std::env::var("MIERY_DIALOG_SHOTS").ok();
    let mut h = harness();
    steps(&mut h, 2);
    for (i, kind) in [crate::update::Install::AppImage("/x/M.AppImage".into()), crate::update::Install::Manual].into_iter().enumerate() {
        h.state_mut().open_dialog(Dialog::Update { release: release.clone(), kind, state: crate::dialogs::UpdateState::Idle });
        steps(&mut h, 3);
        assert!(h.query_by_label_contains("Neu: vieles").is_some());
        assert!(h.query_by_label("Später").is_some());
        if let Some(dir) = &shot {
            h.render().unwrap().save(format!("{dir}/update-{i}.png")).unwrap();
        }
    }
    assert!(h.query_by_label_contains("git pull").is_some(), "self-built: hint instead of install");
    h.get_by_label("Diese Version überspringen").click();
    steps(&mut h, 2);
    assert!(h.state().dialog.is_none());
    assert_eq!(h.state().cfg.skipped_version, "9.0.0");

    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("notiz.txt");
    fs::write(&f, "x").unwrap();
    let app = |n: &str| crate::openwith::App { name: n.into(), exec: format!("{} %F", n.to_lowercase()), default: n == "Kate" };
    h.state_mut().open_dialog(Dialog::OpenWith {
        files: vec![f],
        filter: String::new(),
        suggested: vec![app("Kate"), app("KWrite")],
        all: vec![app("Firefox"), app("Gwenview"), app("Kate"), app("KWrite")],
        command: String::new(),
    });
    steps(&mut h, 3);
    if let Some(dir) = &shot {
        h.render().unwrap().save(format!("{dir}/openwith.png")).unwrap();
    }
    assert!(h.query_by_label("Kate  ★").is_some());
    assert!(h.query_by_label("Firefox").is_some());
}

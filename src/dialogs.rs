use crate::app::MieryApp;
use crate::config::ThemeChoice;
use crate::fsutil::{self, Entry};
use crate::remote::{self, Protocol, Security, Site};
use crate::ops::{JobKind, OverwriteAnswer};
use crate::panel::{Location, expand_tilde};
use eframe::egui::{self, Key, RichText};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

pub enum CopyMode {
    Local,
    /// Copying out of a zip: archive file and the dir inside it.
    Extract { archive: PathBuf, inner: String },
    Upload { conn: usize },
    /// `dirs`: which of the sources are directories.
    Download { conn: usize, dirs: Vec<PathBuf> },
}

/// State of the FTP connect dialog.
pub struct FtpForm {
    pub selected: Option<usize>,
    pub site: Site,
    pub password: String,
    /// (is_error, message)
    pub status: Option<(bool, String)>,
    /// SSH: fingerprint of an unknown host key waiting for confirmation.
    pub host_key: Option<String>,
}

impl FtpForm {
    pub fn new(sites: &[Site]) -> Self {
        let mut f = FtpForm::empty();
        if !sites.is_empty() {
            f.select(0, sites);
        }
        f
    }

    fn empty() -> Self {
        FtpForm { selected: None, site: Site::default(), password: String::new(), status: None, host_key: None }
    }

    fn select(&mut self, i: usize, sites: &[Site]) {
        self.host_key = None;
        self.selected = Some(i);
        self.site = sites[i].clone();
        self.password = if self.site.save_password { self.site.load_password().unwrap_or_default() } else { String::new() };
        self.status = None;
    }
}

pub enum Dialog {
    CopyMove {
        is_move: bool,
        sources: Vec<PathBuf>,
        target: String,
        mode: CopyMode,
    },
    FtpConnect(FtpForm),
    /// Alt+F9: unpack whole archives.
    Unpack { archives: Vec<PathBuf>, target: String, smart: bool },
    FtpReupload { local: PathBuf, conn: usize, remote: String },
    Mkdir { name: String },
    NewFile { name: String },
    Rename { from: PathBuf, name: String, ftp: Option<usize> },
    Delete { paths: Vec<PathBuf>, permanent: bool, ftp: Option<(usize, Vec<PathBuf>)> },
    Pack { sources: Vec<PathBuf>, target: String },
    Pattern { select: bool, mask: String },
    Hotlist,
    History,
    Settings,
    Keys,
    Message { title: String, text: String },
    /// Like `Message`, but a folder's size is still being counted.
    Properties {
        title: String,
        text: String,
        pending: Option<(std::sync::mpsc::Receiver<(u64, u64)>, std::time::Instant)>,
    },
}

impl Dialog {
    /// Changes whenever the dialog switches to a differently sized view, so
    /// egui measures it afresh instead of keeping the old height.
    fn layout_key(&self) -> u8 {
        match self {
            Dialog::FtpConnect(f) => {
                (f.host_key.is_some() as u8) << 2 | f.site.protocol as u8
            }
            Dialog::Properties { pending, .. } => pending.is_some() as u8,
            _ => 0,
        }
    }

    pub fn properties(e: &Entry, on_disk: bool) -> Dialog {
        let mut text = lf!("Pfad:  {}\n", "Path:  {}\n", e.path.to_string_lossy());
        text.push_str(&lf!("Typ:  {}\n", "Type:  {}\n",
            if e.is_dir {
                l!("Ordner", "Folder")
            } else if e.is_link {
                l!("Symbolischer Link", "Symbolic link")
            } else {
                l!("Datei", "File")
            }
        ));
        if on_disk && e.is_link
            && let Ok(t) = std::fs::read_link(&e.path)
        {
            text.push_str(&lf!("Ziel:  {}\n", "Target:  {}\n", t.to_string_lossy()));
        }
        text.push_str(&lf!("Geändert:  {}\n", "Modified:  {}\n", fsutil::format_time(e.modified)));
        text.push_str(&lf!("Rechte:  {} ({:o})\n", "Permissions:  {} ({:o})\n", fsutil::format_mode(e.mode), e.mode & 0o7777));
        // Folder sizes can take long (big trees, network drives): compute in the background.
        let pending = if on_disk && e.is_dir {
            let (tx, rx) = std::sync::mpsc::channel();
            let path = e.path.clone();
            std::thread::spawn(move || {
                let _ = tx.send(crate::ops::count(std::slice::from_ref(&path)));
                fsutil::wake_ui();
            });
            Some((rx, std::time::Instant::now()))
        } else {
            text.push_str(&lf!("Größe:  {} Bytes ({})\n", "Size:  {} bytes ({})\n",
                fsutil::format_size(e.size),
                fsutil::format_size_short(e.size)
            ));
            None
        };
        Dialog::Properties { title: lf!("Eigenschaften – {}", "Properties – {}", e.name), text, pending }
    }
}

enum Outcome {
    None,
    Close,
}

/// A labelled single-line text field that grabs focus when the dialog opens.
fn focused_field(ui: &mut egui::Ui, value: &mut String, fresh: bool) -> egui::Response {
    let r = ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
    if fresh {
        r.request_focus();
    }
    r
}

fn ok_cancel(ui: &mut egui::Ui, ok_label: &str) -> (bool, bool) {
    let mut ok = false;
    let mut cancel = false;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button(RichText::new(ok_label).strong()).clicked() {
            ok = true;
        }
        if ui.button(l!("Abbrechen", "Cancel")).clicked() {
            cancel = true;
        }
    });
    ok |= ui.input(|i| i.key_pressed(Key::Enter));
    (ok, cancel)
}

/// "a/b.tar.gz" + ".7z" → "a/b.7z"
fn with_archive_ext(target: &str, ext: &str) -> String {
    let lower = target.to_lowercase();
    let known = [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tgz", ".txz", ".tar", ".zip", ".7z"];
    let stem_len = known
        .iter()
        .find(|k| lower.ends_with(*k))
        .map(|k| target.len() - k.len())
        .unwrap_or(target.len());
    format!("{}{ext}", &target[..stem_len])
}

fn resolve(base: &Path, input: &str) -> PathBuf {
    let p = PathBuf::from(expand_tilde(input.trim()));
    if p.is_absolute() { p } else { base.join(p) }
}

impl MieryApp {
    pub fn show_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let fresh = std::mem::take(&mut self.dialog_fresh);
        let base_dir = self.active_dir();
        let remote_dir = self.active_ref().tab().loc.ftp().map(|(id, p)| (id, p.to_string()));
        let mut outcome = Outcome::None;

        // Small screens: dialogs scroll instead of running off the window.
        let max_h = (ctx.content_rect().height() - 80.0).max(200.0);
        // Each kind of dialog gets its own id: egui remembers window and scroll
        // sizes per id, and a small dialog must not shrink the next, bigger one.
        let kind_id = egui::Id::new(("dialog", std::mem::discriminant(&dialog), dialog.layout_key()));
        let modal = egui::Modal::new(kind_id).show(ctx, |ui| {
            ui.set_min_width(460.0);
            egui::ScrollArea::vertical()
                .id_salt(kind_id)
                .max_height(max_h)
                .show(ui, |ui| {
            match &mut dialog {
                Dialog::CopyMove { is_move, sources, target, mode } => {
                    let verb = match (&*mode, *is_move) {
                        (CopyMode::Extract { .. }, _) => l!("Entpacken", "Unpack"),
                        (CopyMode::Upload { .. }, false) => l!("Hochladen", "Upload"),
                        (CopyMode::Upload { .. }, true) => l!("Hochladen & lokal löschen", "Upload & delete locally"),
                        (CopyMode::Download { .. }, false) => l!("Herunterladen", "Download"),
                        (CopyMode::Download { .. }, true) => l!("Herunterladen & vom Server löschen", "Download & delete on server"),
                        (CopyMode::Local, true) => l!("Verschieben", "Move"),
                        (CopyMode::Local, false) => l!("Kopieren", "Copy"),
                    };
                    ui.heading(verb);
                    let what = if matches!(mode, CopyMode::Extract { .. }) && sources.len() == 1 && sources[0].as_os_str().is_empty() {
                        l!("gesamtes Archiv", "the whole archive").to_string()
                    } else if sources.len() == 1 {
                        format!("„{}“", sources[0].file_name().unwrap_or_default().to_string_lossy())
                    } else {
                        lf!("{} Elemente", "{} items", sources.len())
                    };
                    let where_ = match &*mode {
                        CopyMode::Upload { conn } => remote::get(*conn).map(|c| lf!(" auf {}", " on {}", c.url())).unwrap_or_default(),
                        _ => String::new(),
                    };
                    ui.label(lf!("{what} nach{where_}:", "{what} to{where_}:"));
                    let r = focused_field(ui, target, fresh);
                    let (ok, cancel) = ok_cancel(ui, verb);
                    if cancel {
                        outcome = Outcome::Close;
                    } else if ok && (r.lost_focus() || !r.has_focus()) {
                        let srcs = std::mem::take(sources);
                        let (kind, dest) = match mode {
                            CopyMode::Local => {
                                (if *is_move { JobKind::Move } else { JobKind::Copy }, resolve(&base_dir, target))
                            }
                            CopyMode::Extract { archive, inner } => {
                                let dest = resolve(&base_dir, target);
                                let _ = std::fs::create_dir_all(&dest);
                                (JobKind::Extract { archive: archive.clone(), base: inner.clone() }, dest)
                            }
                            CopyMode::Upload { conn } => (
                                JobKind::Upload { conn: *conn, delete_source: *is_move, overwrite: false },
                                PathBuf::from(target.trim()),
                            ),
                            CopyMode::Download { conn, dirs } => (
                                JobKind::Download { conn: *conn, dirs: std::mem::take(dirs), delete_source: *is_move },
                                resolve(&base_dir, target),
                            ),
                        };
                        self.start_job(kind, srcs, dest, ctx);
                        outcome = Outcome::Close;
                    }
                }
                Dialog::FtpConnect(form) => {
                    outcome = self.ftp_connect_ui(ui, form, fresh);
                }
                Dialog::Unpack { archives, target, smart } => {
                    ui.heading(l!("Entpacken", "Unpack"));
                    let what = if archives.len() == 1 {
                        format!("„{}“", archives[0].file_name().unwrap_or_default().to_string_lossy())
                    } else {
                        lf!("{} Archive", "{} archives", archives.len())
                    };
                    ui.label(lf!("{what} entpacken nach:", "Unpack {what} to:"));
                    let r = focused_field(ui, target, fresh);
                    ui.horizontal(|ui| {
                        if ui.small_button(l!("📂 Hierher (aktueller Ordner)", "📂 Here (current folder)")).clicked() {
                            *target = base_dir.to_string_lossy().into_owned();
                        }
                    });
                    ui.checkbox(smart, l!("Smart: eigenen Ordner anlegen, wenn das Archiv mehrere Dateien direkt enthält", "Smart: create a folder if the archive contains several files directly"));
                    if *smart && archives.len() == 1 {
                        ui.label(
                            RichText::new(lf!("z. B. „{}/“ – liegt alles in einem Ordner, wird direkt entpackt", "e.g. “{}/” – if everything is in one folder, it is unpacked directly", crate::archive::stem(&archives[0])))
                                .small()
                                .weak(),
                        );
                    }
                    let (ok, cancel) = ok_cancel(ui, l!("Entpacken", "Unpack"));
                    if cancel {
                        outcome = Outcome::Close;
                    } else if ok && (r.lost_focus() || !r.has_focus()) {
                        let dest = resolve(&base_dir, target);
                        let _ = std::fs::create_dir_all(&dest);
                        self.start_job(JobKind::Unpack { smart: *smart }, std::mem::take(archives), dest, ctx);
                        outcome = Outcome::Close;
                    }
                }
                Dialog::FtpReupload { local, conn, remote } => {
                    ui.heading(l!("Datei geändert", "File changed"));
                    ui.label(lf!("„{}“ wurde im Editor gespeichert.\nWieder auf den Server hochladen?", "“{}” was saved in the editor.\nUpload it to the server again?",
                        remote::file_name(remote)
                    ));
                    let (ok, cancel) = ok_cancel(ui, l!("Hochladen", "Upload"));
                    if cancel {
                        outcome = Outcome::Close;
                    } else if ok {
                        let dir = remote::parent(remote).unwrap_or_else(|| "/".into());
                        let kind = JobKind::Upload { conn: *conn, delete_source: false, overwrite: true };
                        self.start_job(kind, vec![local.clone()], PathBuf::from(dir), ctx);
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Mkdir { name } | Dialog::NewFile { name } => {
                    ui.heading("Name:");
                    let r = focused_field(ui, name, fresh);
                    let (ok, cancel) = ok_cancel(ui, "OK");
                    if cancel {
                        outcome = Outcome::Close;
                    } else if ok && !name.trim().is_empty() && (r.lost_focus() || !r.has_focus()) {
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Rename { from, name, ftp: Some(id) } => {
                    ui.heading(l!("Umbenennen (FTP)", "Rename (FTP)"));
                    ui.label(from.to_string_lossy());
                    let r = focused_field(ui, name, fresh);
                    let (ok, cancel) = ok_cancel(ui, l!("Umbenennen", "Rename"));
                    if cancel {
                        outcome = Outcome::Close;
                    } else if ok && (r.lost_focus() || !r.has_focus()) {
                        let from = from.to_string_lossy().into_owned();
                        let to = remote::join(&remote::parent(&from).unwrap_or_else(|| "/".into()), name.trim());
                        let id = *id;
                        if to != from {
                            self.spawn_ftp_op(move || {
                                let c = remote::get(id).ok_or(l!("Verbindung zum Server ist getrennt", "The connection to the server is closed"))?;
                                c.rename(&from, &to).map(|_| l!("Umbenannt", "Renamed").to_string())
                            });
                        }
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Rename { from, name, ftp: None } => {
                    ui.heading(l!("Umbenennen", "Rename"));
                    ui.label(from.to_string_lossy());
                    let r = focused_field(ui, name, fresh);
                    let (ok, cancel) = ok_cancel(ui, l!("Umbenennen", "Rename"));
                    if cancel {
                        outcome = Outcome::Close;
                    } else if ok && (r.lost_focus() || !r.has_focus()) {
                        let target = from.with_file_name(name.trim());
                        if target != *from {
                            if target.exists() {
                                self.notify(lf!("{} existiert bereits", "{} already exists", target.to_string_lossy()), true);
                            } else if let Err(e) = std::fs::rename(&*from, &target) {
                                self.notify(lf!("Umbenennen: {e}", "Rename: {e}"), true);
                            } else {
                                self.reload_all();
                                let n = name.trim().to_string();
                                self.active().tab_mut().select_name(&n);
                            }
                        }
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Delete { paths, permanent, ftp } => {
                    ui.heading(if ftp.is_some() {
                        l!("Vom Server löschen", "Delete from server")
                    } else if *permanent {
                        l!("Endgültig löschen", "Delete permanently")
                    } else {
                        l!("In den Papierkorb", "Move to trash")
                    });
                    if paths.len() == 1 {
                        ui.label(lf!("„{}“ löschen?", "Delete “{}”?", paths[0].to_string_lossy()));
                    } else {
                        ui.label(lf!("{} Elemente löschen?", "Delete {} items?", paths.len()));
                    }
                    if *permanent {
                        ui.colored_label(egui::Color32::from_rgb(200, 60, 60), l!("Dies kann nicht rückgängig gemacht werden!", "This cannot be undone!"));
                    }
                    let (ok, cancel) = ok_cancel(ui, l!("Löschen", "Delete"));
                    if cancel {
                        outcome = Outcome::Close;
                    } else if ok {
                        let p = std::mem::take(paths);
                        match ftp.take() {
                            Some((conn, dirs)) => self.start_job(JobKind::FtpDelete { conn, dirs }, p, PathBuf::new(), ctx),
                            None => self.start_job(JobKind::Delete { to_trash: !*permanent }, p, PathBuf::new(), ctx),
                        }
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Pack { sources, target } => {
                    ui.heading(l!("Packen", "Pack"));
                    ui.label(lf!("{} Element(e) packen nach:", "Pack {} item(s) to:", sources.len()));
                    let r = focused_field(ui, target, fresh);
                    // Quick format switch: replace the archive extension.
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Format:").small());
                        for ext in [".zip", ".7z", ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar"] {
                            let current = target.to_lowercase().ends_with(ext)
                                && !(ext == ".tar" && target.to_lowercase().contains(".tar."));
                            if ui.selectable_label(current, RichText::new(ext).small()).clicked() {
                                *target = with_archive_ext(target, ext);
                            }
                        }
                    });
                    let supported = crate::archive::can_create(Path::new(target.trim()));
                    if !supported {
                        ui.colored_label(egui::Color32::from_rgb(200, 60, 60), l!("Unbekannte Endung – bitte ein Format oben wählen", "Unknown extension – please choose a format above"));
                    }
                    let (ok, cancel) = ok_cancel(ui, l!("Packen", "Pack"));
                    if cancel {
                        outcome = Outcome::Close;
                    } else if ok && supported && (r.lost_focus() || !r.has_focus()) {
                        let dest = resolve(&base_dir, target);
                        let s = std::mem::take(sources);
                        self.start_job(JobKind::Pack, s, dest, ctx);
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Pattern { select, mask } => {
                    ui.heading(if *select { l!("Gruppe markieren", "Select group") } else { l!("Gruppe abwählen", "Deselect group") });
                    ui.label(l!("Maske (z. B. *.jpg;*.png):", "Mask (e.g. *.jpg;*.png):"));
                    let r = focused_field(ui, mask, fresh);
                    let (ok, cancel) = ok_cancel(ui, "OK");
                    if cancel {
                        outcome = Outcome::Close;
                    } else if ok && (r.lost_focus() || !r.has_focus()) {
                        let (m, s) = (mask.clone(), *select);
                        self.active().tab_mut().mark_pattern(&m, s);
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Hotlist => {
                    ui.heading(l!("Ordner-Favoriten", "Favourite folders"));
                    let mut go = None;
                    let mut remove = None;
                    if self.cfg.hotlist.is_empty() {
                        ui.label(RichText::new(l!("Noch keine Favoriten.", "No favourites yet.")).weak());
                    }
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (i, p) in self.cfg.hotlist.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.small_button("✖").on_hover_text(l!("Entfernen", "Remove")).clicked() {
                                    remove = Some(i);
                                }
                                let label = if i < 9 { format!("{}  {}", i + 1, p.to_string_lossy()) } else { p.to_string_lossy().into_owned() };
                                if ui.selectable_label(false, label).clicked() {
                                    go = Some(p.clone());
                                }
                            });
                        }
                    });
                    // Number keys jump directly.
                    for (n, k) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9].iter().enumerate() {
                        if ui.input(|i| i.key_pressed(*k)) && let Some(p) = self.cfg.hotlist.get(n) {
                            go = Some(p.clone());
                        }
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button(l!("➕ Aktuellen Ordner hinzufügen", "➕ Add current folder")).clicked() && !self.cfg.hotlist.contains(&base_dir) {
                            self.cfg.hotlist.push(base_dir.clone());
                        }
                        if ui.button(l!("Schließen", "Close")).clicked() {
                            outcome = Outcome::Close;
                        }
                    });
                    if let Some(i) = remove {
                        self.cfg.hotlist.remove(i);
                    }
                    if let Some(p) = go {
                        self.navigate_active(Location::Dir(p));
                        outcome = Outcome::Close;
                    }
                }
                Dialog::History => {
                    ui.heading(l!("Verlauf", "History"));
                    let tab = self.active_ref().tab();
                    let mut items: Vec<Location> = tab.back.iter().rev().cloned().collect();
                    items.dedup();
                    let mut go = None;
                    if items.is_empty() {
                        ui.label(RichText::new(l!("Leer", "Empty")).weak());
                    }
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        for loc in items {
                            if ui.selectable_label(false, loc.display()).clicked() {
                                go = Some(loc);
                            }
                        }
                    });
                    if ui.button(l!("Schließen", "Close")).clicked() {
                        outcome = Outcome::Close;
                    }
                    if let Some(loc) = go {
                        self.navigate_active(loc);
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Settings => {
                    ui.heading(l!("Einstellungen", "Settings"));
                    let mut reload = false;
                    reload |= ui.checkbox(&mut self.cfg.show_hidden, l!("Versteckte Dateien anzeigen", "Show hidden files")).changed();
                    reload |= ui.checkbox(&mut self.cfg.dirs_first, l!("Ordner zuerst", "Folders first")).changed();
                    ui.checkbox(&mut self.cfg.delete_to_trash, l!("F8 löscht in den Papierkorb", "F8 moves to the trash"));
                    ui.checkbox(&mut self.cfg.confirm_delete, l!("Löschen bestätigen", "Confirm deletion"));
                    ui.add_space(6.0);
                    egui::Grid::new("settings_grid").num_columns(2).show(ui, |ui| {
                        ui.label("Editor (F4):");
                        ui.add(egui::TextEdit::singleline(&mut self.cfg.editor).hint_text(l!("leer = Standardprogramm, z. B. code {} oder kate", "empty = default application, e.g. code {} or kate")));
                        ui.end_row();
                        ui.label("Terminal (F9):");
                        ui.add(egui::TextEdit::singleline(&mut self.cfg.terminal).hint_text(l!("leer = automatisch", "empty = automatic")));
                        ui.end_row();
                        ui.label(l!("Sprache:", "Language:"));
                        ui.horizontal(|ui| {
                            use crate::i18n::LangChoice;
                            ui.selectable_value(&mut self.cfg.language, LangChoice::System, l!("System", "System"));
                            ui.selectable_value(&mut self.cfg.language, LangChoice::German, "Deutsch");
                            ui.selectable_value(&mut self.cfg.language, LangChoice::English, "English");
                        });
                        ui.end_row();
                        ui.label(l!("Design:", "Theme:"));
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.cfg.theme, ThemeChoice::System, "System");
                            ui.selectable_value(&mut self.cfg.theme, ThemeChoice::Light, l!("Hell", "Light"));
                            ui.selectable_value(&mut self.cfg.theme, ThemeChoice::Dark, l!("Dunkel", "Dark"));
                        });
                        ui.end_row();
                        ui.label(l!("Laufwerke:", "Drives:"));
                        ui.horizontal(|ui| {
                            use crate::config::DriveBar;
                            ui.selectable_value(&mut self.cfg.drive_bar, DriveBar::Both, l!("Knöpfe + Dropdown", "Buttons + dropdown"));
                            ui.selectable_value(&mut self.cfg.drive_bar, DriveBar::Buttons, l!("Nur Knöpfe", "Buttons only"));
                            ui.selectable_value(&mut self.cfg.drive_bar, DriveBar::Dropdown, l!("Nur Dropdown", "Dropdown only"));
                        });
                        ui.end_row();
                        ui.label(l!("Schriftgröße:", "Font size:"));
                        if ui.add(egui::Slider::new(&mut self.cfg.font_scale, 0.7..=2.0).step_by(0.05)).changed() {
                            ctx.set_zoom_factor(self.cfg.font_scale);
                        }
                        ui.end_row();
                    });
                    if reload {
                        self.reload_all();
                    }
                    ui.add_space(6.0);
                    if ui.button(l!("Schließen", "Close")).clicked() {
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Keys => {
                    ui.heading(l!("Tastenkürzel", "Keyboard shortcuts"));
                    egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
                        egui::Grid::new("keys_grid").striped(true).num_columns(2).show(ui, |ui| {
                            for (k, v) in keys_table() {
                                ui.label(RichText::new(crate::i18n::keys(k)).monospace().strong());
                                ui.label(v);
                                ui.end_row();
                            }
                        });
                    });
                    if ui.button(l!("Schließen", "Close")).clicked() {
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Properties { title, text, pending } => {
                    if let Some((rx, _)) = pending
                        && let Ok((files, bytes)) = rx.try_recv()
                    {
                        text.push_str(&lf!("Größe:  {} Bytes ({})\nEnthält:  {files} Datei(en)\n", "Size:  {} bytes ({})\nContains:  {files} file(s)\n",
                            fsutil::format_size(bytes),
                            fsutil::format_size_short(bytes)
                        ));
                        *pending = None;
                    }
                    ui.heading(title.as_str());
                    ui.add(egui::Label::new(RichText::new(text.as_str()).monospace()).wrap());
                    if let Some((_, since)) = pending {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(lf!("Größe wird berechnet… {:.0} s", "Calculating size… {:.0} s", since.elapsed().as_secs_f32().floor()));
                        });
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                    }
                    ui.add_space(6.0);
                    if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        outcome = Outcome::Close;
                    }
                }
                Dialog::Message { title, text } => {
                    ui.heading(title.as_str());
                    egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(text.as_str()).monospace()).wrap());
                    });
                    ui.add_space(6.0);
                    if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        outcome = Outcome::Close;
                    }
                }
            }
            });
        });

        // Mkdir/NewFile are handled here because they need the variant itself.
        if matches!(outcome, Outcome::Close) {
            match &dialog {
                Dialog::Mkdir { name } if !name.trim().is_empty() && !fresh && remote_dir.is_some() => {
                    let (id, dir) = remote_dir.clone().unwrap();
                    let path = if name.trim().starts_with('/') { name.trim().to_string() } else { remote::join(&dir, name.trim()) };
                    self.spawn_ftp_op(move || {
                        let c = remote::get(id).ok_or(l!("Verbindung zum Server ist getrennt", "The connection to the server is closed"))?;
                        c.mkdir_all(&path).map(|_| lf!("Ordner angelegt: {path}", "Folder created: {path}"))
                    });
                }
                Dialog::Mkdir { name } if !name.trim().is_empty() && !fresh => {
                    let p = resolve(&base_dir, name);
                    match std::fs::create_dir_all(&p) {
                        Ok(()) => {
                            self.reload_all();
                            let first = name.trim().split('/').next().unwrap_or("").to_string();
                            self.active().tab_mut().select_name(&first);
                        }
                        Err(e) => self.notify(lf!("Ordner anlegen: {e}", "Create folder: {e}"), true),
                    }
                }
                Dialog::NewFile { name } if !name.trim().is_empty() && !fresh => {
                    let p = resolve(&base_dir, name);
                    let r = std::fs::OpenOptions::new().create(true).append(true).open(&p);
                    match r {
                        Ok(_) => {
                            self.reload_all();
                            let n = name.trim().to_string();
                            self.active().tab_mut().select_name(&n);
                            if let Err(e) = fsutil::open_with(&self.cfg.editor, &p) {
                                self.notify(format!("Editor: {e}"), true);
                            }
                        }
                        Err(e) => self.notify(lf!("Datei anlegen: {e}", "Create file: {e}"), true),
                    }
                }
                _ => {}
            }
        }

        let closed_by_escape = modal.should_close();
        if matches!(outcome, Outcome::None) && !closed_by_escape {
            if self.dialog.is_none() {
                self.dialog = Some(dialog);
            }
        }
    }
}

impl MieryApp {
    fn ftp_connect_ui(&mut self, ui: &mut egui::Ui, form: &mut FtpForm, fresh: bool) -> Outcome {
        let mut outcome = Outcome::None;
        let mut connect = false;
        let mut trust: Option<String> = None;
        let connecting = self.ftp_connecting.is_some() || self.smb_mounting.is_some();
        ui.heading(l!("Verbindung zu Server", "Connect to server"));
        // An unknown SSH host key has to be confirmed before anything else.
        if let Some(fp) = form.host_key.clone() {
            ui.label(RichText::new(lf!("🔒 {}:{} ist unbekannt", "🔒 {}:{} is unknown", form.site.host, form.site.port)).strong());
            ui.label(l!("Mit diesem Server wurde noch nie verbunden. Fingerabdruck seines Host-Schlüssels:", "You have never connected to this server. Fingerprint of its host key:"));
            ui.label(RichText::new(&fp).monospace().strong());
            ui.label(
                RichText::new(l!("Nur vertrauen, wenn er mit dem Fingerabdruck des Servers übereinstimmt (auf dem Server: ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub).", "Only trust it if it matches the server's fingerprint (on the server: ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub)."))
                    .small()
                    .weak(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new(l!("Vertrauen und verbinden", "Trust and connect")).strong()).clicked() {
                    trust = Some(fp.clone());
                }
                if ui.button(l!("Nicht vertrauen", "Don't trust")).clicked() {
                    form.host_key = None;
                    form.status = Some((true, l!("Verbindung abgelehnt: Host-Schlüssel nicht bestätigt", "Connection refused: host key not confirmed").into()));
                }
            });
            if let Some(fp) = trust {
                form.host_key = None;
                form.status = Some((false, lf!("Verbinde mit {}:{}…", "Connecting to {}:{}…", form.site.host, form.site.port)));
                self.start_ftp_connect(form.site.clone(), form.password.clone(), Some(fp));
            }
            return outcome;
        }
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(190.0);
                ui.label(RichText::new(l!("Gespeichert", "Saved")).strong());
                egui::ScrollArea::vertical().id_salt("ftp_sites").max_height(280.0).show(ui, |ui| {
                    let mut pick = None;
                    for (i, site) in self.cfg.sites.iter().enumerate() {
                        let icon = if site.protocol == Protocol::Sftp { "🔒" } else { "🌐" };
                        let r = ui.selectable_label(form.selected == Some(i), format!("{icon} {}", site.label()));
                        if r.clicked() {
                            pick = Some(i);
                        }
                        if r.double_clicked() {
                            connect = true;
                        }
                    }
                    if self.cfg.sites.is_empty() {
                        ui.label(RichText::new(l!("noch keine", "none yet")).weak());
                    }
                    if let Some(i) = pick {
                        form.select(i, &self.cfg.sites);
                    }
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(l!("Neu", "New")).clicked() {
                        *form = FtpForm::empty();
                    }
                    if ui.add_enabled(form.selected.is_some(), egui::Button::new(l!("Löschen", "Delete"))).clicked()
                        && let Some(i) = form.selected.take()
                    {
                        let site = self.cfg.sites.remove(i);
                        site.forget_password();
                    }
                });
            });
            ui.add_space(16.0); // (a separator here would stretch to the full available height)
            ui.vertical(|ui| {
                egui::Grid::new("ftp_form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    let site = &mut form.site;
                    ui.label(l!("Protokoll:", "Protocol:"));
                    let before = site.protocol;
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut site.protocol, Protocol::Sftp, "🔒 SFTP (SSH)");
                        ui.selectable_value(&mut site.protocol, Protocol::Smb, "🖧 SMB (Windows/NAS)");
                        ui.selectable_value(&mut site.protocol, Protocol::Ftp, "🌐 FTP / FTPS");
                    });
                    if before != site.protocol {
                        match site.protocol {
                            Protocol::Sftp if matches!(site.port, 21 | 990) => site.port = 22,
                            Protocol::Ftp if site.port == 22 => site.port = 21,
                            _ => {}
                        }
                        if site.protocol == Protocol::Sftp && site.user == "anonymous" {
                            site.user = std::env::var("USER").unwrap_or_default();
                        }
                        if site.protocol == Protocol::Smb && site.user == "anonymous" {
                            site.user.clear();
                        }
                    }
                    ui.end_row();
                    ui.label("Name:");
                    ui.add(egui::TextEdit::singleline(&mut site.name).hint_text("optional"));
                    ui.end_row();
                    ui.label("Server:");
                    let r = ui.add(egui::TextEdit::singleline(&mut site.host).hint_text("server.example.com"));
                    if fresh && site.host.is_empty() {
                        r.request_focus();
                    }
                    ui.end_row();
                    if site.protocol == Protocol::Smb {
                        // SMB: the desktop handles the connection and the login.
                        ui.label(l!("Freigabe:", "Share:"));
                        ui.add(egui::TextEdit::singleline(&mut site.remote_dir).hint_text(l!("optional, leer = alle Freigaben", "optional, empty = all shares")));
                        ui.end_row();
                        ui.label(l!("Benutzer:", "User:"));
                        ui.add(egui::TextEdit::singleline(&mut site.user).hint_text(l!("optional, auch DOMÄNE\\name", "optional, also DOMAIN\\name")));
                        ui.end_row();
                        ui.label(l!("Passwort:", "Password:"));
                        ui.add(
                            egui::TextEdit::singleline(&mut form.password)
                                .password(true)
                                .hint_text(l!("leer = Anmeldung über das System", "empty = log in via the system")),
                        );
                        ui.end_row();
                        return;
                    }
                    ui.label("Port:");
                    ui.add(egui::DragValue::new(&mut site.port).range(1..=65535));
                    ui.end_row();
                    ui.label(l!("Benutzer:", "User:"));
                    ui.text_edit_singleline(&mut site.user);
                    ui.end_row();
                    ui.label(if site.protocol == Protocol::Sftp { l!("Passwort/Passphrase:", "Password/passphrase:") } else { l!("Passwort:", "Password:") });
                    let r = ui.add(egui::TextEdit::singleline(&mut form.password).password(true));
                    if fresh && !site.host.is_empty() {
                        r.request_focus();
                    }
                    ui.end_row();
                    if site.protocol == Protocol::Ftp {
                        ui.label(l!("Verschlüsselung:", "Encryption:"));
                        let before = site.security;
                        egui::ComboBox::from_id_salt("ftp_sec")
                            .selected_text(match site.security {
                                Security::None => l!("Keine (FTP)", "None (FTP)"),
                                Security::Explicit => l!("FTPS explizit (AUTH TLS)", "FTPS explicit (AUTH TLS)"),
                                Security::Implicit => l!("FTPS implizit", "FTPS implicit"),
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut site.security, Security::None, l!("Keine (FTP)", "None (FTP)"));
                                ui.selectable_value(&mut site.security, Security::Explicit, l!("FTPS explizit (AUTH TLS)", "FTPS explicit (AUTH TLS)"));
                                ui.selectable_value(&mut site.security, Security::Implicit, l!("FTPS implizit", "FTPS implicit"));
                            });
                        if before != site.security {
                            if site.security == Security::Implicit && site.port == 21 {
                                site.port = 990;
                            } else if site.security != Security::Implicit && site.port == 990 {
                                site.port = 21;
                            }
                        }
                        ui.end_row();
                    } else {
                        ui.label(l!("Schlüsseldatei:", "Key file:"));
                        ui.add(egui::TextEdit::singleline(&mut site.key_file).hint_text(l!("optional, z. B. ~/.ssh/id_ed25519", "optional, e.g. ~/.ssh/id_ed25519")));
                        ui.end_row();
                    }
                    ui.label(l!("Startordner:", "Start folder:"));
                    ui.add(egui::TextEdit::singleline(&mut site.remote_dir).hint_text(l!("optional, z. B. /var/www", "optional, e.g. /var/www")));
                    ui.end_row();
                });
                let site = &mut form.site;
                match site.protocol {
                    Protocol::Ftp => {
                        ui.checkbox(&mut site.passive, l!("Passiver Modus (empfohlen)", "Passive mode (recommended)"));
                        if site.security != Security::None {
                            ui.checkbox(&mut site.accept_invalid_certs, l!("Selbstsignierte/ungültige Zertifikate akzeptieren", "Accept self-signed/invalid certificates"));
                            if site.accept_invalid_certs {
                                ui.colored_label(egui::Color32::from_rgb(200, 130, 0), l!("⚠ Server-Identität wird nicht geprüft", "⚠ The server's identity is not verified"));
                            }
                        } else if !site.user.is_empty() && site.user != "anonymous" {
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 130, 0),
                                l!("⚠ Ohne FTPS werden Passwort und Daten unverschlüsselt übertragen", "⚠ Without FTPS, password and data are sent unencrypted"),
                            );
                        }
                    }
                    Protocol::Sftp => {
                        ui.checkbox(&mut site.auto_keys, l!("SSH-Agent und Schlüssel aus ~/.ssh verwenden", "Use the SSH agent and keys from ~/.ssh"));
                    }
                    Protocol::Smb => {
                        ui.label(
                            RichText::new(if cfg!(target_os = "macos") {
                                l!("Wird vom Finder eingebunden. Ohne Passwort fragt macOS selbst nach (Schlüsselbund).", "Mounted by the Finder. Without a password macOS asks itself (keychain).")
                            } else {
                                l!("Wird wie in Dolphin eingebunden (KDE: kio-fuse, GNOME: gio). Ohne Passwort fragt das System selbst nach bzw. nutzt KWallet.", "Mounted like in Dolphin (KDE: kio-fuse, GNOME: gio). Without a password the system asks itself or uses KWallet.")
                            })
                            .small()
                            .weak(),
                        );
                    }
                }
                ui.checkbox(&mut site.save_password, l!("Passwort im Schlüsselbund speichern", "Save password in the keyring"));
            });
        });
        if let Some((is_err, msg)) = &form.status {
            let c = if *is_err { egui::Color32::from_rgb(200, 60, 60) } else { ui.visuals().text_color() };
            ui.add(egui::Label::new(RichText::new(msg).color(c)).wrap());
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if connecting {
                ui.spinner();
                ui.label(l!("Verbinde…", "Connecting…"));
            } else if ui.button(RichText::new(l!("Verbinden", "Connect")).strong()).clicked() {
                connect = true;
            }
            if ui.button(l!("Speichern", "Save")).clicked() {
                let mut site = form.site.clone();
                if site.name.trim().is_empty() {
                    site.name = site.host.clone();
                    form.site.name = site.name.clone();
                }
                match form.selected {
                    Some(i) => self.cfg.sites[i] = site.clone(),
                    None => {
                        self.cfg.sites.push(site.clone());
                        form.selected = Some(self.cfg.sites.len() - 1);
                    }
                }
                form.status = Some(match site.save_password.then(|| site.store_password(&form.password)) {
                    Some(Err(e)) => (true, e),
                    _ => (false, l!("Gespeichert", "Saved").into()),
                });
            }
            if ui.button(l!("Abbrechen", "Cancel")).clicked() {
                outcome = Outcome::Close;
            }
        });
        if !connecting && ui.input(|i| i.key_pressed(Key::Enter)) {
            connect = true;
        }
        if connect && !connecting {
            if form.site.host.trim().is_empty() {
                form.status = Some((true, l!("Bitte einen Server angeben", "Please enter a server").into()));
            } else {
                let site = form.site.clone();
                let mut warn = None;
                if site.protocol == Protocol::Smb && form.password.is_empty() {
                    // no password given: the desktop handles the login
                } else if site.save_password {
                    if let Err(e) = site.store_password(&form.password) {
                        warn = Some(e);
                    }
                } else if form.selected.is_some() {
                    site.forget_password();
                }
                form.status = Some(match warn {
                    Some(w) => (true, lf!("{w} – verbinde trotzdem…", "{w} – connecting anyway…")),
                    None => (false, lf!("Verbinde mit {}:{}…", "Connecting to {}:{}…", site.host, site.port)),
                });
                self.start_ftp_connect(site, form.password.clone(), None);
            }
        }
        outcome
    }
}

pub fn overwrite_modal(ctx: &egui::Context, src: &Path, dst: &Path, tx: &Sender<OverwriteAnswer>) {
    let describe = |p: &Path| {
        let m = std::fs::metadata(p).ok();
        format!(
            "{}\n{} · {}",
            p.to_string_lossy(),
            m.as_ref().map(|m| fsutil::format_size(m.len())).unwrap_or_default(),
            fsutil::format_time(m.and_then(|m| m.modified().ok()))
        )
    };
    egui::Modal::new(egui::Id::new("overwrite")).show(ctx, |ui| {
        ui.set_max_width(560.0);
        ui.heading(l!("Datei existiert bereits", "File already exists"));
        ui.label(RichText::new(l!("Überschreiben:", "Overwrite:")).strong());
        ui.label(describe(dst));
        ui.label(RichText::new("mit:").strong());
        ui.label(describe(src));
        ui.add_space(8.0);
        let mut answer = None;
        ui.horizontal_wrapped(|ui| {
            for (label, a) in [
                (l!("Überschreiben", "Overwrite"), OverwriteAnswer::Overwrite),
                (l!("Alle", "All"), OverwriteAnswer::OverwriteAll),
                (l!("Alle älteren", "All older"), OverwriteAnswer::OverwriteOlder),
                (l!("Überspringen", "Skip"), OverwriteAnswer::Skip),
                (l!("Alle überspringen", "Skip all"), OverwriteAnswer::SkipAll),
                (l!("Umbenennen", "Rename"), OverwriteAnswer::Rename),
                (l!("Abbrechen", "Cancel"), OverwriteAnswer::Cancel),
            ] {
                if ui.button(label).clicked() {
                    answer = Some(a);
                }
            }
        });
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            answer = Some(OverwriteAnswer::Cancel);
        }
        if let Some(a) = answer {
            let _ = tx.send(a);
        }
    });
}

/// Shortcut list for Help → Keyboard shortcuts (left column goes through `i18n::keys`).
fn keys_table() -> Vec<(&'static str, &'static str)> {
    vec![
    ("Tab", l!("Panel wechseln", "Switch panel")),
    ("Enter", l!("Öffnen / Ordner betreten / ZIP öffnen", "Open / enter folder / open archive")),
    ("Backspace", l!("Übergeordneter Ordner", "Parent folder")),
    ("Einfg / Leertaste", l!("Markieren (Leertaste berechnet Ordnergröße)", "Mark (Space calculates the folder size)")),
    ("Shift+Pfeile", l!("Bereich markieren", "Mark range")),
    ("Strg+Klick / Shift+Klick", l!("Markieren mit der Maus", "Mark with the mouse")),
    ("+ / - / *", l!("Gruppe markieren / abwählen / umkehren", "Select / deselect / invert group")),
    ("Buchstaben tippen", l!("Schnellsuche nach Namen", "Quick search by name")),
    ("Strg+C / Strg+X / Strg+V", l!("Kopieren / Ausschneiden / Einfügen (auch mit Dolphin & Co.)", "Copy / cut / paste (also with Dolphin & co.)")),
    ("Rechtsklick", l!("Kontextmenü (auf freier Fläche: Einfügen, Neuer Ordner, …)", "Context menu (on empty space: paste, new folder, …)")),
    ("F3", l!("Ansehen (Text/Hex/Bild)", "View (text/hex/image)")),
    ("F4 / Shift+F4", l!("Bearbeiten / Neue Datei", "Edit / New file")),
    ("F5 / Shift+F5", l!("Kopieren / Kopieren im selben Ordner", "Copy / copy within the same folder")),
    ("F6 / Shift+F6", l!("Verschieben / Umbenennen", "Move / Rename")),
    ("F7", l!("Neuer Ordner", "New folder")),
    ("F8 / Entf", l!("In den Papierkorb", "Move to trash")),
    ("Shift+F8 / Shift+Entf", l!("Endgültig löschen", "Delete permanently")),
    ("F9", l!("Terminal im aktuellen Ordner", "Terminal in the current folder")),
    ("Alt+F5 / Alt+F9", l!("Packen / Entpacken nach…", "Pack / Unpack to…")),
    ("Alt+Shift+F9", l!("Smart hier entpacken (eigener Ordner, wenn nötig)", "Smart unpack here (own folder if needed)")),
    ("Alt+F7", l!("Dateien suchen", "Find files")),
    ("Strg+F / Strg+Shift+F", l!("Server verbinden (FTP/SFTP) / trennen", "Connect to server (FTP/SFTP) / disconnect")),
    ("Alt+Enter", l!("Eigenschaften", "Properties")),
    ("Alt+← / Alt+→ / Alt+↓", l!("Zurück / Vor / Verlauf", "Back / Forward / History")),
    ("Strg+← / Strg+→", l!("Ordner unter Cursor links/rechts öffnen", "Open folder under cursor left/right")),
    ("Strg+T / Strg+W", l!("Neuer Tab / Tab schließen", "New tab / Close tab")),
    ("Strg+Tab", l!("Nächster Tab", "Next tab")),
    ("Strg+D / Strg+Shift+D", l!("Favoriten / aktuellen Ordner hinzufügen", "Favourites / add current folder")),
    ("Strg+Q", l!("Schnellansicht", "Quick view")),
    ("Strg+Shift+B", l!("Branch-View: alle Dateien aller Unterordner", "Branch view: all files of all subfolders")),
    ("Strg+S", l!("Schnellfilter", "Quick filter")),
    ("Strg+H", l!("Versteckte Dateien", "Hidden files")),
    ("Strg+M", l!("Mehrfach-Umbenennen", "Multi-rename")),
    ("Strg+U", l!("Panels tauschen", "Swap panels")),
    ("Strg+G / Knopf „=“", l!("Gleicher Ordner wie im anderen Panel", "Same folder as in the other panel")),
    ("Strg+=", l!("Ziel = Quelle (anderes Panel übernimmt diesen Ordner)", "Target = source (other panel takes this folder)")),
    ("Strg+R / F2", l!("Neu einlesen", "Reload")),
    ("Strg+A / Strg+Shift+A", l!("Alles markieren / abwählen", "Select all / deselect all")),
    ("Strg+Shift+C / N", l!("Pfade / Namen kopieren", "Copy paths / names")),
    ("Strg+Enter", l!("Dateiname in die Kommandozeile", "File name into the command line")),
    ("Strg+E", l!("Kommandozeile fokussieren", "Focus the command line")),
    ("Strg+F3…F6", l!("Sortieren nach Name/Erw./Datum/Größe", "Sort by name/ext/date/size")),
    ("Strg+\\", l!("Wurzelverzeichnis", "Root folder")),
    ("Strg+Pos1", l!("Home-Ordner", "Home folder")),
    ("Strg+,", l!("Einstellungen", "Settings")),
]
}

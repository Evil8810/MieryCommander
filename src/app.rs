use crate::config::{Config, STORAGE_KEY, ThemeChoice};
use crate::dialogs::{CopyMode, Dialog, FtpForm};
use crate::remote;
use crate::fsutil;
use crate::ops::{Job, JobKind};
use crate::panel::{self, Location, Panel, PanelAction};
use crate::rename::MultiRename;
use crate::search::{Search, SearchAction};
use crate::viewer::{QuickView, Viewer};
use eframe::egui::{self, Event, Key, Modifiers, RichText};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    View,
    Edit,
    NewFile,
    Copy,
    CopySameDir,
    Move,
    Rename,
    Mkdir,
    Delete,
    DeletePermanent,
    Pack,
    Unpack,
    Search,
    MultiRename,
    QuickView,
    Filter,
    ToggleHidden,
    Reload,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    Swap,
    SameDir,
    Hotlist,
    AddHotlist,
    History,
    Back,
    Forward,
    Up,
    Root,
    Home,
    Terminal,
    CopyPaths,
    CopyNames,
    SelectAll,
    DeselectAll,
    Invert,
    SelectPattern,
    DeselectPattern,
    CompareDirs,
    Settings,
    Exit,
    Properties,
    NameToCmdline,
    OpenDefault,
    Keys,
    About,
    SortName,
    SortExt,
    SortSize,
    SortDate,
    FtpConnect,
    FtpDisconnect,
    ClipCopy,
    ClipCut,
    ClipPaste,
    /// Open the folder under the cursor in the other panel.
    OpenInOther,
    /// Open the folder under the cursor in a new tab.
    OpenInNewTab,
    CalcSize,
    /// Add the folder under the cursor to the favourites.
    AddEntryToHotlist,
    /// The active panel goes to the other panel's folder.
    TakeOtherDir,
    BranchView,
    CompareFiles,
    SyncDirs,
    UnpackHere,
    UnpackSmart,
}

/// What to do once a download for F3/F4/Enter on an FTP file has finished.
pub enum AfterJob {
    View(PathBuf),
    Open(PathBuf),
    Edit { local: PathBuf, conn: usize, remote: String },
}

/// A remote file opened in the editor; uploaded again when it changes.
pub struct FtpEdit {
    pub local: PathBuf,
    pub conn: usize,
    pub remote: String,
    pub mtime: Option<std::time::SystemTime>,
}

pub struct MieryApp {
    pub cfg: Config,
    pub left: Panel,
    pub right: Panel,
    pub right_active: bool,
    pub dialog: Option<Dialog>,
    pub dialog_fresh: bool,
    pub job: Option<Job>,
    pub viewers: Vec<Viewer>,
    pub compares: Vec<crate::compare::CompareWindow>,
    pub sync: crate::sync::SyncWindow,
    next_viewer: u64,
    pub quick_view: bool,
    qv: QuickView,
    pub search: Search,
    pub rename: MultiRename,
    pub cmdline: String,
    focus_cmdline: bool,
    pub toast: Option<(String, Instant, bool)>,
    pub(crate) keys: Vec<(Key, Modifiers)>,
    pub(crate) texts: Vec<String>,
    applied_theme: Option<ThemeChoice>,
    panel_rects: [egui::Rect; 2],
    pub after_job: Option<AfterJob>,
    pub ftp_edits: Vec<FtpEdit>,
    pub(crate) ftp_ops: Vec<std::sync::mpsc::Receiver<Result<String, String>>>,
    startup_frames: u8,
    /// SMB share being made available (kio-fuse / gio / Finder).
    pub smb_mounting: Option<std::sync::mpsc::Receiver<Result<PathBuf, String>>>,
    /// Files copied/cut with Ctrl+C / Ctrl+X.
    pub clip: Option<crate::clip::FileClip>,
    pub ftp_connecting: Option<std::sync::mpsc::Receiver<Result<std::sync::Arc<dyn remote::Remote>, remote::ConnectError>>>,
}

impl MieryApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        fsutil::set_ui_context(&cc.egui_ctx);
        crate::fonts::install_in_background(&cc.egui_ctx);
        let cfg: Config = cc
            .storage
            .and_then(|s| eframe::get_value(s, STORAGE_KEY))
            .unwrap_or_default();
        let home = dirs::home_dir().unwrap_or_else(|| "/".into());
        let cwd = std::env::current_dir().unwrap_or_else(|_| home.clone());
        // A path on the command line opens in the left panel.
        let arg_dir = std::env::args().nth(1).map(PathBuf::from).filter(|p| p.is_dir());
        let left_paths = match &arg_dir {
            Some(p) => vec![p.clone()],
            None if cfg.left_tabs.is_empty() => vec![cwd],
            None => cfg.left_tabs.clone(),
        };
        let right_paths = if cfg.right_tabs.is_empty() {
            vec![home]
        } else {
            cfg.right_tabs.clone()
        };
        let left = Panel::new(&left_paths, cfg.left_active, cfg.show_hidden, cfg.dirs_first);
        let right = Panel::new(&right_paths, cfg.right_active, cfg.show_hidden, cfg.dirs_first);
        cc.egui_ctx.set_zoom_factor(cfg.font_scale.clamp(0.6, 2.5));
        MieryApp {
            cfg,
            left,
            right,
            right_active: false,
            dialog: None,
            dialog_fresh: false,
            job: None,
            viewers: Vec::new(),
            compares: Vec::new(),
            sync: crate::sync::SyncWindow::new(),
            next_viewer: 0,
            quick_view: false,
            qv: QuickView::default(),
            search: Search::new(),
            rename: MultiRename::new(),
            cmdline: String::new(),
            focus_cmdline: false,
            toast: None,
            keys: Vec::new(),
            texts: Vec::new(),
            applied_theme: None,
            panel_rects: [egui::Rect::NOTHING; 2],
            after_job: None,
            ftp_edits: Vec::new(),
            ftp_ops: Vec::new(),
            ftp_connecting: None,
            startup_frames: 0,
            smb_mounting: None,
            clip: None,
        }
    }

    pub fn active(&mut self) -> &mut Panel {
        if self.right_active { &mut self.right } else { &mut self.left }
    }

    pub fn active_ref(&self) -> &Panel {
        if self.right_active { &self.right } else { &self.left }
    }

    pub fn other(&mut self) -> &mut Panel {
        if self.right_active { &mut self.left } else { &mut self.right }
    }

    pub fn other_ref(&self) -> &Panel {
        if self.right_active { &self.left } else { &self.right }
    }

    pub fn active_dir(&self) -> PathBuf {
        self.active_ref().tab().loc.real_dir()
    }

    pub fn notify(&mut self, msg: impl Into<String>, is_error: bool) {
        self.toast = Some((msg.into(), Instant::now(), is_error));
    }

    pub fn open_dialog(&mut self, d: Dialog) {
        self.dialog = Some(d);
        self.dialog_fresh = true;
    }

    pub fn reload_all(&mut self) {
        let (h, d) = (self.cfg.show_hidden, self.cfg.dirs_first);
        for t in self.left.tabs.iter_mut().chain(self.right.tabs.iter_mut()) {
            t.reload(h, d);
        }
    }

    pub fn navigate_active(&mut self, loc: Location) {
        let (h, d) = (self.cfg.show_hidden, self.cfg.dirs_first);
        self.active().tab_mut().navigate(loc, h, d);
    }

    /// Navigate the active panel to the folder of `file` and put the cursor on it.
    pub fn goto_file(&mut self, file: &Path) {
        if let Some(parent) = file.parent() {
            self.navigate_active(Location::Dir(parent.to_path_buf()));
            let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
            self.active().tab_mut().select_name(&name);
        }
    }

    pub fn start_job(&mut self, kind: JobKind, sources: Vec<PathBuf>, dest: PathBuf, ctx: &egui::Context) {
        if self.job.is_some() {
            self.notify("Es läuft bereits eine Operation", true);
            return;
        }
        self.job = Some(Job::start(kind, sources, dest, ctx.clone()));
    }

    fn open_viewer(&mut self, path: &Path) {
        self.next_viewer += 1;
        self.viewers.push(Viewer::new(self.next_viewer, path));
    }

    fn keyboard_for_panels(&self, ctx: &egui::Context) -> bool {
        let question = self
            .job
            .as_ref()
            .is_some_and(|j| j.progress.lock().unwrap().question.is_some());
        self.dialog.is_none() && !question && !ctx.egui_wants_keyboard_input()
    }

    // ------------------------------------------------------------------------
    // Commands
    // ------------------------------------------------------------------------

    pub fn run(&mut self, cmd: Cmd, ctx: &egui::Context) {
        let (h, d) = (self.cfg.show_hidden, self.cfg.dirs_first);
        let loc = self.active_ref().tab().loc.clone();
        let in_archive = matches!(loc, Location::Archive { .. });
        let local = loc.dir().is_some();
        let remote = loc.ftp().map(|(id, _)| id);
        // F3/F4/Enter on a remote file: download into the cache first.
        if let Some(id) = remote
            && matches!(cmd, Cmd::View | Cmd::Edit | Cmd::OpenDefault)
        {
            if let Some(e) = self.active_ref().tab().current().cloned()
                && !e.is_dir
                && !e.is_parent
            {
                self.download_and(id, &e, cmd, ctx);
            }
            return;
        }
        // …and a file inside an archive: extract it into the cache first.
        if let Location::Archive { file, inner } = &loc
            && matches!(cmd, Cmd::View | Cmd::Edit | Cmd::OpenDefault)
        {
            if let Some(e) = self.active_ref().tab().current().cloned()
                && !e.is_dir
                && !e.is_parent
            {
                let cache = std::env::temp_dir()
                    .join(format!("miery-archive-{}", std::process::id()))
                    .join(file.file_name().unwrap_or_default());
                let local = cache.join(&e.name);
                let _ = std::fs::create_dir_all(&cache);
                let _ = std::fs::remove_file(&local);
                self.after_job = Some(match cmd {
                    Cmd::View => AfterJob::View(local),
                    _ => {
                        if cmd == Cmd::Edit {
                            self.notify("Bearbeitet wird eine Kopie – Änderungen landen nicht im Archiv", false);
                        }
                        AfterJob::Open(local)
                    }
                });
                let kind = JobKind::Extract { archive: file.clone(), base: inner.clone() };
                self.start_job(kind, vec![e.path.clone()], cache, ctx);
            }
            return;
        }
        match cmd {
            Cmd::View => {
                let cur = self.active_ref().tab().current().cloned();
                match cur {
                    Some(e) if !e.is_dir && !in_archive => self.open_viewer(&e.path),
                    Some(e) if e.is_dir && !e.is_parent => {
                        self.active().tab_mut().calc_dir_size(&e);
                    }
                    _ => {}
                }
            }
            Cmd::Edit => {
                if let Some(e) = self.active_ref().tab().current().cloned()
                    && !e.is_dir
                    && !in_archive
                    && let Err(err) = fsutil::open_with(&self.cfg.editor, &e.path)
                {
                    self.notify(format!("Editor: {err}"), true);
                }
            }
            Cmd::OpenDefault => {
                if let Some(e) = self.active_ref().tab().current().cloned()
                    && !e.is_parent
                    && !in_archive
                    && let Err(err) = fsutil::open_default(&e.path)
                {
                    self.notify(err, true);
                }
            }
            Cmd::NewFile => {
                if local {
                    self.open_dialog(Dialog::NewFile { name: String::new() });
                }
            }
            Cmd::Copy | Cmd::Move | Cmd::CopySameDir => self.prepare_copy_move(cmd),
            Cmd::Rename => {
                if let Some(e) = self.active_ref().tab().current().cloned()
                    && !e.is_parent
                    && !in_archive
                {
                    // In the branch view the name is a relative path: rename only the file name.
                    let name = e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(e.name.clone());
                    self.open_dialog(Dialog::Rename { from: e.path.clone(), name, ftp: remote });
                }
            }
            Cmd::Mkdir => {
                if !in_archive {
                    self.open_dialog(Dialog::Mkdir { name: String::new() });
                }
            }
            Cmd::Delete | Cmd::DeletePermanent => {
                if in_archive {
                    self.notify("Löschen in Archiven wird nicht unterstützt", true);
                    return;
                }
                let sel = self.active_ref().tab().selection();
                let paths: Vec<PathBuf> = sel.iter().map(|e| e.path.clone()).collect();
                if paths.is_empty() {
                    return;
                }
                if let Some(id) = remote {
                    let dirs = sel.iter().filter(|e| e.is_dir).map(|e| e.path.clone()).collect();
                    self.open_dialog(Dialog::Delete { paths, permanent: true, ftp: Some((id, dirs)) });
                    return;
                }
                let permanent = cmd == Cmd::DeletePermanent || !self.cfg.delete_to_trash;
                if self.cfg.confirm_delete || permanent {
                    self.open_dialog(Dialog::Delete { paths, permanent, ftp: None });
                } else {
                    self.start_job(JobKind::Delete { to_trash: true }, paths, PathBuf::new(), ctx);
                }
            }
            Cmd::Pack => {
                if !local {
                    self.notify("Packen geht nur in lokalen Ordnern", true);
                    return;
                }
                let sel = self.active_ref().tab().selection();
                if sel.is_empty() {
                    return;
                }
                let base = if sel.len() == 1 {
                    sel[0].split_name().0.to_string()
                } else {
                    self.active_ref().tab().title()
                };
                let target = self.other_ref().tab().loc.real_dir().join(format!("{base}.zip"));
                self.open_dialog(Dialog::Pack {
                    sources: sel.into_iter().map(|e| e.path).collect(),
                    target: target.to_string_lossy().into_owned(),
                });
            }
            Cmd::Unpack | Cmd::UnpackHere | Cmd::UnpackSmart => {
                // All selected archives (or the one under the cursor).
                let archives: Vec<PathBuf> = self
                    .active_ref()
                    .tab()
                    .selection()
                    .into_iter()
                    .filter(|e| !e.is_dir && crate::archive::is_archive(&e.path))
                    .map(|e| e.path)
                    .collect();
                if !local || archives.is_empty() {
                    self.notify("Bitte ein Archiv auswählen (zip, 7z, rar, tar.gz …)", true);
                    return;
                }
                let here = self.active_dir();
                match cmd {
                    Cmd::Unpack => {
                        let dest = match self.other_ref().tab().loc.dir() {
                            Some(d) => d.to_path_buf(),
                            None => here,
                        };
                        self.open_dialog(Dialog::Unpack { archives, target: dest.to_string_lossy().into_owned(), smart: true });
                    }
                    _ => self.start_job(JobKind::Unpack { smart: cmd == Cmd::UnpackSmart }, archives, here, ctx),
                }
            }
            Cmd::Search => {
                let dir = self.active_dir();
                self.search.open_at(&dir);
            }
            Cmd::MultiRename => {
                if !local {
                    self.notify("Mehrfach-Umbenennen geht nur in lokalen Ordnern", true);
                    return;
                }
                let files: Vec<(PathBuf, bool)> =
                    self.active_ref().tab().selection().into_iter().map(|e| (e.path, e.is_dir)).collect();
                if !files.is_empty() {
                    self.rename.start(files);
                }
            }
            Cmd::QuickView => self.quick_view = !self.quick_view,
            Cmd::Filter => {
                let side = if self.right_active { "right" } else { "left" };
                let t = self.active().tab_mut();
                t.filter_open = true;
                ctx.memory_mut(|m| m.request_focus(egui::Id::new(format!("{side}_filter"))));
            }
            Cmd::ToggleHidden => {
                self.cfg.show_hidden = !self.cfg.show_hidden;
                self.reload_all();
            }
            Cmd::Reload => self.reload_all(),
            Cmd::NewTab => {
                let loc = self.active_ref().tab().loc.real_dir();
                let p = self.active();
                p.tabs.insert(p.active + 1, panel::Tab::new(loc, h, d));
                p.active += 1;
            }
            Cmd::CloseTab => {
                let p = self.active();
                if p.tabs.len() > 1 {
                    p.tabs.remove(p.active);
                    p.active = p.active.min(p.tabs.len() - 1);
                }
            }
            Cmd::NextTab => {
                let p = self.active();
                p.active = (p.active + 1) % p.tabs.len();
            }
            Cmd::PrevTab => {
                let p = self.active();
                p.active = (p.active + p.tabs.len() - 1) % p.tabs.len();
            }
            Cmd::Swap => std::mem::swap(&mut self.left, &mut self.right),
            Cmd::SyncDirs => match (self.left.tab().loc.dir(), self.right.tab().loc.dir()) {
                (Some(l), Some(r)) => {
                    let (l, r) = (l.to_path_buf(), r.to_path_buf());
                    self.sync.open_with(&l, &r);
                }
                _ => self.notify("Synchronisieren geht nur zwischen zwei lokalen Ordnern (auch eingebundene Netzlaufwerke)", true),
            },
            Cmd::CompareFiles => match self.compare_pair() {
                Ok((a, b)) => {
                    self.next_viewer += 1;
                    self.compares.push(crate::compare::CompareWindow::new(self.next_viewer, a, b));
                }
                Err(msg) => self.notify(msg, true),
            },
            Cmd::BranchView => {
                if !local {
                    self.notify("Branch-View gibt es nur für lokale Ordner", true);
                    return;
                }
                let t = self.active().tab_mut();
                t.branch = !t.branch;
                t.marked.clear();
                t.reload(h, d);
            }
            Cmd::TakeOtherDir => {
                let other = self.other_ref().tab().loc.clone();
                self.active().tab_mut().navigate(other, h, d);
            }
            Cmd::SameDir => {
                let loc = self.active_ref().tab().loc.clone();
                self.other().tab_mut().navigate(loc, h, d);
            }
            Cmd::Hotlist => self.open_dialog(Dialog::Hotlist),
            Cmd::AddHotlist => {
                let dir = self.active_dir();
                if !self.cfg.hotlist.contains(&dir) {
                    self.cfg.hotlist.push(dir.clone());
                }
                self.notify(format!("Zu Favoriten hinzugefügt: {}", dir.to_string_lossy()), false);
            }
            Cmd::History => self.open_dialog(Dialog::History),
            Cmd::Back => self.active().tab_mut().go_back(h, d),
            Cmd::Forward => self.active().tab_mut().go_forward(h, d),
            Cmd::Up => self.active().tab_mut().go_up(h, d),
            Cmd::Root => self.navigate_active(Location::Dir("/".into())),
            Cmd::Home => {
                if let Some(home) = dirs::home_dir() {
                    self.navigate_active(Location::Dir(home));
                }
            }
            Cmd::Terminal => {
                let dir = self.active_dir();
                if let Err(e) = fsutil::open_terminal(&self.cfg.terminal, &dir) {
                    self.notify(e, true);
                }
            }
            Cmd::CopyPaths | Cmd::CopyNames => {
                let sel = self.active_ref().tab().selection();
                let text: Vec<String> = sel
                    .iter()
                    .map(|e| {
                        if cmd == Cmd::CopyPaths {
                            e.path.to_string_lossy().into_owned()
                        } else {
                            e.name.clone()
                        }
                    })
                    .collect();
                ctx.copy_text(text.join("\n"));
                self.notify(format!("{} Eintrag/Einträge in die Zwischenablage kopiert", sel.len()), false);
            }
            Cmd::SelectAll => self.active().tab_mut().mark_all(true),
            Cmd::DeselectAll => self.active().tab_mut().mark_all(false),
            Cmd::Invert => self.active().tab_mut().invert_marks(),
            Cmd::SelectPattern => self.open_dialog(Dialog::Pattern { select: true, mask: "*.*".into() }),
            Cmd::DeselectPattern => self.open_dialog(Dialog::Pattern { select: false, mask: "*.*".into() }),
            Cmd::CompareDirs => self.compare_dirs(),
            Cmd::Settings => self.open_dialog(Dialog::Settings),
            Cmd::Exit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Cmd::Properties => {
                if let Some(e) = self.active_ref().tab().current().cloned()
                    && !e.is_parent
                {
                    self.open_dialog(Dialog::properties(&e, local));
                }
            }
            Cmd::NameToCmdline => {
                if let Some(e) = self.active_ref().tab().current() {
                    let name = if e.name.contains(' ') { format!("\"{}\"", e.name) } else { e.name.clone() };
                    if !self.cmdline.is_empty() && !self.cmdline.ends_with(' ') {
                        self.cmdline.push(' ');
                    }
                    self.cmdline.push_str(&name);
                    self.cmdline.push(' ');
                    self.focus_cmdline = true;
                }
            }
            Cmd::Keys => self.open_dialog(Dialog::Keys),
            Cmd::About => self.open_dialog(Dialog::Message {
                title: "Über MieryCommander".into(),
                text: format!(
                    "MieryCommander {}\nEin schneller, zweispaltiger Dateimanager.\nGeschrieben in Rust mit egui – für Linux und macOS.",
                    env!("CARGO_PKG_VERSION")
                ),
            }),
            Cmd::SortName | Cmd::SortExt | Cmd::SortSize | Cmd::SortDate => {
                let key = match cmd {
                    Cmd::SortName => panel::SortKey::Name,
                    Cmd::SortExt => panel::SortKey::Ext,
                    Cmd::SortSize => panel::SortKey::Size,
                    _ => panel::SortKey::Date,
                };
                let t = self.active().tab_mut();
                if t.sort == key {
                    t.desc = !t.desc;
                } else {
                    t.sort = key;
                    t.desc = false;
                }
                t.apply_view(d);
            }
            Cmd::FtpConnect => {
                let form = FtpForm::new(&self.cfg.sites);
                self.open_dialog(Dialog::FtpConnect(form));
            }
            Cmd::ClipCopy | Cmd::ClipCut => self.clip_put(cmd == Cmd::ClipCut),
            Cmd::ClipPaste => self.clip_paste(ctx),
            Cmd::OpenInOther | Cmd::OpenInNewTab | Cmd::AddEntryToHotlist | Cmd::CalcSize => {
                let Some(e) = self.active_ref().tab().current().cloned() else { return };
                if !e.is_dir {
                    return;
                }
                if cmd == Cmd::CalcSize {
                    self.active().tab_mut().calc_dir_size(&e);
                    return;
                }
                // The folder as a location (".." = parent of the current one).
                let target = if e.is_parent {
                    self.active_ref().tab().loc.parent_loc()
                } else {
                    match &loc {
                        Location::Dir(_) => Some(Location::Dir(e.path.clone())),
                        Location::Ftp { id, .. } => Some(Location::Ftp { id: *id, path: e.path.to_string_lossy().into_owned() }),
                        Location::Archive { file, .. } => Some(Location::Archive {
                            file: file.clone(),
                            inner: format!("{}/", e.path.to_string_lossy()),
                        }),
                    }
                };
                let Some(target) = target else { return };
                match cmd {
                    Cmd::OpenInOther => self.other().tab_mut().navigate(target, h, d),
                    Cmd::OpenInNewTab => {
                        let p = self.active();
                        p.tabs.insert(p.active + 1, panel::Tab::new_at(target, h, d));
                        p.active += 1;
                    }
                    _ => match target.dir() {
                        Some(dir) => {
                            if !self.cfg.hotlist.iter().any(|x| x == dir) {
                                self.cfg.hotlist.push(dir.to_path_buf());
                            }
                            self.notify(format!("Zu Favoriten hinzugefügt: {}", dir.display()), false);
                        }
                        None => self.notify("Nur lokale Ordner können Favoriten sein", true),
                    },
                }
            }
            Cmd::FtpDisconnect => match remote {
                Some(id) => self.ftp_disconnect(id),
                None => self.notify("Das aktive Panel ist mit keinem Server verbunden", true),
            },
        }
    }

    /// Which two files to compare: two marked files in the active panel, or
    /// the file under the cursor in each panel.
    fn compare_pair(&self) -> Result<(PathBuf, PathBuf), String> {
        let hint = "Bitte zwei Dateien markieren oder in beiden Panels je eine Datei auswählen";
        let act = self.active_ref().tab();
        if act.loc.dir().is_none() {
            return Err("Vergleichen geht nur mit lokalen Dateien".into());
        }
        let marked: Vec<PathBuf> =
            act.entries.iter().filter(|e| act.marked.contains(&e.name) && !e.is_dir).map(|e| e.path.clone()).collect();
        if marked.len() == 2 {
            return Ok((marked[0].clone(), marked[1].clone()));
        }
        let oth = self.other_ref().tab();
        match (act.current(), oth.current(), oth.loc.dir()) {
            (Some(a), Some(b), Some(_)) if !a.is_dir && !b.is_dir => {
                // Keep "left file left": the order follows the panels on screen.
                if self.right_active { Ok((b.path.clone(), a.path.clone())) } else { Ok((a.path.clone(), b.path.clone())) }
            }
            _ => Err(hint.into()),
        }
    }

    /// Ctrl+C / Ctrl+X: remember the selection (and offer local files to the system).
    fn clip_put(&mut self, cut: bool) {
        let tab = self.active_ref().tab();
        let sel = tab.selection();
        if sel.is_empty() {
            return;
        }
        let source = match &tab.loc {
            Location::Dir(_) => crate::clip::ClipSource::Local,
            Location::Ftp { id, .. } => crate::clip::ClipSource::Remote(*id),
            Location::Archive { file, inner } => {
                if cut {
                    self.notify("Aus Archiven kann nur kopiert werden", true);
                    return;
                }
                crate::clip::ClipSource::Archive { file: file.clone(), inner: inner.clone() }
            }
        };
        let paths: Vec<PathBuf> = sel.iter().map(|e| e.path.clone()).collect();
        let dirs = sel.iter().filter(|e| e.is_dir).map(|e| e.path.clone()).collect();
        if source == crate::clip::ClipSource::Local {
            crate::clip::to_system(&paths);
        }
        let n = paths.len();
        self.clip = Some(crate::clip::FileClip { paths, dirs, cut, source });
        self.active().tab_mut().marked.clear();
        self.notify(
            format!("{n} Element(e) {} – mit Strg+V einfügen", if cut { "ausgeschnitten" } else { "kopiert" }),
            false,
        );
    }

    /// Paths that are cut and shown dimmed in the panels.
    pub fn cut_paths(&self) -> Vec<PathBuf> {
        match &self.clip {
            Some(c) if c.cut => c.paths.clone(),
            _ => Vec::new(),
        }
    }

    /// Ctrl+V: paste into the active panel's folder.
    fn clip_paste(&mut self, ctx: &egui::Context) {
        use crate::clip::{ClipSource, FileClip};
        // Files copied in another program win, unless they are ours anyway.
        let clip = match (crate::clip::from_system(), self.clip.clone()) {
            (Some((paths, cut)), mine) if mine.as_ref().is_none_or(|m| m.paths != paths) => {
                let dirs = paths.iter().filter(|p| p.is_dir()).cloned().collect();
                Some(FileClip { paths, dirs, cut, source: ClipSource::Local })
            }
            (_, mine) => mine,
        };
        let Some(clip) = clip else {
            self.notify("Die Zwischenablage enthält keine Dateien", true);
            return;
        };
        let target = self.active_ref().tab().loc.clone();
        let (kind, dest, policy) = match (&clip.source, &target) {
            (ClipSource::Local, Location::Dir(dir)) => {
                let same_dir = clip.paths.iter().all(|p| p.parent() == Some(dir.as_path()));
                if clip.cut && same_dir {
                    self.notify("Die Dateien liegen bereits in diesem Ordner", false);
                    return;
                }
                let kind = if clip.cut { JobKind::Move } else { JobKind::Copy };
                // A copy into the same folder becomes "name (2).ext".
                let policy = same_dir.then_some(crate::ops::OverwriteAnswer::Rename);
                (kind, dir.clone(), policy)
            }
            (ClipSource::Local, Location::Ftp { id, path }) => {
                (JobKind::Upload { conn: *id, delete_source: clip.cut, overwrite: false }, PathBuf::from(path), None)
            }
            (ClipSource::Remote(id), Location::Dir(dir)) => (
                JobKind::Download { conn: *id, dirs: clip.dirs.clone(), delete_source: clip.cut },
                dir.clone(),
                None,
            ),
            (ClipSource::Archive { file, inner }, Location::Dir(dir)) => {
                (JobKind::Extract { archive: file.clone(), base: inner.clone() }, dir.clone(), None)
            }
            (_, Location::Archive { .. }) => {
                self.notify("In Archive einfügen geht nicht – bitte Alt+F5 (Packen) nutzen", true);
                return;
            }
            _ => {
                self.notify("Einfügen zwischen zwei Servern wird nicht unterstützt", true);
                return;
            }
        };
        if self.job.is_some() {
            self.notify("Es läuft bereits eine Operation", true);
            return;
        }
        self.job = Some(crate::ops::Job::start_with_policy(kind, clip.paths.clone(), dest, ctx.clone(), policy));
        if clip.cut {
            self.clip = None; // moved files are gone from their old place
        }
    }

    pub fn ftp_disconnect(&mut self, id: usize) {
        remote::disconnect(id);
        let (h, d) = (self.cfg.show_hidden, self.cfg.dirs_first);
        let home = dirs::home_dir().unwrap_or_else(|| "/".into());
        for t in self.left.tabs.iter_mut().chain(self.right.tabs.iter_mut()) {
            if t.loc.ftp().is_some_and(|(i, _)| i == id) {
                t.navigate(Location::Dir(home.clone()), h, d);
            }
        }
        self.notify("Verbindung getrennt", false);
    }

    /// Run a small FTP operation (mkdir, rename, …) off the UI thread.
    pub fn spawn_ftp_op(&mut self, f: impl FnOnce() -> Result<String, String> + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        self.ftp_ops.push(rx);
    }

    fn poll_ftp(&mut self, ctx: &egui::Context) {
        let mut done = Vec::new();
        self.ftp_ops.retain(|rx| match rx.try_recv() {
            Ok(r) => {
                done.push(r);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        for r in done {
            match r {
                Ok(msg) => self.notify(msg, false),
                Err(e) => self.notify(e, true),
            }
            self.reload_all();
        }
        if !self.ftp_ops.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        if let Some(rx) = &self.ftp_connecting {
            match rx.try_recv() {
                Ok(Ok(conn)) => {
                    self.ftp_connecting = None;
                    self.dialog = None;
                    self.navigate_active(Location::Ftp { id: conn.id(), path: conn.home().to_string() });
                    self.notify(format!("Verbunden mit {}", conn.url()), false);
                }
                Ok(Err(e)) => {
                    self.ftp_connecting = None;
                    if let Some(Dialog::FtpConnect(form)) = &mut self.dialog {
                        match e {
                            remote::ConnectError::UnknownHostKey(fp) => {
                                form.status = None;
                                form.host_key = Some(fp);
                            }
                            remote::ConnectError::Failed(msg) => form.status = Some((true, msg)),
                        }
                    } else {
                        self.notify(e.to_string(), true);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                Err(_) => self.ftp_connecting = None,
            }
        }

        if let Some(rx) = &self.smb_mounting {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    self.smb_mounting = None;
                    self.dialog = None;
                    self.notify(format!("SMB-Freigabe eingebunden: {}", path.display()), false);
                    self.navigate_active(Location::Dir(path));
                }
                Ok(Err(e)) => {
                    self.smb_mounting = None;
                    if let Some(Dialog::FtpConnect(form)) = &mut self.dialog {
                        form.status = Some((true, e));
                    } else {
                        self.notify(e, true);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                Err(_) => self.smb_mounting = None,
            }
        }

        // Files edited from FTP: offer to upload them again after saving.
        if self.dialog.is_none() {
            for i in 0..self.ftp_edits.len() {
                let m = std::fs::metadata(&self.ftp_edits[i].local).and_then(|m| m.modified()).ok();
                if m.is_some() && m != self.ftp_edits[i].mtime {
                    self.ftp_edits[i].mtime = m;
                    let e = &self.ftp_edits[i];
                    let d = Dialog::FtpReupload { local: e.local.clone(), conn: e.conn, remote: e.remote.clone() };
                    self.open_dialog(d);
                    break;
                }
            }
        }
    }

    pub fn start_ftp_connect(&mut self, site: remote::Site, password: String, trusted_host_key: Option<String>) {
        if site.protocol == remote::Protocol::Smb {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(crate::smb::mount(&site.host, &site.remote_dir, &site.user, &password));
                fsutil::wake_ui();
            });
            self.smb_mounting = Some(rx);
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(remote::connect(site, password, trusted_host_key));
        });
        self.ftp_connecting = Some(rx);
    }

    /// Download a remote file into the local cache, then view/open/edit it.
    fn download_and(&mut self, id: usize, e: &crate::fsutil::Entry, cmd: Cmd, ctx: &egui::Context) {
        let remote = e.path.to_string_lossy().into_owned();
        let local = remote::cache_dir(id).join(remote.trim_start_matches('/'));
        let dir = local.parent().unwrap_or(Path::new("/")).to_path_buf();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.notify(format!("Cache: {err}"), true);
            return;
        }
        let _ = std::fs::remove_file(&local);
        self.after_job = Some(match cmd {
            Cmd::View => AfterJob::View(local.clone()),
            Cmd::Edit => AfterJob::Edit { local: local.clone(), conn: id, remote },
            _ => AfterJob::Open(local.clone()),
        });
        let kind = JobKind::Download { conn: id, dirs: Vec::new(), delete_source: false };
        self.start_job(kind, vec![e.path.clone()], dir, ctx);
    }

    fn prepare_copy_move(&mut self, cmd: Cmd) {
        let tab = self.active_ref().tab();
        let sel = tab.selection();
        if sel.is_empty() {
            return;
        }
        let is_move = cmd == Cmd::Move;
        let other_loc = self.other_ref().tab().loc.clone();
        let sources: Vec<PathBuf> = sel.iter().map(|e| e.path.clone()).collect();
        let dirs: Vec<PathBuf> = sel.iter().filter(|e| e.is_dir).map(|e| e.path.clone()).collect();
        let with_slash = |mut t: String| {
            if !t.ends_with('/') {
                t.push('/');
            }
            t
        };
        let dialog = match (&tab.loc, &other_loc) {
            (Location::Archive { .. }, _) if is_move => {
                self.notify("Verschieben aus Archiven wird nicht unterstützt – nutze F5", true);
                return;
            }
            (Location::Archive { file, inner }, Location::Dir(dest)) => Dialog::CopyMove {
                is_move: false,
                sources,
                target: dest.to_string_lossy().into_owned(),
                mode: CopyMode::Extract { archive: file.clone(), inner: inner.clone() },
            },
            (Location::Dir(dir), _) if cmd == Cmd::CopySameDir => {
                if sel.len() != 1 {
                    self.notify("Kopieren im selben Ordner geht nur mit einer Datei", true);
                    return;
                }
                let target = fsutil::unique_name(dir, &sel[0].name).to_string_lossy().into_owned();
                Dialog::CopyMove { is_move: false, sources, target, mode: CopyMode::Local }
            }
            (Location::Dir(_), Location::Dir(dest)) => Dialog::CopyMove {
                is_move,
                sources,
                target: with_slash(dest.to_string_lossy().into_owned()),
                mode: CopyMode::Local,
            },
            (Location::Dir(_), Location::Ftp { id, path }) => Dialog::CopyMove {
                is_move,
                sources,
                target: with_slash(path.clone()),
                mode: CopyMode::Upload { conn: *id },
            },
            (Location::Ftp { id, .. }, Location::Dir(dest)) if cmd != Cmd::CopySameDir => Dialog::CopyMove {
                is_move,
                sources,
                target: with_slash(dest.to_string_lossy().into_owned()),
                mode: CopyMode::Download { conn: *id, dirs },
            },
            (Location::Dir(_), Location::Archive { .. }) => {
                self.notify("Kopieren in ein Archiv: bitte Alt+F5 (Packen) nutzen", true);
                return;
            }
            _ => {
                self.notify("Diese Kombination wird nicht unterstützt (FTP ↔ lokaler Ordner geht)", true);
                return;
            }
        };
        self.open_dialog(dialog);
    }

    /// Mark files that are missing or newer compared to the other panel.
    fn compare_dirs(&mut self) {
        use std::collections::HashMap;
        let (Some(_), Some(_)) = (self.left.tab().loc.dir(), self.right.tab().loc.dir()) else {
            self.notify("Vergleich nur zwischen echten Ordnern möglich", true);
            return;
        };
        let index = |p: &Panel| -> HashMap<String, (u64, Option<std::time::SystemTime>)> {
            p.tab()
                .entries
                .iter()
                .filter(|e| !e.is_dir)
                .map(|e| (e.name.clone(), (e.size, e.modified)))
                .collect()
        };
        let li = index(&self.left);
        let ri = index(&self.right);
        let mut counts = [0usize; 2];
        for (n, (panel, other)) in [(&mut self.left, &ri), (&mut self.right, &li)].into_iter().enumerate() {
            let t = panel.tab_mut();
            t.marked.clear();
            for e in t.entries.iter().filter(|e| !e.is_dir) {
                let differs = match other.get(&e.name) {
                    None => true,
                    Some((size, mtime)) => {
                        let newer = match (e.modified, mtime) {
                            (Some(a), Some(b)) => a.duration_since(*b).map(|d| d.as_secs() > 2).unwrap_or(false),
                            _ => false,
                        };
                        newer || (*size != e.size && e.modified >= *mtime)
                    }
                };
                if differs {
                    t.marked.insert(e.name.clone());
                    counts[n] += 1;
                }
            }
        }
        self.notify(
            format!("Vergleich: links {} / rechts {} Datei(en) fehlen oder sind neuer – markiert", counts[0], counts[1]),
            false,
        );
    }

    // ------------------------------------------------------------------------
    // Keyboard
    // ------------------------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let keys = std::mem::take(&mut self.keys);
        let texts = std::mem::take(&mut self.texts);
        let (h, d) = (self.cfg.show_hidden, self.cfg.dirs_first);

        // An open compare window: N/P jump between differences, Esc closes.
        if self.viewers.is_empty() && !self.compares.is_empty() {
            for (key, m) in keys {
                if !m.is_none() {
                    continue;
                }
                let w = self.compares.last_mut().unwrap();
                match key {
                    Key::Escape => {
                        self.compares.pop();
                        break;
                    }
                    Key::N | Key::ArrowDown => w.jump(1),
                    Key::P | Key::ArrowUp => w.jump(-1),
                    _ => {}
                }
            }
            return;
        }
        // An open lister window takes the keyboard.
        if !self.viewers.is_empty() {
            for (key, m) in keys {
                if m.is_none() && matches!(key, Key::Escape | Key::F3 | Key::Q) {
                    self.viewers.pop();
                    break;
                }
                if let Some(v) = self.viewers.last_mut()
                    && let Some(c) = &mut v.content
                    && c.mode != crate::viewer::Mode::Image
                {
                    match key {
                        Key::Num1 => c.mode = crate::viewer::Mode::Text,
                        Key::Num3 => c.mode = crate::viewer::Mode::Hex,
                        Key::W => v.wrap = !v.wrap,
                        _ => {}
                    }
                }
            }
            return;
        }

        for (key, m) in keys {
            let c = m.command;
            let s = m.shift;
            let a = m.alt;
            let page = 20isize;
            let cmd = match (key, c, s, a) {
                (Key::Tab, false, false, false) => {
                    self.right_active = !self.right_active;
                    None
                }
                (Key::Tab, true, false, false) => Some(Cmd::NextTab),
                (Key::Tab, true, true, false) => Some(Cmd::PrevTab),
                (Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown | Key::Home | Key::End, false, shift, false) => {
                    let t = self.active().tab_mut();
                    t.quick_search.clear();
                    let start = t.cursor;
                    match key {
                        Key::ArrowUp => t.move_cursor(-1),
                        Key::ArrowDown => t.move_cursor(1),
                        Key::PageUp => t.move_cursor(-page),
                        Key::PageDown => t.move_cursor(page),
                        Key::Home => t.move_cursor(-(t.entries.len() as isize)),
                        _ => t.move_cursor(t.entries.len() as isize),
                    }
                    if shift {
                        // Shift+arrows mark the range we moved over (like TC).
                        let (lo, hi) = if t.cursor > start { (start, t.cursor - 1) } else { (t.cursor + 1, start) };
                        if t.cursor != start {
                            for i in lo..=hi {
                                t.toggle_mark(i);
                            }
                        }
                    }
                    None
                }
                (Key::Enter, false, false, false) => {
                    let cur = self.active_ref().tab().cursor;
                    self.open_entry(cur, ctx);
                    None
                }
                (Key::Enter, false, false, true) => Some(Cmd::Properties),
                (Key::Enter, true, false, false) => Some(Cmd::NameToCmdline),
                (Key::Backspace, false, false, false) => {
                    let t = self.active().tab_mut();
                    if t.quick_search.is_empty() {
                        t.go_up(h, d);
                    } else {
                        t.quick_search.pop();
                    }
                    None
                }
                (Key::PageUp, true, false, false) => Some(Cmd::Up),
                (Key::Home, true, false, false) => Some(Cmd::Home),
                (Key::Backslash, true, false, false) => Some(Cmd::Root),
                (Key::Escape, false, false, false) => {
                    let t = self.active().tab_mut();
                    if !t.quick_search.is_empty() {
                        t.quick_search.clear();
                    } else if !t.filter.is_empty() || t.filter_open {
                        t.filter.clear();
                        t.filter_open = false;
                        t.apply_view(d);
                    } else {
                        t.marked.clear();
                    }
                    None
                }
                (Key::Insert, false, false, false) | (Key::Space, false, false, false) => {
                    let t = self.active().tab_mut();
                    if key == Key::Space && !t.quick_search.is_empty() {
                        t.quick_search_push(" ");
                        continue;
                    }
                    let i = t.cursor;
                    if let Some(e) = t.entries.get(i).cloned()
                        && key == Key::Space
                        && e.is_dir
                        && !t.marked.contains(&e.name)
                    {
                        t.calc_dir_size(&e);
                    }
                    t.toggle_mark(i);
                    if key == Key::Insert {
                        t.move_cursor(1);
                    }
                    None
                }
                (Key::F2, false, false, false) | (Key::R, true, false, false) => Some(Cmd::Reload),
                (Key::F3, false, false, false) => Some(Cmd::View),
                (Key::F4, false, false, false) => Some(Cmd::Edit),
                (Key::F4, false, true, false) => Some(Cmd::NewFile),
                (Key::F5, false, false, false) => Some(Cmd::Copy),
                (Key::F5, false, true, false) => Some(Cmd::CopySameDir),
                (Key::F5, false, false, true) => Some(Cmd::Pack),
                (Key::F6, false, false, false) => Some(Cmd::Move),
                (Key::F6, false, true, false) => Some(Cmd::Rename),
                (Key::F7, false, false, false) => Some(Cmd::Mkdir),
                (Key::F7, false, false, true) => Some(Cmd::Search),
                (Key::F, true, false, false) => Some(Cmd::FtpConnect),
                (Key::F, true, true, false) => Some(Cmd::FtpDisconnect),
                (Key::F8 | Key::Delete, false, false, false) => Some(Cmd::Delete),
                (Key::F8 | Key::Delete, false, true, false) => Some(Cmd::DeletePermanent),
                // macOS: Cmd+Backspace like in Finder.
                (Key::Backspace, true, false, false) => Some(Cmd::Delete),
                (Key::F9, false, false, false) => Some(Cmd::Terminal),
                (Key::F9, false, false, true) => Some(Cmd::Unpack),
                (Key::F9, false, true, true) => Some(Cmd::UnpackSmart),
                (Key::F3, true, false, false) => Some(Cmd::SortName),
                (Key::F4, true, false, false) => Some(Cmd::SortExt),
                (Key::F5, true, false, false) => Some(Cmd::SortDate),
                (Key::F6, true, false, false) => Some(Cmd::SortSize),
                (Key::A, true, false, false) => Some(Cmd::SelectAll),
                (Key::A, true, true, false) => Some(Cmd::DeselectAll),
                (Key::M, true, false, false) => Some(Cmd::MultiRename),
                (Key::Q, true, false, false) => Some(Cmd::QuickView),
                (Key::S, true, false, false) => Some(Cmd::Filter),
                (Key::H, true, false, false) => Some(Cmd::ToggleHidden),
                (Key::T, true, false, false) => Some(Cmd::NewTab),
                (Key::W, true, false, false) => Some(Cmd::CloseTab),
                (Key::U, true, false, false) => Some(Cmd::Swap),
                (Key::D, true, false, false) => Some(Cmd::Hotlist),
                (Key::D, true, true, false) => Some(Cmd::AddHotlist),
                (Key::C, true, true, false) => Some(Cmd::CopyPaths),
                (Key::C, true, false, false) => Some(Cmd::ClipCopy),
                (Key::X, true, false, false) => Some(Cmd::ClipCut),
                (Key::V, true, false, false) => Some(Cmd::ClipPaste),
                (Key::N, true, true, false) => Some(Cmd::CopyNames),
                // "=" needs Shift on many layouts (e.g. German), so Shift is allowed here.
                (Key::Equals, true, _, false) => Some(Cmd::SameDir),
                (Key::G, true, false, false) => Some(Cmd::TakeOtherDir),
                (Key::B, true, true, false) => Some(Cmd::BranchView),
                (Key::Comma, true, false, false) => Some(Cmd::Settings),
                (Key::ArrowDown, false, false, true) => Some(Cmd::History),
                (Key::ArrowLeft, false, false, true) => Some(Cmd::Back),
                (Key::ArrowRight, false, false, true) => Some(Cmd::Forward),
                (Key::ArrowLeft, true, false, false) | (Key::ArrowRight, true, false, false) => {
                    // Open the dir under the cursor in the left/right panel.
                    let target_right = key == Key::ArrowRight;
                    if let Some(e) = self.active_ref().tab().current().cloned()
                        && e.is_dir
                        && let Some(dir) = self.active_ref().tab().loc.dir().map(Path::to_path_buf)
                    {
                        let p = if e.is_parent { dir.parent().map(Path::to_path_buf).unwrap_or(dir) } else { e.path };
                        let panel = if target_right { &mut self.right } else { &mut self.left };
                        panel.tab_mut().navigate(Location::Dir(p), h, d);
                    }
                    None
                }
                (Key::E, true, false, false) => {
                    self.focus_cmdline = true;
                    None
                }
                _ => None,
            };
            if let Some(cmd) = cmd {
                self.run(cmd, ctx);
            }
        }

        // Typed characters: quick search, or +/-/* selection shortcuts.
        for t in texts {
            let tab = self.active().tab_mut();
            if tab.quick_search.is_empty() {
                match t.as_str() {
                    " " => continue,
                    "+" => {
                        self.run(Cmd::SelectPattern, ctx);
                        continue;
                    }
                    "-" => {
                        self.run(Cmd::DeselectPattern, ctx);
                        continue;
                    }
                    "*" => {
                        self.run(Cmd::Invert, ctx);
                        continue;
                    }
                    _ => {}
                }
            }
            let tab = self.active().tab_mut();
            tab.quick_search_push(&t);
        }
    }

    fn open_entry(&mut self, idx: usize, ctx: &egui::Context) {
        let (h, d) = (self.cfg.show_hidden, self.cfg.dirs_first);
        let loc = &self.active_ref().tab().loc;
        if (loc.ftp().is_some() || matches!(loc, Location::Archive { .. }))
            && let Some(e) = self.active_ref().tab().entries.get(idx).cloned()
            && !e.is_dir
            && !e.is_link
            && !e.is_parent
        {
            self.active().tab_mut().cursor = idx;
            self.run(Cmd::OpenDefault, ctx);
            return;
        }
        if let Some(file) = self.active().tab_mut().enter(idx, h, d)
            && let Err(e) = fsutil::open_default(&file)
        {
            self.notify(e, true);
        }
    }

    // ------------------------------------------------------------------------
    // UI pieces
    // ------------------------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut egui::Ui) -> Option<Cmd> {
        let mut cmd = None;
        egui::MenuBar::new().ui(ui, |ui| {
            let mut item = |ui: &mut egui::Ui, label: &str, sc: &str, c: Cmd| {
                if ui.add(egui::Button::new(label).shortcut_text(sc)).clicked() {
                    cmd = Some(c);
                    ui.close();
                }
            };
            ui.menu_button("Dateien", |ui| {
                item(ui, "Ansehen", "F3", Cmd::View);
                item(ui, "Bearbeiten", "F4", Cmd::Edit);
                item(ui, "Neue Datei", "Shift+F4", Cmd::NewFile);
                item(ui, "Öffnen mit Standardprogramm", "", Cmd::OpenDefault);
                ui.separator();
                item(ui, "Ausschneiden", "Strg+X", Cmd::ClipCut);
                item(ui, "In Zwischenablage kopieren", "Strg+C", Cmd::ClipCopy);
                item(ui, "Einfügen", "Strg+V", Cmd::ClipPaste);
                ui.separator();
                item(ui, "Kopieren nach…", "F5", Cmd::Copy);
                item(ui, "Verschieben nach…", "F6", Cmd::Move);
                item(ui, "Umbenennen", "Shift+F6", Cmd::Rename);
                item(ui, "Mehrfach-Umbenennen", "Strg+M", Cmd::MultiRename);
                item(ui, "Löschen (Papierkorb)", "F8", Cmd::Delete);
                item(ui, "Endgültig löschen", "Shift+F8", Cmd::DeletePermanent);
                ui.separator();
                item(ui, "Packen (ZIP, 7z, TAR …)", "Alt+F5", Cmd::Pack);
                item(ui, "Entpacken nach…", "Alt+F9", Cmd::Unpack);
                item(ui, "Hier entpacken", "", Cmd::UnpackHere);
                item(ui, "Smart hier entpacken", "Alt+Shift+F9", Cmd::UnpackSmart);
                ui.separator();
                item(ui, "Dateien vergleichen (Inhalt)", "", Cmd::CompareFiles);
                item(ui, "Eigenschaften", "Alt+Enter", Cmd::Properties);
                item(ui, "Beenden", "", Cmd::Exit);
            });
            ui.menu_button("Markieren", |ui| {
                item(ui, "Gruppe markieren", "+", Cmd::SelectPattern);
                item(ui, "Gruppe abwählen", "-", Cmd::DeselectPattern);
                item(ui, "Alles markieren", "Strg+A", Cmd::SelectAll);
                item(ui, "Alles abwählen", "Strg+Shift+A", Cmd::DeselectAll);
                item(ui, "Markierung umkehren", "*", Cmd::Invert);
                ui.separator();
                item(ui, "Pfade kopieren", "Strg+Shift+C", Cmd::CopyPaths);
                item(ui, "Namen kopieren", "Strg+Shift+N", Cmd::CopyNames);
                ui.separator();
                item(ui, "Ordner vergleichen", "", Cmd::CompareDirs);
            });
            ui.menu_button("Befehle", |ui| {
                item(ui, "Dateien suchen", "Alt+F7", Cmd::Search);
                item(ui, "Neuer Ordner", "F7", Cmd::Mkdir);
                item(ui, "Verzeichnisse synchronisieren…", "", Cmd::SyncDirs);
                item(ui, "Terminal hier öffnen", "F9", Cmd::Terminal);
                ui.separator();
                item(ui, "Ordner-Favoriten", "Strg+D", Cmd::Hotlist);
                item(ui, "Zu Favoriten hinzufügen", "Strg+Shift+D", Cmd::AddHotlist);
                item(ui, "Verlauf", "Alt+↓", Cmd::History);
                ui.separator();
                item(ui, "Panels tauschen", "Strg+U", Cmd::Swap);
                item(ui, "Ordner vom anderen Panel übernehmen", "Strg+G", Cmd::TakeOtherDir);
                item(ui, "Ziel = Quelle (anderes Panel hierher)", "Strg+=", Cmd::SameDir);
            });
            ui.menu_button("Netz", |ui| {
                item(ui, "Server verbinden (FTP/SFTP)…", "Strg+F", Cmd::FtpConnect);
                item(ui, "Verbindung trennen", "Strg+Shift+F", Cmd::FtpDisconnect);
            });
            ui.menu_button("Ansicht", |ui| {
                item(ui, "Schnellansicht", "Strg+Q", Cmd::QuickView);
                item(ui, "Schnellfilter", "Strg+S", Cmd::Filter);
                item(ui, "Versteckte Dateien", "Strg+H", Cmd::ToggleHidden);
                item(ui, "Branch-View (alle Unterordner)", "Strg+Shift+B", Cmd::BranchView);
                ui.menu_button("Laufwerksleiste", |ui| {
                    use crate::config::DriveBar;
                    for (mode, label) in [
                        (DriveBar::Both, "Knöpfe + Dropdown"),
                        (DriveBar::Buttons, "Nur Knöpfe"),
                        (DriveBar::Dropdown, "Nur Dropdown"),
                    ] {
                        if ui.radio_value(&mut self.cfg.drive_bar, mode, label).clicked() {
                            ui.close();
                        }
                    }
                });
                ui.separator();
                item(ui, "Nach Name", "Strg+F3", Cmd::SortName);
                item(ui, "Nach Erweiterung", "Strg+F4", Cmd::SortExt);
                item(ui, "Nach Datum", "Strg+F5", Cmd::SortDate);
                item(ui, "Nach Größe", "Strg+F6", Cmd::SortSize);
                ui.separator();
                item(ui, "Neuer Tab", "Strg+T", Cmd::NewTab);
                item(ui, "Tab schließen", "Strg+W", Cmd::CloseTab);
                item(ui, "Neu einlesen", "Strg+R", Cmd::Reload);
            });
            ui.menu_button("Konfiguration", |ui| {
                item(ui, "Einstellungen…", "Strg+,", Cmd::Settings);
            });
            ui.menu_button("Hilfe", |ui| {
                item(ui, "Tastenkürzel", "", Cmd::Keys);
                item(ui, "Über", "", Cmd::About);
            });
        });
        cmd
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) -> Option<Cmd> {
        let mut cmd = None;
        ui.horizontal(|ui| {
            let mut b = |ui: &mut egui::Ui, icon: &str, tip: &str, c: Cmd| {
                if ui.add(egui::Button::new(RichText::new(icon).size(16.0)).frame(false)).on_hover_text(tip).clicked() {
                    cmd = Some(c);
                }
            };
            b(ui, "🔄", "Neu einlesen (Strg+R)", Cmd::Reload);
            ui.separator();
            b(ui, "⬅", "Zurück (Alt+←)", Cmd::Back);
            b(ui, "➡", "Vor (Alt+→)", Cmd::Forward);
            b(ui, "⬆", "Übergeordneter Ordner (Backspace)", Cmd::Up);
            ui.separator();
            b(ui, "👁", "Schnellansicht (Strg+Q)", Cmd::QuickView);
            b(ui, "🕶", "Versteckte Dateien (Strg+H)", Cmd::ToggleHidden);
            b(ui, "⛃", "Schnellfilter (Strg+S)", Cmd::Filter);
            ui.separator();
            b(ui, "🔍", "Dateien suchen (Alt+F7)", Cmd::Search);
            b(ui, "✏", "Mehrfach-Umbenennen (Strg+M)", Cmd::MultiRename);
            b(ui, "⚖", "Ordner vergleichen", Cmd::CompareDirs);
            b(ui, "🔃", "Verzeichnisse synchronisieren", Cmd::SyncDirs);
            b(ui, "📦", "Packen (Alt+F5)", Cmd::Pack);
            b(ui, "📂", "Entpacken (Alt+F9)", Cmd::Unpack);
            ui.separator();
            b(ui, "🌐", "Server verbinden – FTP/SFTP (Strg+F)", Cmd::FtpConnect);
            ui.separator();
            b(ui, "⭐", "Favoriten (Strg+D)", Cmd::Hotlist);
            b(ui, "🕘", "Verlauf (Alt+↓)", Cmd::History);
            b(ui, "↔", "Panels tauschen (Strg+U)", Cmd::Swap);
            b(ui, "🖳", "Terminal (F9)", Cmd::Terminal);
            ui.separator();
            b(ui, "⚙", "Einstellungen", Cmd::Settings);
        });
        cmd
    }

    fn fkey_bar(&mut self, ui: &mut egui::Ui) -> Option<Cmd> {
        let mut cmd = None;
        let buttons = [
            ("F3 Ansehen", Cmd::View),
            ("F4 Bearbeiten", Cmd::Edit),
            ("F5 Kopieren", Cmd::Copy),
            ("F6 Verschieben", Cmd::Move),
            ("F7 Neuer Ordner", Cmd::Mkdir),
            ("F8 Löschen", Cmd::Delete),
            ("F9 Terminal", Cmd::Terminal),
            ("Beenden", Cmd::Exit),
        ];
        ui.columns(buttons.len(), |cols| {
            for (i, (label, c)) in buttons.iter().enumerate() {
                if cols[i].add_sized([cols[i].available_width(), 22.0], egui::Button::new(*label)).clicked() {
                    cmd = Some(*c);
                }
            }
        });
        cmd
    }

    /// Everything currently running in the background, for the status corner.
    fn busy_reasons(&self) -> Vec<String> {
        let mut v = Vec::new();
        let tabs = [self.left.tab(), self.right.tab()];
        if tabs.iter().any(|t| t.loading) {
            v.push("Lade Verzeichnis".to_string());
        }
        if tabs.iter().any(|t| !t.sizing.is_empty()) {
            v.push("Berechne Ordnergröße".into());
        }
        if self.ftp_connecting.is_some() || self.smb_mounting.is_some() {
            v.push("Verbinde mit Server".into());
        }
        if !self.ftp_ops.is_empty() {
            v.push("Server-Aktion".into());
        }
        if let Some(j) = &self.job {
            v.push(format!("{} läuft", j.kind.title()));
        }
        if self.search.is_running() {
            v.push("Suche läuft".into());
        }
        v
    }

    fn command_line(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy_reasons();
        ui.horizontal(|ui| {
            let dir = self.active_dir();
            ui.label(RichText::new(format!("{}>", dir.to_string_lossy())).monospace());
            let id = egui::Id::new("cmdline");
            let r = ui
                .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !busy.is_empty() {
                        ui.label(RichText::new(format!("{}…", busy.join(" · "))).small());
                        ui.spinner();
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cmdline)
                            .id(id)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("Befehl (Enter = ausführen, cd <pfad> wechselt Ordner)")
                            .desired_width(f32::INFINITY),
                    )
                })
                .inner;
            if self.focus_cmdline {
                r.request_focus();
                self.focus_cmdline = false;
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                let line = std::mem::take(&mut self.cmdline);
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("cd") .filter(|r| r.is_empty() || r.starts_with(' ')) {
                    let rest = rest.trim();
                    let target = if rest.is_empty() {
                        dirs::home_dir().unwrap_or_else(|| "/".into())
                    } else {
                        let p = PathBuf::from(panel::expand_tilde(rest));
                        if p.is_absolute() { p } else { dir.join(p) }
                    };
                    match target.canonicalize() {
                        Ok(p) if p.is_dir() => self.navigate_active(Location::Dir(p)),
                        _ => self.notify(format!("Ordner nicht gefunden: {}", target.to_string_lossy()), true),
                    }
                } else if !line.is_empty() {
                    match fsutil::run_shell(line, &dir) {
                        Ok(()) => self.notify(format!("Gestartet: {line}"), false),
                        Err(e) => self.notify(e, true),
                    }
                }
            }
        });
    }

    fn job_window(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        let (finished, errors) = {
            let p = job.progress.lock().unwrap();
            (p.finished, p.errors.clone())
        };
        if finished {
            let title = job.kind.title();
            self.job = None;
            self.reload_all();
            let after = self.after_job.take();
            if self.sync.open && !self.sync.is_scanning() {
                self.sync.start_scan(); // show the result of a synchronisation
            }
            if errors.is_empty() {
                match after {
                    Some(AfterJob::View(p)) => self.open_viewer(&p),
                    Some(AfterJob::Open(p)) => {
                        if let Err(e) = fsutil::open_default(&p) {
                            self.notify(e, true);
                        }
                    }
                    Some(AfterJob::Edit { local, conn, remote }) => {
                        match fsutil::open_with(&self.cfg.editor, &local) {
                            Ok(()) => {
                                let mtime = std::fs::metadata(&local).and_then(|m| m.modified()).ok();
                                self.ftp_edits.retain(|e| e.local != local);
                                self.ftp_edits.push(FtpEdit { local, conn, remote, mtime });
                                self.notify("Nach dem Speichern wird angeboten, die Datei wieder hochzuladen", false);
                            }
                            Err(e) => self.notify(format!("Editor: {e}"), true),
                        }
                    }
                    None => self.notify(format!("{title}: fertig"), false),
                }
            } else {
                self.open_dialog(Dialog::Message {
                    title: format!("{title}: {} Fehler", errors.len()),
                    text: errors.join("\n"),
                });
            }
            return;
        }
        let job = self.job.as_ref().unwrap();
        let p = job.progress.lock().unwrap();
        let question = p.question.clone();
        egui::Window::new(job.kind.title())
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let counting = p.files_total == 0 && p.bytes_total == 0;
                if counting {
                    // The worker first walks the sources to know the totals.
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Dateien werden ermittelt…");
                    });
                } else {
                    ui.add(egui::Label::new(RichText::new(&p.current).small()).truncate());
                }
                let frac = if p.bytes_total > 0 {
                    p.bytes_done as f32 / p.bytes_total as f32
                } else if p.files_total > 0 {
                    p.files_done as f32 / p.files_total as f32
                } else {
                    0.0
                };
                // Animated bar: visibly alive even when one big file takes a while.
                ui.add(egui::ProgressBar::new(frac).show_percentage().animate(true));
                let secs = job.started.elapsed().as_secs();
                let speed = if secs > 0 { p.bytes_done / secs } else { 0 };
                ui.label(format!(
                    "{} / {} Dateien · {} / {} · {}:{:02} · {}/s",
                    p.files_done,
                    p.files_total,
                    fsutil::format_size_short(p.bytes_done),
                    fsutil::format_size_short(p.bytes_total),
                    secs / 60,
                    secs % 60,
                    fsutil::format_size_short(speed)
                ));
                if ui.button("Abbrechen").clicked() {
                    job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = job.answer_tx.send(crate::ops::OverwriteAnswer::Cancel);
                }
            });
        drop(p);
        if let Some((src, dst)) = question {
            crate::dialogs::overwrite_modal(ctx, &src, &dst, &job.answer_tx);
        }
    }

    fn handle_panel_actions(&mut self, actions: Vec<PanelAction>, right: bool, ctx: &egui::Context) {
        let (h, d) = (self.cfg.show_hidden, self.cfg.dirs_first);
        for a in actions {
            self.right_active = right;
            match a {
                PanelAction::Activate => {}
                PanelAction::CancelLoading => self.active().tab_mut().cancel_loading(h, d),
                PanelAction::Open(i) => self.open_entry(i, ctx),
                PanelAction::Navigate(loc) => self.active().tab_mut().navigate(loc, h, d),
                PanelAction::Cmd(Cmd::OpenDefault) => {
                    // "Öffnen" in the menu = Enter (folders, archives, remote files).
                    let i = self.active_ref().tab().cursor;
                    self.open_entry(i, ctx);
                }
                PanelAction::Cmd(c) => self.run(c, ctx),
                PanelAction::NewTab => self.run(Cmd::NewTab, ctx),
                PanelAction::CloseTab(i) => {
                    let p = self.active();
                    if p.tabs.len() > 1 {
                        p.tabs.remove(i);
                        if p.active >= i && p.active > 0 {
                            p.active -= 1;
                        }
                    }
                }
            }
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        if self.applied_theme == Some(self.cfg.theme) {
            return;
        }
        self.applied_theme = Some(self.cfg.theme);
        ctx.set_theme(match self.cfg.theme {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
        });
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let (files, pos) = ctx.input(|i| {
            (
                i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect::<Vec<_>>(),
                i.pointer.hover_pos(),
            )
        });
        if files.is_empty() {
            return;
        }
        if let Some(p) = pos {
            if self.panel_rects[0].contains(p) {
                self.right_active = false;
            } else if self.panel_rects[1].contains(p) {
                self.right_active = true;
            }
        }
        let loc = self.active_ref().tab().loc.clone();
        if let Some(dir) = loc.dir() {
            self.open_dialog(Dialog::CopyMove {
                is_move: false,
                sources: files,
                target: format!("{}/", dir.to_string_lossy()),
                mode: CopyMode::Local,
            });
        } else if let Some((id, path)) = loc.ftp() {
            self.open_dialog(Dialog::CopyMove {
                is_move: false,
                sources: files,
                target: format!("{}/", path.trim_end_matches('/')),
                mode: CopyMode::Upload { conn: id },
            });
        }
    }
}

impl eframe::App for MieryApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if !self.keyboard_for_panels(ctx) {
            return;
        }
        // Take keyboard input away from egui so Tab/arrows drive the panels
        // instead of moving widget focus.
        // Ctrl (Cmd on macOS) is implied by these events; Shift tells Ctrl+Shift+C apart.
        let mut mods = Modifiers::COMMAND;
        mods.shift = ctx.input(|i| i.modifiers.shift);
        raw.events.retain(|e| match e {
            // The windowing layer turns Ctrl+C/X/V into these events.
            Event::Copy => {
                self.keys.push((Key::C, mods));
                false
            }
            Event::Cut => {
                self.keys.push((Key::X, mods));
                false
            }
            Event::Paste(_) => {
                self.keys.push((Key::V, mods));
                false
            }
            // …and may also send the key itself: ignore that to avoid doing it twice.
            Event::Key { key: Key::C | Key::X | Key::V, modifiers, .. } if modifiers.command => false,
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                self.keys.push((*key, *modifiers));
                false
            }
            Event::Text(t) => {
                self.texts.push(t.clone());
                false
            }
            _ => true,
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Panel sizes are only known after a frame; settle the layout right away.
        if self.startup_frames < 3 {
            self.startup_frames += 1;
            ctx.request_repaint();
        }
        self.apply_theme(&ctx);
        self.handle_keys(&ctx);
        self.handle_dropped_files(&ctx);
        self.poll_ftp(&ctx);

        let (h, d) = (self.cfg.show_hidden, self.cfg.dirs_first);
        self.left.tab_mut().poll_changes(h, d);
        self.right.tab_mut().poll_changes(h, d);

        let mut cmds = Vec::new();
        egui::Panel::top("menu").show(ui, |ui| {
            cmds.extend(self.menu_bar(ui));
        });
        egui::Panel::top("toolbar").show(ui, |ui| {
            cmds.extend(self.toolbar(ui));
        });
        egui::Panel::bottom("fkeys").show(ui, |ui| {
            ui.add_space(2.0);
            cmds.extend(self.fkey_bar(ui));
            ui.add_space(2.0);
        });
        egui::Panel::bottom("cmdline").show(ui, |ui| {
            self.command_line(ui);
            if let Some((msg, at, err)) = &self.toast {
                if at.elapsed().as_secs() < 5 {
                    let color = if *err { egui::Color32::from_rgb(200, 60, 60) } else { ui.visuals().weak_text_color() };
                    ui.add(egui::Label::new(RichText::new(msg).color(color).small()).truncate());
                    ctx.request_repaint_after(std::time::Duration::from_secs(1));
                } else {
                    self.toast = None;
                }
            }
        });

        let cut = self.cut_paths();
        let mut left_actions = Vec::new();
        let mut right_actions = Vec::new();
        egui::CentralPanel::default().show(ui, |ui| {
            let rects = ui.columns(2, |cols| {
                let qv_left = self.quick_view && self.right_active;
                let qv_right = self.quick_view && !self.right_active;
                for (i, col) in cols.iter_mut().enumerate() {
                    let is_right = i == 1;
                    let show_qv = if is_right { qv_right } else { qv_left };
                    if show_qv {
                        let src = if is_right { &self.left } else { &self.right };
                        let cur = src.tab().current().cloned();
                        let in_archive = src.tab().loc.dir().is_none();
                        let (target, info) = match &cur {
                            Some(e) if e.is_dir => (
                                None,
                                Some(format!(
                                    "📁 {}\n{}",
                                    e.path.to_string_lossy(),
                                    src.tab()
                                        .dir_sizes
                                        .get(&e.name)
                                        .map(|s| format!("Größe: {}", fsutil::format_size(*s)))
                                        .unwrap_or_else(|| "Leertaste/F3 berechnet die Größe".into())
                                )),
                            ),
                            Some(_) if in_archive => (None, Some("Vorschau in Archiven nicht verfügbar".into())),
                            Some(e) => (Some(e.path.clone()), None),
                            None => (None, None),
                        };
                        self.qv.show(col, target.as_deref(), info);
                    } else if is_right {
                        right_actions = panel::show_panel(col, &mut self.right, "right", self.right_active, d, self.cfg.drive_bar, &cut);
                    } else {
                        left_actions = panel::show_panel(col, &mut self.left, "left", !self.right_active, d, self.cfg.drive_bar, &cut);
                    }
                }
                [cols[0].min_rect(), cols[1].min_rect()]
            });
            self.panel_rects = rects;
        });
        self.handle_panel_actions(left_actions, false, &ctx);
        self.handle_panel_actions(right_actions, true, &ctx);

        for c in cmds {
            self.run(c, &ctx);
        }

        for v in &mut self.viewers {
            v.show(&ctx);
        }
        self.viewers.retain(|v| v.open);
        for c in &mut self.compares {
            c.show(&ctx, &self.cfg.editor);
        }
        if let Some(crate::sync::SyncRequest::Run(ops)) = self.sync.show(&ctx, self.job.is_some()) {
            let kind = JobKind::Sync { ops, to_trash: self.cfg.delete_to_trash };
            self.start_job(kind, Vec::new(), PathBuf::new(), &ctx);
        }
        self.compares.retain(|c| c.open);

        match self.search.show(&ctx) {
            Some(SearchAction::GoTo(p)) => self.goto_file(&p),
            Some(SearchAction::View(p)) => self.open_viewer(&p),
            None => {}
        }
        if self.rename.show(&ctx) {
            self.reload_all();
        }

        self.job_window(&ctx);
        self.show_dialog(&ctx);

        if self.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        } else {
            // Keep polling for directory changes.
            ctx.request_repaint_after(std::time::Duration::from_millis(1500));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.cfg.left_tabs = self.left.tab_paths();
        self.cfg.right_tabs = self.right.tab_paths();
        self.cfg.left_active = self.left.active;
        self.cfg.right_active = self.right.active;
        eframe::set_value(storage, STORAGE_KEY, &self.cfg);
    }
}

//! Synchronize directories (like Total Commander's "Synchronize dirs").
//!
//! Both trees are scanned in the background, every file gets a state and a
//! suggested action that the user can change; the result is a list of copy /
//! delete operations executed as a normal background job.

use crate::fsutil;
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    LeftOnly,
    RightOnly,
    LeftNewer,
    RightNewer,
    Equal,
    /// Same date, different content/size.
    Differs,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    None,
    ToRight,
    ToLeft,
    DeleteRight,
    DeleteLeft,
}

impl Action {
    pub fn symbol(self) -> &'static str {
        match self {
            Action::None => "  ",
            Action::ToRight => "➡",
            Action::ToLeft => "⬅",
            Action::DeleteRight => "🗑 R",
            Action::DeleteLeft => "L 🗑",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    /// Path relative to both roots, '/'-separated.
    pub rel: String,
    pub is_dir: bool,
    pub left: Option<Meta>,
    pub right: Option<Meta>,
    pub state: State,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub subdirs: bool,
    pub by_content: bool,
    pub ignore_date: bool,
    pub empty_dirs: bool,
    /// One-way: make the right side a mirror of the left (also deletes there).
    pub asymmetric: bool,
    pub mask: String,
}

impl Default for Options {
    fn default() -> Self {
        Options { subdirs: true, by_content: false, ignore_date: false, empty_dirs: true, asymmetric: false, mask: "*".into() }
    }
}

/// A planned operation.
#[derive(Clone, Debug)]
pub enum Op {
    Copy { from: PathBuf, to: PathBuf, is_dir: bool },
    Delete(PathBuf),
}

/// All files (and folders) below `root`, keyed by relative path.
fn collect(root: &Path, opts: &Options, mask: &regex::Regex) -> std::io::Result<BTreeMap<String, (bool, Meta)>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    let mut first = true;
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if first => return Err(e),
            Err(_) => continue,
        };
        first = false;
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue; // don't follow links into other trees
            }
            let path = e.path();
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            let Ok(m) = e.metadata() else { continue };
            if ft.is_dir() {
                if opts.subdirs {
                    out.insert(rel, (true, Meta { size: 0, mtime: m.modified().ok() }));
                    stack.push(path);
                }
            } else if mask.is_match(&e.file_name().to_string_lossy()) {
                out.insert(rel, (false, Meta { size: m.len(), mtime: m.modified().ok() }));
            }
        }
    }
    Ok(out)
}

fn newer(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<std::cmp::Ordering> {
    let (a, b) = (a?, b?);
    // FAT/SMB round to 2 s: treat that as equal.
    let diff = match a.duration_since(b) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    };
    Some(if diff > 2.0 {
        std::cmp::Ordering::Greater
    } else if diff < -2.0 {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Equal
    })
}

fn default_action(state: State, opts: &Options) -> Action {
    match (state, opts.asymmetric) {
        (State::LeftOnly | State::LeftNewer, _) => Action::ToRight,
        (State::RightOnly, true) => Action::DeleteRight,
        (State::RightNewer, true) | (State::Differs, true) => Action::ToRight,
        (State::RightOnly | State::RightNewer, false) => Action::ToLeft,
        (State::Equal, _) | (State::Differs, false) => Action::None,
    }
}

pub fn scan(left: &Path, right: &Path, opts: &Options) -> Result<Vec<Item>, String> {
    let mask = fsutil::wildcard_regex(&opts.mask).ok_or_else(|| l!("Ungültige Dateimaske", "Invalid file mask").to_string())?;
    let l = collect(left, opts, &mask).map_err(|e| format!("{}: {e}", left.display()))?;
    let r = collect(right, opts, &mask).map_err(|e| format!("{}: {e}", right.display()))?;
    let mut keys: Vec<&String> = l.keys().chain(r.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut items = Vec::new();
    for rel in keys {
        let (lm, rm) = (l.get(rel), r.get(rel));
        let is_dir = lm.or(rm).is_some_and(|(d, _)| *d);
        if is_dir {
            // Folders only matter when they exist on one side only (empty dirs).
            if !opts.empty_dirs || (lm.is_some() && rm.is_some()) {
                continue;
            }
            let prefix = format!("{rel}/");
            let has_files = |m: &BTreeMap<String, (bool, Meta)>| m.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).next().is_some();
            // A folder with content is created by copying its files anyway.
            if (lm.is_some() && has_files(&l)) || (rm.is_some() && has_files(&r)) {
                continue;
            }
        }
        let state = match (lm, rm) {
            (Some(_), None) => State::LeftOnly,
            (None, Some(_)) => State::RightOnly,
            (Some((_, a)), Some((_, b))) => {
                let same_size = a.size == b.size;
                let order = if opts.ignore_date { Some(std::cmp::Ordering::Equal) } else { newer(a.mtime, b.mtime) };
                let content_equal = || {
                    same_size
                        && crate::compare::files_identical(&left.join(rel), &right.join(rel))
                };
                match order {
                    Some(std::cmp::Ordering::Equal) if same_size && !opts.by_content => State::Equal,
                    _ if opts.by_content && content_equal() => State::Equal,
                    Some(std::cmp::Ordering::Greater) => State::LeftNewer,
                    Some(std::cmp::Ordering::Less) => State::RightNewer,
                    _ => State::Differs,
                }
            }
            (None, None) => continue,
        };
        let action = default_action(state, opts);
        items.push(Item {
            rel: rel.clone(),
            is_dir,
            left: lm.map(|(_, m)| *m),
            right: rm.map(|(_, m)| *m),
            state,
            action,
        });
    }
    Ok(items)
}

/// Operations for all items with an action.
pub fn plan(left: &Path, right: &Path, items: &[Item]) -> Vec<Op> {
    items
        .iter()
        .filter_map(|it| {
            let (l, r) = (left.join(&it.rel), right.join(&it.rel));
            match it.action {
                Action::ToRight if it.left.is_some() => Some(Op::Copy { from: l, to: r, is_dir: it.is_dir }),
                Action::ToLeft if it.right.is_some() => Some(Op::Copy { from: r, to: l, is_dir: it.is_dir }),
                Action::DeleteRight if it.right.is_some() => Some(Op::Delete(r)),
                Action::DeleteLeft if it.left.is_some() => Some(Op::Delete(l)),
                _ => None,
            }
        })
        .collect()
}

/// Which actions make sense for an item; clicking cycles through them.
fn choices(it: &Item) -> Vec<Action> {
    let mut v = Vec::new();
    if it.left.is_some() {
        v.push(Action::ToRight);
    }
    if it.right.is_some() {
        v.push(Action::ToLeft);
    }
    if it.left.is_none() {
        v.push(Action::DeleteRight);
    }
    if it.right.is_none() {
        v.push(Action::DeleteLeft);
    }
    v.push(Action::None);
    v
}

pub struct SyncWindow {
    pub open: bool,
    pub left: String,
    pub right: String,
    pub opts: Options,
    pub items: Vec<Item>,
    scan_rx: Option<Receiver<Result<Vec<Item>, String>>>,
    scan_started: std::time::Instant,
    scanned_for: Option<(PathBuf, PathBuf)>,
    error: Option<String>,
    show: [bool; 4], // → only/newer left, = equal, ≠ differs, ← right
}

pub enum SyncRequest {
    /// Run these operations as a job, then rescan.
    Run(Vec<Op>),
}

impl SyncWindow {
    pub fn new() -> Self {
        SyncWindow {
            open: false,
            left: String::new(),
            right: String::new(),
            opts: Options::default(),
            items: Vec::new(),
            scan_rx: None,
            scan_started: std::time::Instant::now(),
            scanned_for: None,
            error: None,
            show: [true, false, true, true],
        }
    }

    pub fn open_with(&mut self, left: &Path, right: &Path) {
        self.open = true;
        self.left = left.to_string_lossy().into_owned();
        self.right = right.to_string_lossy().into_owned();
        self.start_scan();
    }

    pub fn is_scanning(&self) -> bool {
        self.scan_rx.is_some()
    }

    pub fn start_scan(&mut self) {
        let (l, r) = (PathBuf::from(&self.left), PathBuf::from(&self.right));
        let opts = self.opts.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.scanned_for = Some((l.clone(), r.clone()));
        std::thread::spawn(move || {
            let _ = tx.send(scan(&l, &r, &opts));
            fsutil::wake_ui();
        });
        self.scan_rx = Some(rx);
        self.scan_started = std::time::Instant::now();
        self.error = None;
    }

    fn visible(&self, it: &Item) -> bool {
        match it.state {
            State::LeftOnly | State::LeftNewer => self.show[0],
            State::Equal => self.show[1],
            State::Differs => self.show[2],
            State::RightOnly | State::RightNewer => self.show[3],
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, job_running: bool) -> Option<SyncRequest> {
        if !self.open {
            return None;
        }
        if let Some(rx) = &self.scan_rx
            && let Ok(res) = rx.try_recv()
        {
            self.scan_rx = None;
            match res {
                Ok(items) => self.items = items,
                Err(e) => {
                    self.items.clear();
                    self.error = Some(e);
                }
            }
        }
        let mut request = None;
        let mut open = self.open;
        egui::Window::new(l!("Verzeichnisse synchronisieren", "Synchronize folders"))
            .open(&mut open)
            .default_size([1100.0, 640.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(l!("Links:", "Left:"));
                    ui.add(egui::TextEdit::singleline(&mut self.left).desired_width(400.0));
                    if ui.button("↔").on_hover_text(l!("Seiten tauschen", "Swap sides")).clicked() {
                        std::mem::swap(&mut self.left, &mut self.right);
                        self.start_scan();
                    }
                    ui.label(l!("Rechts:", "Right:"));
                    ui.add(egui::TextEdit::singleline(&mut self.right).desired_width(f32::INFINITY));
                });
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.opts.subdirs, l!("Unterordner", "Subfolders"));
                    ui.checkbox(&mut self.opts.by_content, l!("Nach Inhalt vergleichen", "Compare by content"));
                    ui.checkbox(&mut self.opts.ignore_date, l!("Datum ignorieren", "Ignore date"));
                    ui.checkbox(&mut self.opts.empty_dirs, l!("Leere Ordner", "Empty folders"));
                    ui.checkbox(&mut self.opts.asymmetric, l!("Asymmetrisch (rechts = Spiegel von links)", "Asymmetric (right = mirror of left)"))
                        .on_hover_text(l!("Rechts wird an links angeglichen – auch Löschen von Dateien, die links fehlen", "The right side is made equal to the left – including deleting files missing on the left"));
                    ui.label(l!("Maske:", "Mask:"));
                    ui.add(egui::TextEdit::singleline(&mut self.opts.mask).desired_width(90.0));
                    if self.is_scanning() {
                        ui.spinner();
                        ui.label(lf!("Vergleiche… {:.0} s", "Comparing… {:.0} s", self.scan_started.elapsed().as_secs_f32().floor()));
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                    } else if ui.button(RichText::new(l!("🔄 Vergleichen", "🔄 Compare")).strong()).clicked() {
                        self.start_scan();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(l!("Zeigen:", "Show:"));
                    let count = |f: &dyn Fn(State) -> bool| self.items.iter().filter(|i| f(i.state)).count();
                    let labels = [
                        lf!("➡ links neuer/nur links ({})", "➡ left newer/left only ({})", count(&|s| matches!(s, State::LeftOnly | State::LeftNewer))),
                        lf!("= gleich ({})", "= equal ({})", count(&|s| s == State::Equal)),
                        lf!("≠ ungleich ({})", "≠ different ({})", count(&|s| s == State::Differs)),
                        lf!("⬅ rechts neuer/nur rechts ({})", "⬅ right newer/right only ({})", count(&|s| matches!(s, State::RightOnly | State::RightNewer))),
                    ];
                    for (i, l) in labels.iter().enumerate() {
                        ui.toggle_value(&mut self.show[i], l);
                    }
                });
                if let Some(e) = &self.error {
                    ui.colored_label(Color32::from_rgb(200, 60, 60), e);
                }
                ui.separator();

                let rows: Vec<usize> = (0..self.items.len()).filter(|&i| self.visible(&self.items[i])).collect();
                let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
                let table_h = (ui.available_height() - 60.0).max(120.0);
                let mut clicked_action: Option<usize> = None;
                TableBuilder::new(ui)
                    .id_salt("sync_table")
                    .striped(true)
                    .column(Column::remainder().at_least(220.0).clip(true))
                    .column(Column::exact(90.0))
                    .column(Column::exact(120.0))
                    .column(Column::exact(56.0))
                    .column(Column::exact(120.0))
                    .column(Column::exact(90.0))
                    .min_scrolled_height(table_h)
                    .max_scroll_height(table_h)
                    .auto_shrink([false, false])
                    .header(row_h, |mut h| {
                        for t in [l!("Datei", "File"), l!("Größe links", "Size left"), l!("Datum links", "Date left"), l!("Aktion", "Action"), l!("Datum rechts", "Date right"), l!("Größe rechts", "Size right")] {
                            h.col(|ui| {
                                ui.strong(t);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(row_h, rows.len(), |mut row| {
                            let idx = rows[row.index()];
                            let it = &self.items[idx];
                            let name = if it.is_dir { format!("📁 {}/", it.rel) } else { it.rel.clone() };
                            let meta = |m: Option<Meta>, size: bool| match m {
                                Some(m) if size => if it.is_dir { "<DIR>".into() } else { fsutil::format_size(m.size) },
                                Some(m) => fsutil::format_time(m.mtime),
                                None => String::new(),
                            };
                            row.col(|ui| {
                                ui.add(egui::Label::new(name).truncate());
                            });
                            row.col(|ui| {
                                ui.label(meta(it.left, true));
                            });
                            row.col(|ui| {
                                ui.label(meta(it.left, false));
                            });
                            row.col(|ui| {
                                let color = match it.action {
                                    Action::ToRight | Action::ToLeft => Color32::from_rgb(60, 160, 80),
                                    Action::DeleteLeft | Action::DeleteRight => Color32::from_rgb(210, 60, 60),
                                    Action::None => ui.visuals().weak_text_color(),
                                };
                                let text = if it.action == Action::None {
                                    match it.state {
                                        State::Equal => "=",
                                        State::Differs => "≠",
                                        _ => "·",
                                    }
                                } else {
                                    it.action.symbol()
                                };
                                if ui
                                    .add(egui::Button::new(RichText::new(text).strong().color(color)).frame(false))
                                    .on_hover_text(l!("Klicken zum Ändern der Aktion", "Click to change the action"))
                                    .clicked()
                                {
                                    clicked_action = Some(idx);
                                }
                            });
                            row.col(|ui| {
                                ui.label(meta(it.right, false));
                            });
                            row.col(|ui| {
                                ui.label(meta(it.right, true));
                            });
                        });
                    });
                if let Some(i) = clicked_action {
                    let it = &mut self.items[i];
                    let c = choices(it);
                    let pos = c.iter().position(|a| *a == it.action).map(|p| (p + 1) % c.len()).unwrap_or(0);
                    it.action = c[pos];
                }
                ui.separator();
                let (l, r) = (PathBuf::from(&self.left), PathBuf::from(&self.right));
                let ops = plan(&l, &r, &self.items);
                let to_r = ops.iter().filter(|o| matches!(o, Op::Copy { to, .. } if to.starts_with(&r))).count();
                let to_l = ops.iter().filter(|o| matches!(o, Op::Copy { to, .. } if to.starts_with(&l))).count();
                let dels = ops.iter().filter(|o| matches!(o, Op::Delete(_))).count();
                ui.horizontal(|ui| {
                    ui.label(lf!("{to_r} ➡ nach rechts · {to_l} ⬅ nach links · {dels} löschen", "{to_r} ➡ to the right · {to_l} ⬅ to the left · {dels} to delete"));
                    let stale = self.scanned_for.as_ref() != Some(&(l.clone(), r.clone()));
                    let enabled = !ops.is_empty() && !job_running && !self.is_scanning() && !stale;
                    if ui.add_enabled(enabled, egui::Button::new(RichText::new(l!("▶ Synchronisieren", "▶ Synchronize")).strong())).clicked() {
                        request = Some(SyncRequest::Run(ops.clone()));
                    }
                    if stale {
                        ui.label(RichText::new(l!("Pfade geändert – bitte neu vergleichen", "Paths changed – please compare again")).small().weak());
                    }
                });
            });
        self.open = open;
        request
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn touch(p: &Path, content: &str, secs_ago: u64) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn states_and_default_actions() {
        let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        touch(&l.path().join("nur_links.txt"), "a", 100);
        touch(&r.path().join("nur_rechts.txt"), "b", 100);
        touch(&l.path().join("sub/neuer_links.txt"), "neu", 10);
        touch(&r.path().join("sub/neuer_links.txt"), "alt", 1000);
        touch(&l.path().join("gleich.txt"), "x", 500);
        touch(&r.path().join("gleich.txt"), "x", 500);
        touch(&l.path().join("konflikt.txt"), "aaa", 500);
        touch(&r.path().join("konflikt.txt"), "bbbb", 500);
        std::fs::create_dir_all(l.path().join("leer")).unwrap();
        let items = scan(l.path(), r.path(), &Options::default()).unwrap();
        let by = |n: &str| items.iter().find(|i| i.rel == n).unwrap_or_else(|| panic!("{n} missing"));
        assert_eq!((by("nur_links.txt").state, by("nur_links.txt").action), (State::LeftOnly, Action::ToRight));
        assert_eq!((by("nur_rechts.txt").state, by("nur_rechts.txt").action), (State::RightOnly, Action::ToLeft));
        assert_eq!(by("sub/neuer_links.txt").state, State::LeftNewer);
        assert_eq!(by("gleich.txt").state, State::Equal);
        assert_eq!((by("konflikt.txt").state, by("konflikt.txt").action), (State::Differs, Action::None));
        assert!(by("leer").is_dir, "empty folders are listed");
        assert!(!items.iter().any(|i| i.rel == "sub"), "folders with files are not separate items");

        // Asymmetric: the right side becomes a mirror.
        let mirror = Options { asymmetric: true, ..Default::default() };
        let items = scan(l.path(), r.path(), &mirror).unwrap();
        let by = |n: &str| items.iter().find(|i| i.rel == n).unwrap();
        assert_eq!(by("nur_rechts.txt").action, Action::DeleteRight);
        assert_eq!(by("konflikt.txt").action, Action::ToRight);

        // By content: same size + same content counts as equal even with other dates.
        touch(&r.path().join("gleich.txt"), "x", 5);
        let items = scan(l.path(), r.path(), &Options { by_content: true, ..Default::default() }).unwrap();
        assert_eq!(items.iter().find(|i| i.rel == "gleich.txt").unwrap().state, State::Equal);
    }
}

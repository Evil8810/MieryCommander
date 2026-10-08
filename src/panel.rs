use crate::archive;
use crate::remote;
use crate::fsutil::{self, Entry};
use eframe::egui::{self, Color32, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Instant, SystemTime};

#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    Dir(PathBuf),
    /// Inside a zip file; `inner` is "" or "dir/sub/".
    Archive { file: PathBuf, inner: String },
    /// On an FTP server; `id` refers to the connection registry in `ftp`.
    Ftp { id: usize, path: String },
}

impl Location {
    pub fn display(&self) -> String {
        match self {
            Location::Dir(p) => p.to_string_lossy().into_owned(),
            Location::Archive { file, inner } => {
                format!("{}/{}", file.to_string_lossy(), inner)
            }
            Location::Ftp { id, path } => match remote::get(*id) {
                Some(c) => format!("{}{}", c.url(), path),
                None => format!("ftp://(getrennt){path}"),
            },
        }
    }

    /// The real directory on disk (for an archive: the dir containing it).
    pub fn real_dir(&self) -> PathBuf {
        match self {
            Location::Dir(p) => p.clone(),
            Location::Archive { file, .. } => {
                file.parent().map(Path::to_path_buf).unwrap_or_else(|| "/".into())
            }
            Location::Ftp { .. } => dirs::home_dir().unwrap_or_else(|| "/".into()),
        }
    }

    pub fn dir(&self) -> Option<&Path> {
        match self {
            Location::Dir(p) => Some(p),
            _ => None,
        }
    }

    pub fn ftp(&self) -> Option<(usize, &str)> {
        match self {
            Location::Ftp { id, path } => Some((*id, path)),
            _ => None,
        }
    }

    pub fn parent_loc(&self) -> Option<Location> {
        self.parent().map(|(l, _)| l)
    }

    fn parent(&self) -> Option<(Location, String)> {
        match self {
            Location::Dir(p) => {
                let parent = p.parent()?;
                let name = p.file_name()?.to_string_lossy().into_owned();
                Some((Location::Dir(parent.to_path_buf()), name))
            }
            Location::Archive { file, inner } => {
                if inner.is_empty() {
                    let name = file.file_name()?.to_string_lossy().into_owned();
                    let dir = file.parent()?.to_path_buf();
                    return Some((Location::Dir(dir), name));
                }
                let trimmed = inner.trim_end_matches('/');
                let (up, name) = match trimmed.rfind('/') {
                    Some(i) => (format!("{}/", &trimmed[..i]), &trimmed[i + 1..]),
                    None => (String::new(), trimmed),
                };
                Some((
                    Location::Archive {
                        file: file.clone(),
                        inner: up,
                    },
                    name.to_string(),
                ))
            }
            Location::Ftp { id, path } => {
                let up = remote::parent(path)?;
                Some((Location::Ftp { id: *id, path: up }, remote::file_name(path).to_string()))
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Ext,
    Size,
    Date,
}

pub struct Tab {
    pub loc: Location,
    raw: Vec<Entry>,
    /// Sorted, filtered view, ".." first.
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub marked: HashSet<String>,
    pub sort: SortKey,
    pub desc: bool,
    pub filter: String,
    pub filter_open: bool,
    pub quick_search: String,
    pub back: Vec<Location>,
    pub forward: Vec<Location>,
    pub error: Option<String>,
    pub scroll_to_cursor: bool,
    pub dir_sizes: HashMap<String, u64>,
    size_tx: Sender<(String, u64)>,
    size_rx: Receiver<(String, u64)>,
    /// Directories whose size is being computed right now.
    pub sizing: HashSet<String>,
    dir_mtime: Option<SystemTime>,
    last_check: Instant,
    /// Background "did the directory change?" probe in flight.
    mtime_rx: Option<Receiver<Option<SystemTime>>>,
    pub space: Option<(u64, u64)>,
    pub path_edit: Option<String>,
    /// Branch view (Ctrl+Shift+B): all files of all subfolders in one list.
    pub branch: bool,
    /// FTP listings arrive asynchronously.
    pub loading: bool,
    /// When the current load started (for "still loading…" hints).
    pub loading_since: Instant,
    /// Increased with every reload; stale results are dropped.
    generation: u64,
    list_tx: Sender<Listing>,
    list_rx: Receiver<Listing>,
    pending_select: Option<String>,
    loaded_loc: Option<Location>,
}

/// Result of a background directory read.
struct Listing {
    generation: u64,
    loc: Location,
    result: Result<Vec<Entry>, String>,
    mtime: Option<SystemTime>,
    space: Option<(u64, u64)>,
}

pub enum PanelAction {
    CancelLoading,
    Activate,
    Open(usize),
    Navigate(Location),
    /// A command chosen from a context menu.
    Cmd(crate::app::Cmd),
    NewTab,
    CloseTab(usize),
}

impl Tab {
    pub fn new(path: PathBuf, show_hidden: bool, dirs_first: bool) -> Self {
        let (size_tx, size_rx) = channel();
        let (list_tx, list_rx) = channel();
        let mut t = Tab {
            loc: Location::Dir(path),
            raw: Vec::new(),
            entries: Vec::new(),
            cursor: 0,
            marked: HashSet::new(),
            sort: SortKey::Name,
            desc: false,
            filter: String::new(),
            filter_open: false,
            quick_search: String::new(),
            back: Vec::new(),
            forward: Vec::new(),
            error: None,
            scroll_to_cursor: true,
            dir_sizes: HashMap::new(),
            size_tx,
            size_rx,
            sizing: HashSet::new(),
            dir_mtime: None,
            last_check: Instant::now(),
            mtime_rx: None,
            space: None,
            path_edit: None,
            branch: false,
            loading: false,
            loading_since: Instant::now(),
            generation: 0,
            list_tx,
            list_rx,
            pending_select: None,
            loaded_loc: None,
        };
        t.reload(show_hidden, dirs_first);
        t
    }

    /// A tab that opens directly at `loc` (also a server or an archive).
    pub fn new_at(loc: Location, show_hidden: bool, dirs_first: bool) -> Self {
        let mut t = Tab::new(loc.real_dir(), show_hidden, dirs_first);
        if t.loc != loc {
            t.loc = loc;
            t.reload(show_hidden, dirs_first);
        }
        t
    }

    pub fn title(&self) -> String {
        let p = match &self.loc {
            Location::Dir(p) => p.clone(),
            Location::Archive { file, .. } => file.clone(),
            Location::Ftp { id, path } => {
                let (icon, host) = remote::get(*id)
                    .map(|c| (c.site().icon(), c.site().host.clone()))
                    .unwrap_or(("🌐", String::new()));
                let leaf = remote::file_name(path);
                return if leaf.is_empty() { format!("{icon} {host}") } else { format!("{icon} {leaf}") };
            }
        };
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into())
    }

    /// Re-read the current location in the background. The old entries stay
    /// visible while reloading the same place; a new place starts empty.
    pub fn reload(&mut self, show_hidden: bool, dirs_first: bool) {
        let keep = self.pending_select.take().or_else(|| self.current().map(|e| e.name.clone()));
        self.pending_select = keep;
        if self.loaded_loc.as_ref() != Some(&self.loc) {
            self.raw.clear();
            self.space = None;
            self.apply_view(dirs_first);
        }
        if !self.loading {
            self.loading_since = Instant::now();
        }
        self.loading = true;
        self.generation += 1;
        let (generation, loc, tx) = (self.generation, self.loc.clone(), self.list_tx.clone());
        let branch = self.branch;
        std::thread::spawn(move || {
            let (result, mtime, space) = match &loc {
                Location::Dir(p) => (
                    if branch { fsutil::read_tree(p, show_hidden) } else { fsutil::read_dir(p, show_hidden) }
                        .map_err(|e| e.to_string()),
                    std::fs::metadata(p).and_then(|m| m.modified()).ok(),
                    fsutil::disk_space(p),
                ),
                Location::Archive { file, inner } => (
                    archive::list(file, inner),
                    std::fs::metadata(file).and_then(|m| m.modified()).ok(),
                    None,
                ),
                Location::Ftp { id, path } => (
                    match remote::get(*id) {
                        Some(c) => c.list(path),
                        None => Err(l!("Verbindung zum Server ist getrennt", "The connection to the server is closed").into()),
                    },
                    None,
                    None,
                ),
            };
            let _ = tx.send(Listing { generation, loc, result, mtime, space });
            fsutil::wake_ui();
        });
    }

    /// Pick up finished background work; every 1.5 s check (also in the
    /// background) whether the directory changed on disk.
    pub fn poll_changes(&mut self, show_hidden: bool, dirs_first: bool) {
        while let Ok((name, size)) = self.size_rx.try_recv() {
            self.sizing.remove(&name);
            self.dir_sizes.insert(name, size);
        }
        while let Ok(l) = self.list_rx.try_recv() {
            if l.generation != self.generation || l.loc != self.loc {
                continue; // superseded by a newer reload / navigation
            }
            self.loading = false;
            self.loaded_loc = Some(l.loc);
            self.dir_mtime = l.mtime;
            self.space = l.space;
            self.last_check = Instant::now();
            match l.result {
                Ok(v) => {
                    self.raw = v;
                    self.error = None;
                }
                Err(e) => {
                    self.raw.clear();
                    self.error = Some(e);
                }
            }
            self.marked.retain(|n| self.raw.iter().any(|e| &e.name == n));
            self.apply_view(dirs_first);
            if let Some(n) = self.pending_select.take() {
                self.select_name(&n);
            }
            self.clamp();
        }
        if let Some(rx) = &self.mtime_rx {
            match rx.try_recv() {
                Ok(mtime) => {
                    self.mtime_rx = None;
                    if mtime != self.dir_mtime && !self.loading {
                        self.reload(show_hidden, dirs_first);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => self.mtime_rx = None,
            }
        }
        if self.loading || self.mtime_rx.is_some() || self.last_check.elapsed().as_millis() < 1500 {
            return;
        }
        self.last_check = Instant::now();
        let probe = match &self.loc {
            Location::Dir(p) => p.clone(),
            Location::Archive { file, .. } => file.clone(),
            Location::Ftp { .. } => return, // no cheap change detection over FTP/SFTP
        };
        let (tx, rx) = channel();
        self.mtime_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(std::fs::metadata(&probe).and_then(|m| m.modified()).ok());
            fsutil::wake_ui();
        });
    }

    pub fn apply_view(&mut self, dirs_first: bool) {
        let filter = self.filter.to_lowercase();
        let mut v: Vec<Entry> = self
            .raw
            .iter()
            .filter(|e| filter.is_empty() || e.name.to_lowercase().contains(&filter))
            .cloned()
            .collect();
        let key = self.sort;
        let sizes = &self.dir_sizes;
        v.sort_by(|a, b| {
            if dirs_first && a.is_dir != b.is_dir {
                return b.is_dir.cmp(&a.is_dir);
            }
            let size = |e: &Entry| {
                if e.is_dir {
                    sizes.get(&e.name).copied().unwrap_or(0)
                } else {
                    e.size
                }
            };
            let ord = match key {
                SortKey::Name => natural_cmp(&a.name, &b.name),
                SortKey::Ext => a
                    .ext()
                    .to_lowercase()
                    .cmp(&b.ext().to_lowercase())
                    .then_with(|| natural_cmp(&a.name, &b.name)),
                SortKey::Size => size(a).cmp(&size(b)).then_with(|| natural_cmp(&a.name, &b.name)),
                SortKey::Date => a.modified.cmp(&b.modified),
            };
            if self.desc { ord.reverse() } else { ord }
        });
        if let Some((parent, _)) = self.loc.parent() {
            v.insert(0, Entry::parent(parent.real_dir()));
        }
        self.entries = v;
        self.clamp();
    }

    fn clamp(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.entries.len() {
            self.cursor = self.entries.len() - 1;
        }
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    pub fn select_name(&mut self, name: &str) {
        if self.loading {
            self.pending_select = Some(name.to_string());
            return;
        }
        if let Some(i) = self.entries.iter().position(|e| e.name == name) {
            self.cursor = i;
            self.scroll_to_cursor = true;
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let max = self.entries.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
        self.scroll_to_cursor = true;
    }

    pub fn navigate(&mut self, loc: Location, show_hidden: bool, dirs_first: bool) {
        if loc == self.loc {
            return;
        }
        let old = std::mem::replace(&mut self.loc, loc);
        self.back.push(old);
        if self.back.len() > 100 {
            self.back.remove(0);
        }
        self.forward.clear();
        self.after_location_change(show_hidden, dirs_first);
    }

    fn after_location_change(&mut self, show_hidden: bool, dirs_first: bool) {
        self.branch = false;
        self.marked.clear();
        self.dir_sizes.clear();
        self.filter.clear();
        self.filter_open = false;
        self.quick_search.clear();
        self.cursor = 0;
        self.path_edit = None;
        self.reload(show_hidden, dirs_first);
        self.scroll_to_cursor = true;
    }

    /// Give up on a load that hangs: go back to where we came from.
    pub fn cancel_loading(&mut self, show_hidden: bool, dirs_first: bool) {
        self.generation += 1; // the hanging result will be ignored
        self.loading = false;
        if !self.back.is_empty() {
            self.go_back(show_hidden, dirs_first);
            self.forward.clear();
        } else {
            let home = dirs::home_dir().unwrap_or_else(|| "/".into());
            self.navigate(Location::Dir(home), show_hidden, dirs_first);
        }
    }

    pub fn go_back(&mut self, show_hidden: bool, dirs_first: bool) {
        if let Some(prev) = self.back.pop() {
            let cur = std::mem::replace(&mut self.loc, prev);
            self.forward.push(cur);
            self.after_location_change(show_hidden, dirs_first);
        }
    }

    pub fn go_forward(&mut self, show_hidden: bool, dirs_first: bool) {
        if let Some(next) = self.forward.pop() {
            let cur = std::mem::replace(&mut self.loc, next);
            self.back.push(cur);
            self.after_location_change(show_hidden, dirs_first);
        }
    }

    pub fn go_up(&mut self, show_hidden: bool, dirs_first: bool) {
        if let Some((parent, name)) = self.loc.parent() {
            self.navigate(parent, show_hidden, dirs_first);
            self.select_name(&name);
        }
    }

    /// What Enter on the entry at `idx` means. Returns a file to open externally, if any.
    pub fn enter(&mut self, idx: usize, show_hidden: bool, dirs_first: bool) -> Option<PathBuf> {
        let e = self.entries.get(idx)?.clone();
        if e.is_parent {
            self.go_up(show_hidden, dirs_first);
            return None;
        }
        match &self.loc {
            Location::Dir(_) => {
                if e.is_dir {
                    self.navigate(Location::Dir(e.path.clone()), show_hidden, dirs_first);
                    None
                } else if archive::is_archive(&e.path) {
                    self.navigate(
                        Location::Archive {
                            file: e.path.clone(),
                            inner: String::new(),
                        },
                        show_hidden,
                        dirs_first,
                    );
                    None
                } else {
                    Some(e.path)
                }
            }
            Location::Archive { file, .. } => {
                if e.is_dir {
                    let loc = Location::Archive {
                        file: file.clone(),
                        inner: format!("{}/", e.path.to_string_lossy()),
                    };
                    self.navigate(loc, show_hidden, dirs_first);
                }
                None
            }
            Location::Ftp { id, .. } => {
                if e.is_dir || e.is_link {
                    let loc = Location::Ftp { id: *id, path: e.path.to_string_lossy().into_owned() };
                    self.navigate(loc, show_hidden, dirs_first);
                }
                None
            }
        }
    }

    pub fn toggle_mark(&mut self, idx: usize) {
        if let Some(e) = self.entries.get(idx)
            && !e.is_parent
        {
            let n = e.name.clone();
            if !self.marked.remove(&n) {
                self.marked.insert(n);
            }
        }
    }

    pub fn set_mark(&mut self, idx: usize, on: bool) {
        if let Some(e) = self.entries.get(idx)
            && !e.is_parent
        {
            if on {
                self.marked.insert(e.name.clone());
            } else {
                self.marked.remove(&e.name);
            }
        }
    }

    pub fn mark_all(&mut self, on: bool) {
        if on {
            self.marked = self
                .entries
                .iter()
                .filter(|e| !e.is_parent)
                .map(|e| e.name.clone())
                .collect();
        } else {
            self.marked.clear();
        }
    }

    pub fn invert_marks(&mut self) {
        let all: HashSet<String> = self
            .entries
            .iter()
            .filter(|e| !e.is_parent && !e.is_dir)
            .map(|e| e.name.clone())
            .collect();
        self.marked = all.symmetric_difference(&self.marked).cloned().collect();
    }

    pub fn mark_pattern(&mut self, mask: &str, on: bool) {
        let Some(re) = fsutil::wildcard_regex(mask) else { return };
        for e in &self.entries {
            if !e.is_parent && re.is_match(&e.name) {
                if on {
                    self.marked.insert(e.name.clone());
                } else {
                    self.marked.remove(&e.name);
                }
            }
        }
    }

    /// Marked entries, or the entry under the cursor if nothing is marked.
    pub fn selection(&self) -> Vec<Entry> {
        let marked: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| self.marked.contains(&e.name))
            .cloned()
            .collect();
        if !marked.is_empty() {
            return marked;
        }
        self.current()
            .filter(|e| !e.is_parent)
            .cloned()
            .into_iter()
            .collect()
    }

    pub fn calc_dir_size(&mut self, e: &Entry) {
        if !e.is_dir || e.is_parent || self.loc.dir().is_none() {
            return;
        }
        if !self.sizing.insert(e.name.clone()) {
            return; // already running
        }
        let tx = self.size_tx.clone();
        let name = e.name.clone();
        let path = e.path.clone();
        std::thread::spawn(move || {
            let _ = tx.send((name, fsutil::dir_size(&path)));
            fsutil::wake_ui();
        });
    }

    pub fn quick_search_push(&mut self, s: &str) {
        let mut candidate = self.quick_search.clone();
        candidate.push_str(s);
        let needle = candidate.to_lowercase();
        // Search from the current position so repeated letters cycle.
        let n = self.entries.len();
        let found = (0..n)
            .map(|i| (self.cursor + i) % n)
            .find(|&i| self.entries[i].name.to_lowercase().starts_with(&needle));
        if let Some(i) = found {
            self.quick_search = candidate;
            self.cursor = i;
            self.scroll_to_cursor = true;
        }
    }

    fn status_line(&self) -> String {
        let files: Vec<&Entry> = self.entries.iter().filter(|e| !e.is_parent && !e.is_dir).collect();
        let dirs = self.entries.iter().filter(|e| !e.is_parent && e.is_dir).count();
        let total: u64 = files.iter().map(|e| e.size).sum();
        let mut sel_files = 0;
        let mut sel_dirs = 0;
        let mut sel_bytes = 0;
        for e in &self.entries {
            if self.marked.contains(&e.name) {
                if e.is_dir {
                    sel_dirs += 1;
                    sel_bytes += self.dir_sizes.get(&e.name).copied().unwrap_or(0);
                } else {
                    sel_files += 1;
                    sel_bytes += e.size;
                }
            }
        }
        lf!("{} / {} in {} / {} Datei(en), {} / {} Ordner", "{} / {} in {} / {} file(s), {} / {} folders",
            fsutil::format_size_short(sel_bytes),
            fsutil::format_size_short(total),
            sel_files,
            files.len(),
            sel_dirs,
            dirs
        )
    }
}

/// "file2" < "file10", case-insensitive.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().filter(|c| c.is_ascii_digit()) {
                    na.push(*c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().filter(|c| c.is_ascii_digit()) {
                    nb.push(*c);
                    bi.next();
                }
                let na_t = na.trim_start_matches('0');
                let nb_t = nb.trim_start_matches('0');
                let ord = na_t.len().cmp(&nb_t.len()).then_with(|| na_t.cmp(nb_t));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(ca), Some(cb)) => {
                let la = ca.to_lowercase().next().unwrap_or(*ca);
                let lb = cb.to_lowercase().next().unwrap_or(*cb);
                if la != lb {
                    return la.cmp(&lb);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

pub struct Panel {
    pub tabs: Vec<Tab>,
    pub active: usize,
}

impl Panel {
    pub fn new(paths: &[PathBuf], active: usize, show_hidden: bool, dirs_first: bool) -> Self {
        let mut tabs: Vec<Tab> = paths
            .iter()
            .filter(|p| p.is_dir())
            .map(|p| Tab::new(p.clone(), show_hidden, dirs_first))
            .collect();
        if tabs.is_empty() {
            let home = dirs::home_dir().unwrap_or_else(|| "/".into());
            tabs.push(Tab::new(home, show_hidden, dirs_first));
        }
        let active = active.min(tabs.len() - 1);
        Panel { tabs, active }
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    pub fn tab_paths(&self) -> Vec<PathBuf> {
        self.tabs.iter().map(|t| t.loc.real_dir()).collect()
    }
}

pub struct PanelColors {
    pub marked: Color32,
    pub active_header: Color32,
    pub inactive_header: Color32,
    pub header_text: Color32,
}

impl PanelColors {
    pub fn for_visuals(v: &egui::Visuals) -> Self {
        if v.dark_mode {
            PanelColors {
                marked: Color32::from_rgb(255, 110, 110),
                active_header: Color32::from_rgb(40, 80, 150),
                inactive_header: Color32::from_gray(60),
                header_text: Color32::WHITE,
            }
        } else {
            PanelColors {
                marked: Color32::from_rgb(210, 0, 0),
                active_header: Color32::from_rgb(10, 36, 106),
                inactive_header: Color32::from_gray(170),
                header_text: Color32::WHITE,
            }
        }
    }
}

fn icon_for(e: &Entry) -> &'static str {
    if e.is_parent {
        "⬆"
    } else if e.is_dir {
        "📁"
    } else if e.is_link {
        "🔗"
    } else if archive::is_archive(&e.path) {
        "📦"
    } else {
        match e.ext().to_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" => "🖼",
            "mp3" | "flac" | "ogg" | "wav" | "m4a" => "🎵",
            "mp4" | "mkv" | "avi" | "mov" | "webm" => "🎞",
            "rs" | "py" | "js" | "ts" | "c" | "h" | "cpp" | "go" | "java" | "sh" => "📝",
            _ => "📄",
        }
    }
}

/// Draw one panel. `side` is used to make ids unique.
pub fn show_panel(
    ui: &mut egui::Ui,
    panel: &mut Panel,
    side: &str,
    is_active: bool,
    dirs_first: bool,
    drive_bar: crate::config::DriveBar,
    cut: &[PathBuf],
) -> Vec<PanelAction> {
    let mut actions = Vec::new();
    let colors = PanelColors::for_visuals(ui.visuals());

    // --- Tabs ---------------------------------------------------------------
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for i in 0..panel.tabs.len() {
            let title = panel.tabs[i].title();
            let r = ui.selectable_label(i == panel.active, title);
            if r.clicked() {
                panel.active = i;
                actions.push(PanelAction::Activate);
            }
            if r.middle_clicked() {
                actions.push(PanelAction::CloseTab(i));
            }
            r.context_menu(|ui| {
                if ui.button(l!("Tab schließen", "Close tab")).clicked() {
                    actions.push(PanelAction::CloseTab(i));
                }
            });
        }
        if ui.small_button("+").on_hover_text(l!("Neuer Tab (Strg+T)", "New tab (Ctrl+T)")).clicked() {
            actions.push(PanelAction::NewTab);
        }
    });

    let tab = panel.tab_mut();

    // --- Drive buttons (like Total Commander's drive bar) -----------------------
    use crate::config::DriveBar;
    let current_place = match tab.loc.dir() {
        Some(d) => fsutil::place_of(d).map(|p| p.path),
        None => None,
    };
    let current_conn = tab.loc.ftp().map(|(id, _)| id);
    if drive_bar != DriveBar::Dropdown {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.spacing_mut().button_padding = egui::vec2(4.0, 1.0);
            for p in fsutil::locations() {
                let selected = current_place.as_ref() == Some(&p.path);
                let text = RichText::new(format!("{} {}", p.kind.icon(), p.short_label())).small();
                let r = ui.add(egui::Button::selectable(selected, text));
                let r = r.on_hover_text(format!("{}\n{}", p.label, p.path.to_string_lossy()));
                if r.clicked() {
                    actions.push(PanelAction::Navigate(Location::Dir(p.path)));
                }
            }
            for c in remote::connections() {
                let selected = current_conn == Some(c.id());
                let text = RichText::new(format!("{} {}", c.site().icon(), c.site().label())).small();
                let r = ui.add(egui::Button::selectable(selected, text)).on_hover_text(c.url());
                if r.clicked() {
                    actions.push(PanelAction::Navigate(Location::Ftp { id: c.id(), path: c.home().to_string() }));
                }
            }
        });
    }

    // --- Location bar ---------------------------------------------------------
    ui.horizontal(|ui| {
        let cur = tab.loc.real_dir();
        if drive_bar != DriveBar::Buttons {
        egui::ComboBox::from_id_salt(format!("{side}_loc"))
            .width(110.0)
            .selected_text(match tab.loc.ftp().and_then(|(id, _)| remote::get(id)) {
                Some(c) => format!("{} {}", c.site().icon(), c.site().label()),
                None => fsutil::place_of(&cur)
                    .map(|p| format!("{} {}", p.kind.icon(), p.label))
                    .unwrap_or_else(|| "/".into()),
            })
            .show_ui(ui, |ui| {
                let mut last_kind = None;
                for p in fsutil::locations() {
                    if last_kind.is_some_and(|k| k != p.kind) && p.kind == fsutil::PlaceKind::Network {
                        ui.separator();
                    }
                    last_kind = Some(p.kind);
                    let r = ui.selectable_label(false, format!("{} {}", p.kind.icon(), p.label));
                    if r.on_hover_text(p.path.to_string_lossy()).clicked() {
                        actions.push(PanelAction::Navigate(Location::Dir(p.path)));
                    }
                }
                for c in remote::connections() {
                    if ui.selectable_label(false, format!("{} {}", c.site().icon(), c.site().label())).clicked() {
                        actions.push(PanelAction::Navigate(Location::Ftp { id: c.id(), path: c.home().to_string() }));
                    }
                }
            });
        }
        if let Some((free, total)) = tab.space {
            ui.label(
                RichText::new(lf!("{} frei von {}", "{} free of {}",
                    fsutil::format_size_short(free),
                    fsutil::format_size_short(total)
                ))
                .small(),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("=")
                .on_hover_text(l!("Gleicher Ordner wie im anderen Panel (Strg+G)", "Same folder as in the other panel (Ctrl+G)"))
                .clicked()
            {
                actions.push(PanelAction::Cmd(crate::app::Cmd::TakeOtherDir));
            }
            if ui.small_button("..").on_hover_text(l!("Übergeordneter Ordner", "Parent folder")).clicked() {
                if let Some((p, _)) = tab.loc.parent() {
                    actions.push(PanelAction::Navigate(p));
                }
            }
            if ui.small_button("/").on_hover_text(l!("Wurzel", "Root")).clicked() {
                actions.push(PanelAction::Navigate(Location::Dir("/".into())));
            }
            if ui.small_button("~").on_hover_text("Home").clicked()
                && let Some(h) = dirs::home_dir()
            {
                actions.push(PanelAction::Navigate(Location::Dir(h)));
            }
        });
    });

    // --- Path header (click to edit) ----------------------------------------
    let header_bg = if is_active {
        colors.active_header
    } else {
        colors.inactive_header
    };
    egui::Frame::new()
        .fill(header_bg)
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(edit) = &mut tab.path_edit {
                let r = ui.add(
                    egui::TextEdit::singleline(edit)
                        .desired_width(f32::INFINITY)
                        .id(egui::Id::new(format!("{side}_pathedit"))),
                );
                if r.lost_focus() {
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let target = match tab.loc.ftp() {
                            Some((id, _)) => Location::Ftp { id, path: edit.trim().to_string() },
                            None => Location::Dir(PathBuf::from(expand_tilde(edit.trim()))),
                        };
                        actions.push(PanelAction::Navigate(target));
                    }
                    tab.path_edit = None;
                } else if !r.has_focus() {
                    r.request_focus();
                }
            } else {
                let mut text = tab.loc.display();
                if tab.branch {
                    text.push_str(l!("   [Branch-View: alle Unterordner]", "   [Branch view: all subfolders]"));
                }
                if !tab.filter.is_empty() {
                    text.push_str(&format!("   [Filter: {}]", tab.filter));
                }
                let r = ui
                    .horizontal(|ui| {
                        if tab.loading {
                            // Small spinner in the path bar: work in progress, app is alive.
                            ui.add(egui::Spinner::new().size(12.0).color(colors.header_text));
                        }
                        ui.add(
                            egui::Label::new(RichText::new(text).color(colors.header_text).strong())
                                .truncate()
                                .sense(Sense::click()),
                        )
                    })
                    .inner;
                if r.clicked() {
                    actions.push(PanelAction::Activate);
                }
                if r.double_clicked() {
                    tab.path_edit = Some(match tab.loc.ftp() {
                        Some((_, path)) => path.to_string(),
                        None => tab.loc.display(),
                    });
                }
                r.on_hover_text(l!("Doppelklick zum Bearbeiten", "Double-click to edit"));
            }
        });

    // --- Quick filter ---------------------------------------------------------
    if tab.filter_open {
        ui.horizontal(|ui| {
            ui.label("Filter:");
            let id = egui::Id::new(format!("{side}_filter"));
            let r = ui.add(
                egui::TextEdit::singleline(&mut tab.filter)
                    .id(id)
                    .hint_text(l!("Teil des Namens… (Esc schließt)", "Part of the name… (Esc closes)"))
                    .desired_width(f32::INFINITY),
            );
            if r.changed() {
                tab.apply_view(dirs_first);
                tab.cursor = 0;
            }
            // Enter keeps the filter and hands the keyboard back to the list.
            if (r.has_focus() || r.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                tab.filter.clear();
                tab.filter_open = false;
                tab.apply_view(dirs_first);
            }
        });
    }

    // Loading a new place (nothing to show yet) or something takes unusually long:
    // explain what is going on and offer a way out.
    let waited = tab.loading_since.elapsed();
    let nothing_yet = tab.entries.iter().all(|e| e.is_parent);
    if tab.loading && (nothing_yet || waited.as_secs_f32() > 1.5) {
        egui::Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spinner();
                    let what = if tab.loc.ftp().is_some() { l!("Lade Verzeichnis vom Server…", "Loading folder from server…") } else { l!("Lade Verzeichnis…", "Loading folder…") };
                    ui.label(format!("{what} {:.0} s", waited.as_secs_f32().floor()));
                });
                if waited.as_secs() >= 3 {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(l!("Das dauert ungewöhnlich lange – vielleicht antwortet ein Netzlaufwerk oder Server nicht. Die App läuft weiter.", "This is taking unusually long – maybe a network drive or server is not responding. The app keeps running."))
                                .small()
                                .weak(),
                        );
                        if ui.small_button(l!("Abbrechen", "Cancel")).clicked() {
                            actions.push(PanelAction::CancelLoading);
                        }
                    });
                }
            });
        // Keep the seconds counter ticking.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
    if let Some(err) = &tab.error {
        ui.colored_label(Color32::from_rgb(200, 60, 60), format!("⚠ {err}"));
    }

    // --- File table -------------------------------------------------------
    let status_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
    let table_h = (ui.available_height() - status_h).max(50.0);
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let mut sort_click: Option<SortKey> = None;
    let mut row_clicked: Option<(usize, egui::Modifiers)> = None;
    let mut row_double: Option<usize> = None;
    let mut row_ctx: Option<PanelAction> = None;
    let mut row_right_clicked: Option<usize> = None;

    let scroll_to = if tab.scroll_to_cursor {
        tab.scroll_to_cursor = false;
        Some(tab.cursor)
    } else {
        None
    };

    // The table sits in a click-sensing area: a right-click on empty space
    // (not on a row) opens the folder menu (paste, new folder, …).
    let background = ui.scope_builder(egui::UiBuilder::new().sense(Sense::click()), |ui| {
        let mut table = TableBuilder::new(ui)
            .id_salt(format!("{side}_table"))
            .striped(true)
            .sense(Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::remainder().at_least(140.0).clip(true))
            .column(Column::initial(55.0).at_least(30.0).resizable(true).clip(true))
            .column(Column::initial(90.0).at_least(50.0).resizable(true).clip(true))
            .column(Column::initial(118.0).at_least(60.0).resizable(true).clip(true))
            .column(Column::initial(80.0).at_least(40.0).clip(true))
            .min_scrolled_height(table_h)
            .max_scroll_height(table_h)
            .auto_shrink([false, false]);
        if let Some(r) = scroll_to {
            table = table.scroll_to_row(r, None);
        }

        let sort = tab.sort;
        let desc = tab.desc;
        let arrow = |k: SortKey| {
            if sort == k {
                if desc { " ⬇" } else { " ⬆" }
            } else {
                ""
            }
        };
        table
            .header(row_h + 2.0, |mut header| {
                for (k, label) in [
                    (SortKey::Name, "Name"),
                    (SortKey::Ext, l!("Erw.", "Ext")),
                    (SortKey::Size, l!("Größe", "Size")),
                    (SortKey::Date, l!("Datum", "Date")),
                ] {
                    header.col(|ui| {
                        if ui
                            .add(egui::Button::new(RichText::new(format!("{label}{}", arrow(k))).strong()).frame(false))
                            .clicked()
                        {
                            sort_click = Some(k);
                        }
                    });
                }
                header.col(|ui| {
                    ui.strong(l!("Rechte", "Attr"));
                });
            })
            .body(|body| {
                body.rows(row_h, tab.entries.len(), |mut row| {
                    let i = row.index();
                    let e = &tab.entries[i];
                    let is_cursor = i == tab.cursor;
                    let marked = tab.marked.contains(&e.name);
                    row.set_selected(is_cursor && is_active);
                    let color = if marked { Some(colors.marked) } else { None };
                    // Cut (Ctrl+X) entries are shown faded until they are pasted.
                    let is_cut = !cut.is_empty() && cut.contains(&e.path);
                    let txt = |s: String| {
                        let mut t = RichText::new(s);
                        if let Some(c) = color {
                            t = t.color(c).strong();
                        }
                        if is_cut {
                            t = t.weak().italics();
                        }
                        t
                    };
                    let (stem, ext) = e.split_name();
                    let name = if e.is_parent {
                        "..".to_string()
                    } else if ext.is_empty() {
                        e.name.clone()
                    } else {
                        stem.to_string()
                    };
                    row.col(|ui| {
                        if is_cursor && !is_active {
                            let r = ui.max_rect();
                            ui.painter().rect_stroke(
                                r,
                                0.0,
                                egui::Stroke::new(1.0, ui.visuals().selection.bg_fill),
                                egui::StrokeKind::Inside,
                            );
                        }
                        ui.add(egui::Label::new(txt(format!("{} {}", icon_for(e), name))).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(txt(ext.to_string())).selectable(false));
                    });
                    row.col(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let s = if e.is_dir {
                                match tab.dir_sizes.get(&e.name) {
                                    Some(s) => fsutil::format_size(*s),
                                    None if tab.sizing.contains(&e.name) => l!("berechne…", "calculating…").into(),
                                    None => "<DIR>".into(),
                                }
                            } else {
                                fsutil::format_size(e.size)
                            };
                            ui.add(egui::Label::new(txt(s)).selectable(false));
                        });
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(txt(fsutil::format_time(e.modified))).selectable(false));
                    });
                    row.col(|ui| {
                        let m = if e.is_parent { String::new() } else { fsutil::format_mode(e.mode) };
                        ui.add(egui::Label::new(txt(m).monospace()).selectable(false));
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        row_clicked = Some((i, resp.ctx.input(|inp| inp.modifiers)));
                    }
                    if resp.double_clicked() {
                        row_double = Some(i);
                    }
                    if resp.secondary_clicked() {
                        row_right_clicked = Some(i);
                    }
                    let marked_count = tab.marked.len();
                    resp.context_menu(|ui| {
                        if let Some(c) = entry_menu(ui, e, &tab.loc, marked_count) {
                            row_ctx = Some(PanelAction::Cmd(c));
                        }
                    });
                });
            });
    });
    background.response.context_menu(|ui| {
        if let Some(c) = folder_menu(ui, &tab.loc) {
            row_ctx = Some(PanelAction::Cmd(c));
        }
    });

    if let Some(k) = sort_click {
        if tab.sort == k {
            tab.desc = !tab.desc;
        } else {
            tab.sort = k;
            tab.desc = false;
        }
        let keep = tab.current().map(|e| e.name.clone());
        tab.apply_view(dirs_first);
        if let Some(n) = keep {
            tab.select_name(&n);
        }
        actions.push(PanelAction::Activate);
    }
    if let Some((i, mods)) = row_clicked {
        if mods.command {
            tab.toggle_mark(i);
        } else if mods.shift {
            let (a, b) = if i < tab.cursor { (i, tab.cursor) } else { (tab.cursor, i) };
            for j in a..=b {
                tab.set_mark(j, true);
            }
        }
        tab.cursor = i;
        tab.quick_search.clear();
        actions.push(PanelAction::Activate);
    }
    if let Some(i) = row_right_clicked {
        // Right-click on an unmarked entry acts on that entry only (like other
        // file managers) – never silently on other marked files.
        if tab.entries.get(i).is_some_and(|e| !tab.marked.contains(&e.name)) {
            tab.marked.clear();
        }
        tab.cursor = i;
        tab.quick_search.clear();
        actions.push(PanelAction::Activate);
    }
    if let Some(i) = row_double {
        actions.push(PanelAction::Open(i));
    }
    if let Some(a) = row_ctx {
        actions.push(a);
    }

    // --- Status line ----------------------------------------------------------
    ui.horizontal(|ui| {
        ui.label(RichText::new(tab.status_line()).small());
        if !tab.quick_search.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("🔎 {}", tab.quick_search))
                        .background_color(ui.visuals().selection.bg_fill)
                        .color(ui.visuals().selection.stroke.color),
                );
            });
        }
    });

    actions
}

pub fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix('~')
        && let Some(h) = dirs::home_dir()
    {
        return format!("{}{}", h.to_string_lossy(), rest);
    }
    s.to_string()
}

/// One menu entry with its keyboard shortcut on the right.
fn menu_item(ui: &mut egui::Ui, label: &str, shortcut: &str, cmd: crate::app::Cmd, out: &mut Option<crate::app::Cmd>) {
    if ui.add(egui::Button::new(label).shortcut_text(crate::i18n::keys(shortcut))).clicked() {
        *out = Some(cmd);
        ui.close();
    }
}

/// Context menu for a file or folder.
fn entry_menu(ui: &mut egui::Ui, e: &Entry, loc: &Location, marked: usize) -> Option<crate::app::Cmd> {
    use crate::app::Cmd;
    let mut out = None;
    let local = loc.dir().is_some();
    let in_archive = matches!(loc, Location::Archive { .. });
    ui.set_min_width(230.0);
    if marked > 1 {
        ui.label(RichText::new(lf!("{marked} markierte Elemente", "{marked} marked items")).weak().small());
        ui.separator();
    }
    if !e.is_parent {
        menu_item(ui, if e.is_dir { l!("Öffnen", "Open") } else { l!("Öffnen (Standardprogramm)", "Open (default application)") }, "Enter", Cmd::OpenDefault, &mut out);
        if !e.is_dir {
            menu_item(ui, l!("Ansehen", "View"), "F3", Cmd::View, &mut out);
            menu_item(ui, l!("Bearbeiten", "Edit"), "F4", Cmd::Edit, &mut out);
            if local {
                let label = if marked == 2 { l!("Markierte vergleichen", "Compare marked") } else { l!("Mit Datei im anderen Panel vergleichen", "Compare with file in the other panel") };
                menu_item(ui, label, "", Cmd::CompareFiles, &mut out);
            }
        }
        ui.separator();
        if !in_archive {
            menu_item(ui, l!("Ausschneiden", "Cut"), "Strg+X", Cmd::ClipCut, &mut out);
        }
        menu_item(ui, l!("Kopieren", "Copy"), "Strg+C", Cmd::ClipCopy, &mut out);
    }
    menu_item(ui, l!("Einfügen", "Paste"), "Strg+V", Cmd::ClipPaste, &mut out);
    if !e.is_parent {
        ui.separator();
        menu_item(ui, l!("Kopieren nach…", "Copy to…"), "F5", Cmd::Copy, &mut out);
        if !in_archive {
            menu_item(ui, l!("Verschieben nach…", "Move to…"), "F6", Cmd::Move, &mut out);
            menu_item(ui, l!("Umbenennen", "Rename"), "Shift+F6", Cmd::Rename, &mut out);
            if local {
                menu_item(ui, l!("In den Papierkorb", "Move to trash"), "F8", Cmd::Delete, &mut out);
                menu_item(ui, l!("Endgültig löschen", "Delete permanently"), "Shift+F8", Cmd::DeletePermanent, &mut out);
            } else {
                menu_item(ui, l!("Löschen", "Delete"), "F8", Cmd::Delete, &mut out);
            }
        }
    }
    if e.is_dir {
        ui.separator();
        menu_item(ui, l!("Im anderen Panel öffnen", "Open in the other panel"), "", Cmd::OpenInOther, &mut out);
        menu_item(ui, l!("In neuem Tab öffnen", "Open in new tab"), "", Cmd::OpenInNewTab, &mut out);
        if local && !e.is_parent {
            menu_item(ui, l!("Ordnergröße berechnen", "Calculate folder size"), "Leertaste", Cmd::CalcSize, &mut out);
            menu_item(ui, l!("Zu Favoriten hinzufügen", "Add to favourites"), "", Cmd::AddEntryToHotlist, &mut out);
        }
    }
    if local && !e.is_parent {
        ui.separator();
        if crate::archive::is_archive(&e.path) {
            menu_item(ui, l!("Smart hier entpacken", "Smart unpack here"), "Alt+Shift+F9", Cmd::UnpackSmart, &mut out);
            menu_item(ui, l!("Hier entpacken", "Unpack here"), "", Cmd::UnpackHere, &mut out);
            menu_item(ui, l!("Entpacken nach…", "Unpack to…"), "Alt+F9", Cmd::Unpack, &mut out);
        }
        menu_item(ui, l!("Packen (ZIP, 7z, TAR …)…", "Pack (ZIP, 7z, TAR …)…"), "Alt+F5", Cmd::Pack, &mut out);
    }
    ui.separator();
    if !in_archive {
        menu_item(ui, l!("Neuer Ordner", "New folder"), "F7", Cmd::Mkdir, &mut out);
    }
    if local {
        menu_item(ui, l!("Neue Datei", "New file"), "Shift+F4", Cmd::NewFile, &mut out);
    }
    ui.separator();
    if !e.is_parent {
        menu_item(ui, l!("Pfad kopieren", "Copy path"), "Strg+Shift+C", Cmd::CopyPaths, &mut out);
        menu_item(ui, l!("Name kopieren", "Copy name"), "Strg+Shift+N", Cmd::CopyNames, &mut out);
    }
    menu_item(ui, l!("Terminal hier öffnen", "Open terminal here"), "F9", Cmd::Terminal, &mut out);
    if !e.is_parent {
        menu_item(ui, l!("Eigenschaften", "Properties"), "Alt+Enter", Cmd::Properties, &mut out);
    }
    out
}

/// Context menu for empty space in a panel (the current folder itself).
fn folder_menu(ui: &mut egui::Ui, loc: &Location) -> Option<crate::app::Cmd> {
    use crate::app::Cmd;
    let mut out = None;
    let local = loc.dir().is_some();
    ui.set_min_width(230.0);
    menu_item(ui, l!("Einfügen", "Paste"), "Strg+V", Cmd::ClipPaste, &mut out);
    ui.separator();
    if !matches!(loc, Location::Archive { .. }) {
        menu_item(ui, l!("Neuer Ordner", "New folder"), "F7", Cmd::Mkdir, &mut out);
    }
    if local {
        menu_item(ui, l!("Neue Datei", "New file"), "Shift+F4", Cmd::NewFile, &mut out);
    }
    ui.separator();
    menu_item(ui, l!("Alles markieren", "Select all"), "Strg+A", Cmd::SelectAll, &mut out);
    menu_item(ui, l!("Neu einlesen", "Reload"), "Strg+R", Cmd::Reload, &mut out);
    if local {
        menu_item(ui, l!("Zu Favoriten hinzufügen", "Add to favourites"), "Strg+Shift+D", Cmd::AddHotlist, &mut out);
    }
    menu_item(ui, l!("Terminal hier öffnen", "Open terminal here"), "F9", Cmd::Terminal, &mut out);
    out
}

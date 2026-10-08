//! Find files (Alt+F7).

use crate::fsutil;
use eframe::egui::{self, RichText};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Shared {
    results: Vec<PathBuf>,
    scanned: u64,
    current: String,
    done: bool,
}

pub struct Search {
    pub open: bool,
    pub start_dir: String,
    pub name_mask: String,
    pub use_regex: bool,
    pub content: String,
    pub case_sensitive: bool,
    pub include_hidden: bool,
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    running: bool,
    error: Option<String>,
    selected: Option<usize>,
}

pub enum SearchAction {
    GoTo(PathBuf),
    View(PathBuf),
}

impl Search {
    pub fn new() -> Self {
        Search {
            open: false,
            start_dir: String::new(),
            name_mask: "*".into(),
            use_regex: false,
            content: String::new(),
            case_sensitive: false,
            include_hidden: false,
            shared: Arc::new(Mutex::new(Shared::default())),
            cancel: Arc::new(AtomicBool::new(false)),
            running: false,
            error: None,
            selected: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn open_at(&mut self, dir: &Path) {
        self.open = true;
        if !self.running {
            self.start_dir = dir.to_string_lossy().into_owned();
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.error = None;
        self.selected = None;
        let name_re = if self.use_regex {
            regex::RegexBuilder::new(&self.name_mask)
                .case_insensitive(!self.case_sensitive)
                .build()
                .map_err(|e| e.to_string())
        } else {
            fsutil::wildcard_regex(&self.name_mask).ok_or_else(|| l!("Ungültige Maske", "Invalid mask").to_string())
        };
        let name_re = match name_re {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let content_re = if self.content.is_empty() {
            None
        } else {
            let pat = if self.use_regex {
                self.content.clone()
            } else {
                regex::escape(&self.content)
            };
            match regex::bytes::RegexBuilder::new(&pat)
                .case_insensitive(!self.case_sensitive)
                .build()
            {
                Ok(r) => Some(r),
                Err(e) => {
                    self.error = Some(e.to_string());
                    return;
                }
            }
        };
        let root = PathBuf::from(crate::panel::expand_tilde(&self.start_dir));
        self.shared = Arc::new(Mutex::new(Shared::default()));
        self.cancel = Arc::new(AtomicBool::new(false));
        self.running = true;
        let shared = self.shared.clone();
        let cancel = self.cancel.clone();
        let hidden = self.include_hidden;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut stack = vec![root];
            let mut last_paint = std::time::Instant::now();
            while let Some(dir) = stack.pop() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                shared.lock().unwrap().current = dir.to_string_lossy().into_owned();
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if !hidden && name.starts_with('.') {
                        continue;
                    }
                    let Ok(ft) = e.file_type() else { continue };
                    let path = e.path();
                    if ft.is_dir() {
                        stack.push(path.clone());
                    }
                    let mut hit = name_re.is_match(&name);
                    if hit && let Some(re) = &content_re {
                        hit = ft.is_file() && file_contains(&path, re);
                    }
                    let mut s = shared.lock().unwrap();
                    s.scanned += 1;
                    if hit {
                        s.results.push(path);
                    }
                }
                if last_paint.elapsed().as_millis() > 100 {
                    ctx.request_repaint();
                    last_paint = std::time::Instant::now();
                }
            }
            shared.lock().unwrap().done = true;
            ctx.request_repaint();
        });
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<SearchAction> {
        if !self.open {
            return None;
        }
        let mut action = None;
        let mut open = self.open;
        egui::Window::new(l!("Dateien suchen", "Find files"))
            .open(&mut open)
            .default_size([700.0, 500.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::Grid::new("search_grid").num_columns(2).show(ui, |ui| {
                    ui.label(l!("Suchen in:", "Search in:"));
                    ui.add(egui::TextEdit::singleline(&mut self.start_dir).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label(l!("Dateiname:", "File name:"));
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.name_mask)
                            .hint_text("*.txt;*.md")
                            .desired_width(f32::INFINITY),
                    );
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.running {
                        self.start(ctx);
                    }
                    ui.end_row();
                    ui.label(l!("Enthält Text:", "Contains text:"));
                    ui.add(egui::TextEdit::singleline(&mut self.content).desired_width(f32::INFINITY));
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.use_regex, l!("Reguläre Ausdrücke", "Regular expressions"));
                    ui.checkbox(&mut self.case_sensitive, l!("Groß/klein beachten", "Match case"));
                    ui.checkbox(&mut self.include_hidden, l!("Versteckte", "Hidden"));
                });
                ui.horizontal(|ui| {
                    if self.running {
                        if ui.button(l!("⏹ Abbrechen", "⏹ Cancel")).clicked() {
                            self.cancel.store(true, Ordering::Relaxed);
                        }
                    } else if ui.button(l!("🔎 Suche starten", "🔎 Start search")).clicked() {
                        self.start(ctx);
                    }
                    if self.running {
                        ui.spinner();
                    }
                    let s = self.shared.lock().unwrap();
                    if s.done {
                        self.running = false;
                    }
                    let status = if self.running {
                        lf!("{} geprüft · {} Treffer · {}", "{} checked · {} hits · {}", s.scanned, s.results.len(), s.current)
                    } else {
                        lf!("{} geprüft · {} Treffer", "{} checked · {} hits", s.scanned, s.results.len())
                    };
                    ui.add(egui::Label::new(RichText::new(status).small()).truncate());
                });
                if let Some(e) = &self.error {
                    ui.colored_label(egui::Color32::RED, e);
                }
                ui.separator();
                let results = self.shared.lock().unwrap().results.clone();
                let row_h = ui.text_style_height(&egui::TextStyle::Body) + 2.0;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height(ui.available_height() - 30.0)
                    .show_rows(ui, row_h, results.len(), |ui, range| {
                        for i in range {
                            let p = &results[i];
                            let r = ui.selectable_label(self.selected == Some(i), p.to_string_lossy());
                            if r.clicked() {
                                self.selected = Some(i);
                            }
                            if r.double_clicked() {
                                action = Some(SearchAction::GoTo(p.clone()));
                            }
                        }
                    });
                ui.horizontal(|ui| {
                    let sel = self.selected.and_then(|i| results.get(i)).cloned();
                    ui.add_enabled_ui(sel.is_some(), |ui| {
                        if ui.button(l!("Gehe zu Datei", "Go to file")).clicked() {
                            action = sel.clone().map(SearchAction::GoTo);
                        }
                        if ui.button(l!("Ansehen (F3)", "View (F3)")).clicked() {
                            action = sel.clone().map(SearchAction::View);
                        }
                    });
                });
            });
        self.open = open;
        if matches!(action, Some(SearchAction::GoTo(_))) {
            self.open = false;
        }
        action
    }
}

fn file_contains(path: &Path, re: &regex::bytes::Regex) -> bool {
    let Ok(f) = std::fs::File::open(path) else { return false };
    let mut buf = Vec::new();
    // Big files are only searched in their first 64 MB.
    if f.take(64 * 1024 * 1024).read_to_end(&mut buf).is_err() {
        return false;
    }
    re.is_match(&buf)
}

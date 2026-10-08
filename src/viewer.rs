//! Lister (F3) and the quick-view panel (Ctrl+Q).

use crate::fsutil;
use eframe::egui::{self, RichText};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Hex,
    Image,
}

pub struct Content {
    pub path: PathBuf,
    pub mode: Mode,
    bytes: Vec<u8>,
    text: String,
    truncated: bool,
    size: u64,
    error: Option<String>,
}

fn is_image(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico"
    )
}

impl Content {
    pub fn load(path: &Path) -> Self {
        let mut c = Content {
            path: path.to_path_buf(),
            mode: Mode::Text,
            bytes: Vec::new(),
            text: String::new(),
            truncated: false,
            size: 0,
            error: None,
        };
        if is_image(path) {
            c.mode = Mode::Image;
            c.size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            return c;
        }
        match std::fs::File::open(path) {
            Ok(f) => {
                c.size = f.metadata().map(|m| m.len()).unwrap_or(0);
                let mut buf = Vec::new();
                if let Err(e) = f.take(MAX_BYTES).read_to_end(&mut buf) {
                    c.error = Some(e.to_string());
                }
                c.truncated = c.size > MAX_BYTES;
                let binary = buf.iter().take(8192).any(|&b| b == 0);
                c.mode = if binary { Mode::Hex } else { Mode::Text };
                c.text = String::from_utf8_lossy(&buf).into_owned();
                c.bytes = buf;
            }
            Err(e) => c.error = Some(e.to_string()),
        }
        c
    }

    fn hex_line(&self, line: usize) -> String {
        let start = line * 16;
        let end = (start + 16).min(self.bytes.len());
        let chunk = &self.bytes[start..end];
        let mut s = format!("{start:08X}  ");
        for i in 0..16 {
            if i < chunk.len() {
                s.push_str(&format!("{:02X} ", chunk[i]));
            } else {
                s.push_str("   ");
            }
            if i == 7 {
                s.push(' ');
            }
        }
        s.push(' ');
        for &b in chunk {
            s.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
        }
        s
    }

    /// Render the content into `ui`. `wrap` only applies to text mode.
    pub fn show(&self, ui: &mut egui::Ui, wrap: bool, id: &str) {
        if let Some(e) = &self.error {
            ui.colored_label(egui::Color32::RED, e);
            return;
        }
        match self.mode {
            Mode::Image => {
                let uri = format!("file://{}", self.path.to_string_lossy());
                egui::ScrollArea::both().id_salt(id).show(ui, |ui| {
                    ui.add(
                        egui::Image::new(uri)
                            .max_size(ui.available_size())
                            .maintain_aspect_ratio(true)
                            .fit_to_exact_size(ui.available_size()),
                    );
                });
            }
            Mode::Text => {
                let lines: Vec<&str> = self.text.lines().collect();
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                if wrap {
                    egui::ScrollArea::vertical()
                        .id_salt(id)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(&self.text).monospace()).wrap());
                        });
                } else {
                    egui::ScrollArea::both()
                        .id_salt(id)
                        .auto_shrink([false, false])
                        .show_rows(ui, row_h, lines.len(), |ui, range| {
                            for l in &lines[range] {
                                ui.add(
                                    egui::Label::new(RichText::new(l.replace('\t', "    ")).monospace())
                                        .extend(),
                                );
                            }
                        });
                }
            }
            Mode::Hex => {
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                let n = self.bytes.len().div_ceil(16);
                egui::ScrollArea::both()
                    .id_salt(id)
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, n, |ui, range| {
                        for l in range {
                            ui.add(egui::Label::new(RichText::new(self.hex_line(l)).monospace()).extend());
                        }
                    });
            }
        }
    }

    pub fn info(&self) -> String {
        let mut s = format!("{}  ·  {}", self.path.to_string_lossy(), fsutil::format_size_short(self.size));
        if self.truncated {
            s.push_str(&format!("  ·  nur erste {} angezeigt", fsutil::format_size_short(MAX_BYTES)));
        }
        s
    }
}

/// Loads a file on a worker thread.
struct Loading {
    path: PathBuf,
    rx: std::sync::mpsc::Receiver<Content>,
    since: std::time::Instant,
}

impl Loading {
    fn start(path: &Path) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(Content::load(&p));
            fsutil::wake_ui();
        });
        Loading { path: path.to_path_buf(), rx, since: std::time::Instant::now() }
    }

    fn spinner(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!(
                "Lade {} … {:.0} s",
                self.path.file_name().unwrap_or_default().to_string_lossy(),
                self.since.elapsed().as_secs_f32().floor()
            ));
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
}

pub struct Viewer {
    pub id: u64,
    pub path: PathBuf,
    /// `None` while the file is still being read.
    pub content: Option<Content>,
    loading: Option<Loading>,
    pub wrap: bool,
    pub open: bool,
}

impl Viewer {
    pub fn new(id: u64, path: &Path) -> Self {
        Viewer {
            id,
            path: path.to_path_buf(),
            content: None,
            loading: Some(Loading::start(path)),
            wrap: false,
            open: true,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if let Some(l) = &self.loading
            && let Ok(c) = l.rx.try_recv()
        {
            self.content = Some(c);
            self.loading = None;
        }
        let title = format!("Lister – {}", self.path.file_name().unwrap_or_default().to_string_lossy());
        let mut open = self.open;
        egui::Window::new(title)
            .id(egui::Id::new(("viewer", self.id)))
            .open(&mut open)
            .default_size([900.0, 600.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let Some(content) = &mut self.content else {
                    if let Some(l) = &self.loading {
                        l.spinner(ui);
                    }
                    return;
                };
                ui.horizontal(|ui| {
                    if content.mode != Mode::Image {
                        ui.selectable_value(&mut content.mode, Mode::Text, "Text (1)");
                        ui.selectable_value(&mut content.mode, Mode::Hex, "Hex (3)");
                        ui.checkbox(&mut self.wrap, "Umbruch (W)");
                    }
                    if ui.button("Extern öffnen").clicked() {
                        let _ = fsutil::open_default(&content.path);
                    }
                    ui.label(RichText::new(content.info()).small());
                });
                ui.separator();
                content.show(ui, self.wrap, &format!("viewer_scroll_{}", self.id));
            });
        self.open = open;
    }
}

/// Quick view keeps the last loaded file so we don't re-read it every frame.
#[derive(Default)]
pub struct QuickView {
    cache: Option<Content>,
    loading: Option<Loading>,
}

impl QuickView {
    pub fn show(&mut self, ui: &mut egui::Ui, target: Option<&Path>, is_dir_info: Option<String>) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.strong("Schnellansicht (Strg+Q)");
            ui.separator();
            if let Some(info) = is_dir_info {
                ui.label(info);
                return;
            }
            let Some(path) = target else {
                ui.label("Keine Datei ausgewählt");
                return;
            };
            if let Some(l) = &self.loading
                && let Ok(c) = l.rx.try_recv()
            {
                if c.path == l.path {
                    self.cache = Some(c);
                }
                self.loading = None;
            }
            let have = self.cache.as_ref().is_some_and(|c| c.path == path);
            let pending = self.loading.as_ref().is_some_and(|l| l.path == path);
            if !have && !pending {
                // Cursor moved: load the new file (an older load is simply dropped).
                self.loading = Some(Loading::start(path));
            }
            match &self.cache {
                Some(c) if c.path == path => {
                    ui.label(RichText::new(c.info()).small());
                    c.show(ui, true, "quickview_scroll");
                }
                _ => {
                    if let Some(l) = &self.loading {
                        l.spinner(ui);
                    }
                }
            }
        });
    }
}

//! Compare two files by content (side by side, like Total Commander's
//! "Compare by content"). Text files get a line diff with highlighted
//! changes inside lines; binary files are compared byte by byte.

use crate::fsutil;
use eframe::egui::{self, Color32, RichText, text::LayoutJob};
use similar::{ChangeTag, DiffOp, TextDiff};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

/// Larger files are compared byte-wise only.
const MAX_TEXT: u64 = 20 * 1024 * 1024;

/// Text pieces of one line; `true` = changed part (emphasized).
pub type Segs = Vec<(bool, String)>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    Equal,
    /// Only in the left file.
    Delete,
    /// Only in the right file.
    Insert,
    /// Present on both sides but different.
    Change,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub kind: RowKind,
    /// (1-based line number, text)
    pub left: Option<(usize, Segs)>,
    pub right: Option<(usize, Segs)>,
}

#[derive(Debug)]
pub enum Outcome {
    Text {
        rows: Vec<Row>,
        /// Row index where each block of differences starts.
        changes: Vec<usize>,
    },
    Binary {
        size_a: u64,
        size_b: u64,
        /// Offset of the first differing byte, `None` = identical.
        first_diff: Option<u64>,
    },
    Error(String),
}

fn plain(s: &str) -> Segs {
    vec![(false, s.trim_end_matches(['\n', '\r']).to_string())]
}

fn looks_binary(p: &Path) -> std::io::Result<bool> {
    let mut buf = vec![0u8; 8192];
    let n = std::fs::File::open(p)?.read(&mut buf)?;
    Ok(buf[..n].contains(&0))
}

fn compare_bytes(a: &Path, b: &Path) -> std::io::Result<Option<u64>> {
    let mut fa = std::io::BufReader::new(std::fs::File::open(a)?);
    let mut fb = std::io::BufReader::new(std::fs::File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    let mut offset = 0u64;
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        let n = na.min(nb);
        if let Some(i) = (0..n).find(|&i| ba[i] != bb[i]) {
            return Ok(Some(offset + i as u64));
        }
        if na != nb {
            return Ok(Some(offset + n as u64)); // one file is longer
        }
        if na == 0 {
            return Ok(None);
        }
        offset += n as u64;
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Byte-for-byte identical? (Used by "synchronize by content".)
pub fn files_identical(a: &Path, b: &Path) -> bool {
    matches!(compare_bytes(a, b), Ok(None))
}

/// Side-by-side rows for two texts.
pub fn diff_texts(a: &str, b: &str) -> (Vec<Row>, Vec<usize>) {
    let diff = TextDiff::from_lines(a, b);
    let old = |i: usize| diff.old_slice(i).unwrap_or("");
    let new = |i: usize| diff.new_slice(i).unwrap_or("");
    let mut rows = Vec::new();
    let mut changes = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { old_index, new_index, len } => {
                for i in 0..len {
                    rows.push(Row {
                        kind: RowKind::Equal,
                        left: Some((old_index + i + 1, plain(old(old_index + i)))),
                        right: Some((new_index + i + 1, plain(new(new_index + i)))),
                    });
                }
            }
            DiffOp::Delete { old_index, old_len, .. } => {
                changes.push(rows.len());
                for i in 0..old_len {
                    rows.push(Row { kind: RowKind::Delete, left: Some((old_index + i + 1, plain(old(old_index + i)))), right: None });
                }
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                changes.push(rows.len());
                for i in 0..new_len {
                    rows.push(Row { kind: RowKind::Insert, left: None, right: Some((new_index + i + 1, plain(new(new_index + i)))) });
                }
            }
            DiffOp::Replace { .. } => {
                changes.push(rows.len());
                // Inline changes give the differing parts inside each line.
                let mut lefts: Vec<(usize, Segs)> = Vec::new();
                let mut rights: Vec<(usize, Segs)> = Vec::new();
                for change in diff.iter_inline_changes(op) {
                    let segs: Segs = change
                        .iter_strings_lossy()
                        .map(|(emph, s)| (emph, s.trim_end_matches(['\n', '\r']).to_string()))
                        .filter(|(_, s)| !s.is_empty())
                        .collect();
                    match change.tag() {
                        ChangeTag::Delete => lefts.push((change.old_index().unwrap_or(0) + 1, segs)),
                        ChangeTag::Insert => rights.push((change.new_index().unwrap_or(0) + 1, segs)),
                        ChangeTag::Equal => {}
                    }
                }
                let n = lefts.len().max(rights.len());
                let (mut li, mut ri) = (lefts.into_iter(), rights.into_iter());
                for _ in 0..n {
                    let (l, r) = (li.next(), ri.next());
                    let kind = match (&l, &r) {
                        (Some(_), Some(_)) => RowKind::Change,
                        (Some(_), None) => RowKind::Delete,
                        _ => RowKind::Insert,
                    };
                    rows.push(Row { kind, left: l, right: r });
                }
            }
        }
    }
    (rows, changes)
}

pub fn compute(a: &Path, b: &Path) -> Outcome {
    let run = || -> std::io::Result<Outcome> {
        let (sa, sb) = (std::fs::metadata(a)?.len(), std::fs::metadata(b)?.len());
        if sa > MAX_TEXT || sb > MAX_TEXT || looks_binary(a)? || looks_binary(b)? {
            return Ok(Outcome::Binary { size_a: sa, size_b: sb, first_diff: compare_bytes(a, b)? });
        }
        let ta = String::from_utf8_lossy(&std::fs::read(a)?).into_owned();
        let tb = String::from_utf8_lossy(&std::fs::read(b)?).into_owned();
        let (rows, changes) = diff_texts(&ta, &tb);
        Ok(Outcome::Text { rows, changes })
    };
    run().unwrap_or_else(|e| Outcome::Error(e.to_string()))
}

pub struct CompareWindow {
    pub id: u64,
    pub a: PathBuf,
    pub b: PathBuf,
    pub open: bool,
    outcome: Option<Outcome>,
    rx: Option<Receiver<Outcome>>,
    since: std::time::Instant,
    /// Index into `changes` of the current difference.
    current: usize,
    scroll_to: Option<usize>,
    only_diffs: bool,
}

impl CompareWindow {
    pub fn new(id: u64, a: PathBuf, b: PathBuf) -> Self {
        let mut w = CompareWindow {
            id,
            a,
            b,
            open: true,
            outcome: None,
            rx: None,
            since: std::time::Instant::now(),
            current: 0,
            scroll_to: None,
            only_diffs: false,
        };
        w.start();
        w
    }

    fn start(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (a, b) = (self.a.clone(), self.b.clone());
        std::thread::spawn(move || {
            let _ = tx.send(compute(&a, &b));
            fsutil::wake_ui();
        });
        self.rx = Some(rx);
        self.outcome = None;
        self.since = std::time::Instant::now();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }

    /// Jump to the next (`+1`) or previous (`-1`) difference.
    pub fn jump(&mut self, delta: isize) {
        if let Some(Outcome::Text { changes, .. }) = &self.outcome
            && !changes.is_empty()
        {
            let n = changes.len() as isize;
            self.current = (self.current as isize + delta).rem_euclid(n) as usize;
            self.scroll_to = Some(changes[self.current]);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, editor: &str) {
        if let Some(rx) = &self.rx
            && let Ok(o) = rx.try_recv()
        {
            self.rx = None;
            if let Outcome::Text { changes, .. } = &o
                && let Some(&first) = changes.first()
            {
                self.current = 0;
                self.scroll_to = Some(first);
            }
            self.outcome = Some(o);
        }
        let title = format!(
            "Vergleich – {} ↔ {}",
            self.a.file_name().unwrap_or_default().to_string_lossy(),
            self.b.file_name().unwrap_or_default().to_string_lossy()
        );
        let mut open = self.open;
        let mut rerun = false;
        let mut jump = 0isize;
        egui::Window::new(title)
            .id(egui::Id::new(("compare", self.id)))
            .open(&mut open)
            .default_size([1100.0, 650.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("◀ Vorige").on_hover_text("Vorige Änderung (P)").clicked() {
                        jump = -1;
                    }
                    if ui.button("Nächste ▶").on_hover_text("Nächste Änderung (N)").clicked() {
                        jump = 1;
                    }
                    ui.checkbox(&mut self.only_diffs, "Nur Unterschiede");
                    if ui.button("🔄 Neu vergleichen").clicked() {
                        rerun = true;
                    }
                    if ui.button("Links bearbeiten").clicked() {
                        let _ = fsutil::open_with(editor, &self.a);
                    }
                    if ui.button("Rechts bearbeiten").clicked() {
                        let _ = fsutil::open_with(editor, &self.b);
                    }
                    let summary = match &self.outcome {
                        Some(Outcome::Text { changes, .. }) if changes.is_empty() => "✔ Inhalt identisch".to_string(),
                        Some(Outcome::Text { changes, .. }) => {
                            format!("{} Unterschied(e) · {} / {}", changes.len(), self.current + 1, changes.len())
                        }
                        _ => String::new(),
                    };
                    ui.strong(summary);
                });
                ui.columns(2, |c| {
                    c[0].add(egui::Label::new(RichText::new(self.a.to_string_lossy()).small().monospace()).truncate());
                    c[1].add(egui::Label::new(RichText::new(self.b.to_string_lossy()).small().monospace()).truncate());
                });
                ui.separator();
                match &self.outcome {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Vergleiche… {:.0} s", self.since.elapsed().as_secs_f32().floor()));
                        });
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                    }
                    Some(Outcome::Error(e)) => {
                        ui.colored_label(Color32::from_rgb(200, 60, 60), e);
                    }
                    Some(Outcome::Binary { size_a, size_b, first_diff }) => {
                        ui.label("Binärdateien (oder sehr groß) – byteweiser Vergleich:");
                        ui.label(format!(
                            "Links: {} Bytes · Rechts: {} Bytes",
                            fsutil::format_size(*size_a),
                            fsutil::format_size(*size_b)
                        ));
                        match first_diff {
                            None => ui.strong("✔ Inhalt identisch"),
                            Some(o) => ui.strong(format!("✖ Unterschiedlich ab Byte {} (0x{o:X})", fsutil::format_size(*o))),
                        };
                    }
                    Some(Outcome::Text { rows, .. }) => {
                        let visible: Vec<usize> = if self.only_diffs {
                            (0..rows.len()).filter(|&i| rows[i].kind != RowKind::Equal).collect()
                        } else {
                            (0..rows.len()).collect()
                        };
                        draw_rows(ui, rows, &visible, self.scroll_to.take(), self.id);
                    }
                }
            });
        if jump != 0 {
            self.jump(jump);
        }
        if rerun {
            self.start();
        }
        self.open = open;
    }
}

fn row_colors(kind: RowKind, dark: bool) -> (Color32, Color32) {
    let a = if dark { 60 } else { 70 };
    match kind {
        RowKind::Equal => (Color32::TRANSPARENT, Color32::TRANSPARENT),
        RowKind::Delete => (Color32::from_rgba_unmultiplied(220, 60, 60, a), Color32::from_rgba_unmultiplied(128, 128, 128, 25)),
        RowKind::Insert => (Color32::from_rgba_unmultiplied(128, 128, 128, 25), Color32::from_rgba_unmultiplied(60, 180, 80, a)),
        RowKind::Change => (Color32::from_rgba_unmultiplied(230, 180, 40, a), Color32::from_rgba_unmultiplied(230, 180, 40, a)),
    }
}

fn line_job(ui: &egui::Ui, segs: &Segs, emph_bg: Color32) -> LayoutJob {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let color = ui.visuals().text_color();
    let mut job = LayoutJob::default();
    for (emph, text) in segs {
        let mut fmt = egui::TextFormat::simple(font.clone(), color);
        if *emph {
            fmt.background = emph_bg;
        }
        job.append(&text.replace('\t', "    "), 0.0, fmt);
    }
    job.wrap.max_rows = 1;
    job
}

fn draw_rows(ui: &mut egui::Ui, rows: &[Row], visible: &[usize], scroll_to: Option<usize>, id: u64) {
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
    let dark = ui.visuals().dark_mode;
    let emph = if dark { Color32::from_rgba_unmultiplied(255, 120, 0, 110) } else { Color32::from_rgba_unmultiplied(255, 140, 0, 120) };
    let mut area = egui::ScrollArea::both().id_salt(("compare_rows", id)).auto_shrink([false, false]);
    if let Some(target) = scroll_to
        && let Some(pos) = visible.iter().position(|&i| i >= target)
    {
        // A few lines of context above the difference.
        area = area.vertical_scroll_offset((pos.saturating_sub(3)) as f32 * row_h);
    }
    area.show_rows(ui, row_h, visible.len(), |ui, range| {
        let full_w = ui.available_width().max(400.0);
        let half = full_w / 2.0;
        let num_w = 48.0;
        for vi in range {
            let row = &rows[visible[vi]];
            let (rect, _) = ui.allocate_exact_size(egui::vec2(full_w, row_h), egui::Sense::hover());
            let (bg_l, bg_r) = row_colors(row.kind, dark);
            let left_rect = egui::Rect::from_min_size(rect.min, egui::vec2(half - 2.0, row_h));
            let right_rect = egui::Rect::from_min_size(rect.min + egui::vec2(half + 2.0, 0.0), egui::vec2(half - 2.0, row_h));
            let painter = ui.painter_at(rect);
            for (r, bg, side) in [(left_rect, bg_l, &row.left), (right_rect, bg_r, &row.right)] {
                painter.rect_filled(r, 0.0, bg);
                if let Some((no, segs)) = side {
                    painter.text(
                        r.min + egui::vec2(num_w - 6.0, 1.0),
                        egui::Align2::RIGHT_TOP,
                        no.to_string(),
                        egui::TextStyle::Monospace.resolve(ui.style()),
                        ui.visuals().weak_text_color(),
                    );
                    let galley = ui.fonts_mut(|f| f.layout_job(line_job(ui, segs, emph)));
                    let text_rect = egui::Rect::from_min_max(r.min + egui::vec2(num_w, 0.0), r.max);
                    ui.painter_at(text_rect).galley(text_rect.min + egui::vec2(0.0, 1.0), galley, ui.visuals().text_color());
                }
            }
            painter.vline(rect.center().x, rect.y_range(), ui.visuals().widgets.noninteractive.bg_stroke);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_by_side_rows() {
        let a = "eins\nzwei\ndrei\nvier\n";
        let b = "eins\nZWEI!\ndrei\nfünf\nsechs\n";
        let (rows, changes) = diff_texts(a, b);
        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, [RowKind::Equal, RowKind::Change, RowKind::Equal, RowKind::Change, RowKind::Insert]);
        assert_eq!(changes.len(), 2);
        // The changed line knows which part differs.
        let (no, segs) = rows[1].right.as_ref().unwrap();
        assert_eq!(*no, 2);
        assert!(segs.iter().any(|(emph, s)| *emph && s.contains("ZWEI")));
        assert_eq!(rows[4].right.as_ref().unwrap().0, 5);
        assert!(diff_texts("x\n", "x\n").1.is_empty());
    }

    #[test]
    fn binary_and_text_files() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a.bin"), d.path().join("b.bin"), d.path().join("c.bin"));
        std::fs::write(&a, [0u8, 1, 2, 3, 4]).unwrap();
        std::fs::write(&b, [0u8, 1, 9, 3, 4]).unwrap();
        std::fs::write(&c, [0u8, 1, 2, 3, 4]).unwrap();
        assert!(matches!(compute(&a, &b), Outcome::Binary { first_diff: Some(2), .. }));
        assert!(matches!(compute(&a, &c), Outcome::Binary { first_diff: None, .. }));
        let (t1, t2) = (d.path().join("1.txt"), d.path().join("2.txt"));
        std::fs::write(&t1, "a\nb\n").unwrap();
        std::fs::write(&t2, "a\nc\n").unwrap();
        assert!(matches!(compute(&t1, &t2), Outcome::Text { ref changes, .. } if changes.len() == 1));
    }
}

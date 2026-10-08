//! Multi-rename tool (Ctrl+M).

use crate::fsutil::split_ext;
use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CaseMode {
    Keep,
    Lower,
    Upper,
    FirstUpper,
}

pub struct MultiRename {
    pub open: bool,
    files: Vec<(PathBuf, bool)>,
    /// Modification times, read once (not every frame) for [Y][M][D].
    mtimes: Vec<Option<std::time::SystemTime>>,
    name_mask: String,
    ext_mask: String,
    search: String,
    replace: String,
    use_regex: bool,
    case: CaseMode,
    counter_start: i64,
    counter_step: i64,
    counter_digits: usize,
    message: Option<(bool, String)>,
}

impl MultiRename {
    pub fn new() -> Self {
        MultiRename {
            open: false,
            files: Vec::new(),
            mtimes: Vec::new(),
            name_mask: "[N]".into(),
            ext_mask: "[E]".into(),
            search: String::new(),
            replace: String::new(),
            use_regex: false,
            case: CaseMode::Keep,
            counter_start: 1,
            counter_step: 1,
            counter_digits: 1,
            message: None,
        }
    }

    /// `files`: (path, is_dir)
    pub fn start(&mut self, files: Vec<(PathBuf, bool)>) {
        self.mtimes = files
            .iter()
            .map(|(p, _)| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .collect();
        self.files = files;
        self.open = true;
        self.message = None;
    }

    fn apply_mask(&self, mask: &str, stem: &str, ext: &str, counter: i64, parent: &str, mtime: Option<std::time::SystemTime>) -> String {
        let token = regex::Regex::new(r"\[(N|E)(\d+)?(?:-(\d+))?\]|\[C\]|\[P\]|\[Y\]|\[M\]|\[D\]").unwrap();
        let date: Option<chrono::DateTime<chrono::Local>> = mtime.map(Into::into);
        token
            .replace_all(mask, |caps: &regex::Captures| {
                let whole = &caps[0];
                match whole {
                    "[C]" => format!("{:0width$}", counter, width = self.counter_digits),
                    "[P]" => parent.to_string(),
                    "[Y]" => date.map(|d| d.format("%Y").to_string()).unwrap_or_default(),
                    "[M]" => date.map(|d| d.format("%m").to_string()).unwrap_or_default(),
                    "[D]" => date.map(|d| d.format("%d").to_string()).unwrap_or_default(),
                    _ => {
                        let src = if &caps[1] == "N" { stem } else { ext };
                        let chars: Vec<char> = src.chars().collect();
                        match caps.get(2) {
                            None => src.to_string(),
                            Some(from) => {
                                // [N2-5] = characters 2..5 (1-based), [N3] = just char 3
                                let a = from.as_str().parse::<usize>().unwrap_or(1).max(1) - 1;
                                let b = caps
                                    .get(3)
                                    .and_then(|m| m.as_str().parse::<usize>().ok())
                                    .unwrap_or(a + 1)
                                    .min(chars.len());
                                if a >= b { String::new() } else { chars[a..b].iter().collect() }
                            }
                        }
                    }
                }
            })
            .into_owned()
    }

    fn new_name(&self, idx: usize) -> Result<String, String> {
        let (path, is_dir) = &self.files[idx];
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let (stem, ext) = split_ext(&name, *is_dir);
        let counter = self.counter_start + self.counter_step * idx as i64;
        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mtime = self.mtimes.get(idx).copied().flatten();
        let new_stem = self.apply_mask(&self.name_mask, stem, ext, counter, &parent, mtime);
        let new_ext = self.apply_mask(&self.ext_mask, stem, ext, counter, &parent, mtime);
        let mut out = if new_ext.is_empty() {
            new_stem
        } else {
            format!("{new_stem}.{new_ext}")
        };
        if !self.search.is_empty() {
            out = if self.use_regex {
                let re = regex::Regex::new(&self.search).map_err(|e| e.to_string())?;
                re.replace_all(&out, self.replace.as_str()).into_owned()
            } else {
                out.replace(&self.search, &self.replace)
            };
        }
        out = match self.case {
            CaseMode::Keep => out,
            CaseMode::Lower => out.to_lowercase(),
            CaseMode::Upper => out.to_uppercase(),
            CaseMode::FirstUpper => {
                let lower = out.to_lowercase();
                let mut c = lower.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => lower,
                }
            }
        };
        if out.is_empty() || out.contains('/') {
            return Err("ungültiger Name".into());
        }
        Ok(out)
    }

    fn execute(&mut self) -> Result<usize, String> {
        let mut plan = Vec::new();
        for i in 0..self.files.len() {
            let new = self.new_name(i)?;
            let src = self.files[i].0.clone();
            let dst = src.with_file_name(&new);
            if dst != src {
                plan.push((src, dst));
            }
        }
        let mut targets: Vec<&PathBuf> = plan.iter().map(|(_, d)| d).collect();
        targets.sort();
        if targets.windows(2).any(|w| w[0] == w[1]) {
            return Err("Mehrere Dateien bekämen denselben Namen".into());
        }
        let sources: std::collections::HashSet<&PathBuf> = plan.iter().map(|(s, _)| s).collect();
        for (_, d) in &plan {
            if d.exists() && !sources.contains(d) {
                return Err(format!("{} existiert bereits", d.to_string_lossy()));
            }
        }
        // Two phases so that swaps (a→b, b→a) work.
        let mut temps = Vec::new();
        for (i, (src, dst)) in plan.iter().enumerate() {
            let tmp = src.with_file_name(format!(".miery_rename_{}_{i}", std::process::id()));
            std::fs::rename(src, &tmp).map_err(|e| format!("{}: {e}", src.to_string_lossy()))?;
            temps.push((tmp, dst.clone()));
        }
        for (tmp, dst) in &temps {
            std::fs::rename(tmp, dst).map_err(|e| format!("{}: {e}", dst.to_string_lossy()))?;
        }
        for (i, (_, dst)) in plan.iter().enumerate() {
            if let Some(f) = self.files.iter_mut().find(|f| f.0 == plan[i].0) {
                f.0 = dst.clone();
            }
        }
        Ok(plan.len())
    }

    /// Returns true when files were renamed (panels should reload).
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        if !self.open {
            return false;
        }
        let mut changed = false;
        let mut open = self.open;
        egui::Window::new("Mehrfach-Umbenennen")
            .open(&mut open)
            .default_size([800.0, 520.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::Grid::new("mr_grid").num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Name-Maske:");
                    ui.text_edit_singleline(&mut self.name_mask);
                    ui.label("Suchen:");
                    ui.text_edit_singleline(&mut self.search);
                    ui.end_row();
                    ui.label("Erweiterung:");
                    ui.text_edit_singleline(&mut self.ext_mask);
                    ui.label("Ersetzen:");
                    ui.text_edit_singleline(&mut self.replace);
                    ui.end_row();
                    ui.label("Zähler ab / Schritt / Stellen:");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.counter_start));
                        ui.add(egui::DragValue::new(&mut self.counter_step));
                        ui.add(egui::DragValue::new(&mut self.counter_digits).range(1..=10));
                    });
                    ui.checkbox(&mut self.use_regex, "RegEx");
                    egui::ComboBox::from_id_salt("mr_case")
                        .selected_text(match self.case {
                            CaseMode::Keep => "Groß/klein unverändert",
                            CaseMode::Lower => "kleinbuchstaben",
                            CaseMode::Upper => "GROSSBUCHSTABEN",
                            CaseMode::FirstUpper => "Erster groß",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.case, CaseMode::Keep, "Groß/klein unverändert");
                            ui.selectable_value(&mut self.case, CaseMode::Lower, "kleinbuchstaben");
                            ui.selectable_value(&mut self.case, CaseMode::Upper, "GROSSBUCHSTABEN");
                            ui.selectable_value(&mut self.case, CaseMode::FirstUpper, "Erster groß");
                        });
                    ui.end_row();
                });
                ui.label(
                    RichText::new("Platzhalter: [N] Name, [N2-5] Zeichen 2–5, [E] Erweiterung, [C] Zähler, [P] Ordner, [Y] [M] [D] Datum")
                        .small()
                        .weak(),
                );
                ui.separator();
                let previews: Vec<Result<String, String>> = (0..self.files.len()).map(|i| self.new_name(i)).collect();
                let table_h = ui.available_height() - 40.0;
                TableBuilder::new(ui)
                    .id_salt("mr_table")
                    .striped(true)
                    .column(Column::remainder().at_least(200.0).clip(true))
                    .column(Column::remainder().at_least(200.0).clip(true))
                    .min_scrolled_height(table_h)
                    .max_scroll_height(table_h)
                    .auto_shrink([false, false])
                    .header(20.0, |mut h| {
                        h.col(|ui| {
                            ui.strong("Alter Name");
                        });
                        h.col(|ui| {
                            ui.strong("Neuer Name");
                        });
                    })
                    .body(|body| {
                        body.rows(18.0, self.files.len(), |mut row| {
                            let i = row.index();
                            let old = self.files[i].0.file_name().unwrap_or_default().to_string_lossy().into_owned();
                            row.col(|ui| {
                                ui.label(&old);
                            });
                            row.col(|ui| match &previews[i] {
                                Ok(n) if *n != old => {
                                    ui.colored_label(Color32::from_rgb(40, 150, 60), n);
                                }
                                Ok(n) => {
                                    ui.label(n);
                                }
                                Err(e) => {
                                    ui.colored_label(Color32::RED, e);
                                }
                            });
                        });
                    });
                ui.horizontal(|ui| {
                    if ui.button("▶ Umbenennen").clicked() {
                        self.message = Some(match self.execute() {
                            Ok(n) => {
                                changed = true;
                                (true, format!("{n} Datei(en) umbenannt"))
                            }
                            Err(e) => (false, e),
                        });
                    }
                    if let Some((ok, msg)) = &self.message {
                        let c = if *ok { Color32::from_rgb(40, 150, 60) } else { Color32::RED };
                        ui.colored_label(c, msg);
                    }
                });
            });
        self.open = open;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_counter_and_swap() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["Urlaub 1.JPG", "Urlaub 2.JPG"] {
            std::fs::write(dir.path().join(n), n).unwrap();
        }
        let mut mr = MultiRename::new();
        mr.start(vec![
            (dir.path().join("Urlaub 1.JPG"), false),
            (dir.path().join("Urlaub 2.JPG"), false),
        ]);
        mr.name_mask = "Foto_[C]_[N1-3]".into();
        mr.counter_digits = 3;
        mr.case = CaseMode::Lower;
        assert_eq!(mr.new_name(0).unwrap(), "foto_001_url.jpg");
        assert_eq!(mr.new_name(1).unwrap(), "foto_002_url.jpg");
        assert_eq!(mr.execute().unwrap(), 2);
        assert!(dir.path().join("foto_002_url.jpg").exists());

        // Swapping two names must work thanks to the two-phase rename.
        let a = dir.path().join("foto_001_url.jpg");
        let b = dir.path().join("foto_002_url.jpg");
        let mut mr = MultiRename::new();
        mr.start(vec![(a.clone(), false), (b.clone(), false)]);
        mr.use_regex = true;
        mr.search = "00(1|2)".into();
        mr.replace = "x$1".into();
        mr.execute().unwrap();
        mr.files = vec![(dir.path().join("foto_x1_url.jpg"), false), (dir.path().join("foto_x2_url.jpg"), false)];
        mr.search = "x1".into();
        mr.replace = "x2".into();
        mr.use_regex = false;
        // Both would become x2 for the first one only -> duplicate is rejected.
        mr.files.truncate(1);
        assert!(mr.execute().is_err(), "must not clobber an existing file");
    }
}

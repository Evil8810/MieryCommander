//! Fallback fonts for file names in any script.
//!
//! egui's built-in fonts cover Latin, Cyrillic, Greek and some symbols. File
//! names can contain anything (Japanese, Chinese, Arabic, …), so we add the
//! system's fonts for other scripts as fallbacks. Loaded on a background
//! thread – the CJK font alone is ~20 MB.

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};
use std::path::PathBuf;
use std::sync::Arc;

/// Languages whose fonts we ask fontconfig for (Linux).
#[cfg(target_os = "linux")]
const LANGS: &[&str] = &[
    "ja", "zh-cn", "zh-tw", "ko", "ar", "he", "th", "hi", "bn", "ta", "te", "ka", "hy", "am", "km", "lo", "my", "si", "ru",
    "el", "vi",
];

fn candidate_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let query = |pattern: &str| -> Option<PathBuf> {
            let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
            let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            p.is_file().then_some(p)
        };
        files.extend(query("sans"));
        for lang in LANGS {
            files.extend(query(&format!("sans:lang={lang}")));
        }
        for family in ["Noto Sans Symbols", "Noto Sans Symbols 2", "Noto Sans Math", "DejaVu Sans"] {
            files.extend(query(family));
        }
    }
    #[cfg(target_os = "macos")]
    {
        for f in [
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
            "/Library/Fonts/Arial Unicode.ttf",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/Apple Symbols.ttf",
        ] {
            files.push(PathBuf::from(f));
        }
    }
    // Keep order, drop duplicates and missing files. Colour emoji fonts are
    // bitmap fonts egui can't draw, so skip them.
    let mut seen = std::collections::HashSet::new();
    files.retain(|f| f.is_file() && !f.to_string_lossy().contains("Emoji") && seen.insert(f.clone()));
    files
}

/// Font family for folder names (bold when the system has a bold font).
pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// egui's defaults plus the "bold" family (until a bold font is loaded it
/// uses the normal font, so it always exists).
pub fn defaults() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let normal = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    defs.families.insert(bold(), normal);
    defs
}

/// A bold sans-serif font of the system.
fn bold_file() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let out = std::process::Command::new("fc-match").args(["-f", "%{file}", "sans:bold"]).output().ok()?;
        let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        return p.is_file().then_some(p);
    }
    #[cfg(target_os = "macos")]
    {
        return ["/System/Library/Fonts/Supplemental/Arial Bold.ttf", "/Library/Fonts/Arial Bold.ttf"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file());
    }
    #[allow(unreachable_code)]
    None
}

/// Font definitions = egui defaults + system fallbacks for other scripts.
pub fn with_fallbacks() -> FontDefinitions {
    let mut defs = defaults();
    if let Some(bytes) = bold_file().and_then(|f| std::fs::read(f).ok()) {
        defs.font_data.insert("system-bold".into(), Arc::new(FontData::from_owned(bytes)));
        defs.families.entry(bold()).or_default().insert(0, "system-bold".into());
    }
    for (i, file) in candidate_files().into_iter().enumerate() {
        let Ok(bytes) = std::fs::read(&file) else { continue };
        let name = format!("system-fallback-{i}");
        defs.font_data.insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace, bold()] {
            defs.families.entry(family).or_default().push(name.clone());
        }
    }
    defs
}

/// Load the fallbacks in the background and switch to them when ready.
pub fn install_in_background(ctx: &egui::Context) {
    ctx.set_fonts(defaults());
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let defs = with_fallbacks();
        ctx.set_fonts(defs);
        ctx.request_repaint();
    });
}

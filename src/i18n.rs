//! User interface language (German / English).
//!
//! Every UI text is written with both languages side by side:
//! `l!("Kopieren", "Copy")` for fixed texts and
//! `lf!("{n} Dateien", "{n} files")` for texts with values. The compiler
//! therefore guarantees that no text is left without a translation.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
pub enum LangChoice {
    /// Follow the operating system's language.
    #[default]
    System,
    German,
    English,
}

static ENGLISH: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    /// Per-thread language for tests, so an English test can't disturb the
    /// German ones running in parallel.
    static TEST_ENGLISH: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Tests: use English (`Some(true)`), German (`Some(false)`) or the global setting on this thread.
#[cfg(test)]
pub fn set_test_lang(english: Option<bool>) {
    TEST_ENGLISH.with(|c| c.set(english));
}

/// Is the UI currently English?
pub fn en() -> bool {
    #[cfg(test)]
    if let Some(v) = TEST_ENGLISH.with(|c| c.get()) {
        return v;
    }
    ENGLISH.load(Ordering::Relaxed)
}

/// Does the operating system prefer German?
pub fn system_is_german() -> bool {
    if cfg!(test) {
        return true; // tests check German texts – independent of the machine
    }
    sys_locale::get_locale().is_some_and(|l| l.to_lowercase().starts_with("de"))
}

pub fn apply(choice: LangChoice) {
    let english = match choice {
        LangChoice::System => !system_is_german(),
        LangChoice::German => false,
        LangChoice::English => true,
    };
    // Tests run in parallel: switching there only affects the test's own thread.
    #[cfg(test)]
    set_test_lang(Some(english));
    #[cfg(not(test))]
    ENGLISH.store(english, Ordering::Relaxed);
}

/// A fixed text in both languages: `l!("Kopieren", "Copy")`.
#[macro_export]
macro_rules! l {
    ($de:literal, $en:literal $(,)?) => {
        if $crate::i18n::en() { $en } else { $de }
    };
}

/// A formatted text in both languages (same arguments for both):
/// `lf!("{n} Dateien kopiert", "{n} files copied")`.
#[macro_export]
macro_rules! lf {
    ($de:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::en() { format!($en $(, $arg)*) } else { format!($de $(, $arg)*) }
    };
}

/// Key names in shortcuts: "Strg+Shift+Entf" → "Ctrl+Shift+Del" in English.
pub fn keys(s: &str) -> String {
    if !en() {
        return s.to_string();
    }
    s.replace("Strg", "Ctrl")
        .replace("Einfg", "Ins")
        .replace("Entf", "Del")
        .replace("Leertaste", "Space")
        .replace("Pos1", "Home")
        .replace("Rechtsklick", "Right-click")
        .replace("Buchstaben tippen", "Type letters")
        .replace("Klick", "click")
        .replace("Knopf „=“", "“=” button")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_languages() {
        set_test_lang(Some(true));
        assert_eq!(l!("Kopieren", "Copy"), "Copy");
        let n = 3;
        assert_eq!(lf!("{n} Dateien", "{n} files"), "3 files");
        assert_eq!(keys("Strg+Shift+Entf"), "Ctrl+Shift+Del");
        set_test_lang(Some(false));
        assert_eq!(l!("Kopieren", "Copy"), "Kopieren");
        assert_eq!(lf!("{} Dateien", "{} files", 2), "2 Dateien");
        assert_eq!(keys("Strg+C"), "Strg+C");
        set_test_lang(None);
    }
}

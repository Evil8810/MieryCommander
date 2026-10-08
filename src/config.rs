use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

/// Everything that survives a restart.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub show_hidden: bool,
    pub dirs_first: bool,
    pub delete_to_trash: bool,
    pub confirm_delete: bool,
    /// Command used for F4. Empty = system default application.
    /// `{}` is replaced by the file path, otherwise the path is appended.
    pub editor: String,
    /// Command used for "Open terminal". Empty = auto-detect.
    pub terminal: String,
    pub theme: ThemeChoice,
    pub font_scale: f32,
    pub drive_bar: DriveBar,
    pub hotlist: Vec<PathBuf>,
    /// Saved FTP sites (passwords live in the OS keyring, never here).
    #[serde(alias = "ftp_sites")]
    pub sites: Vec<crate::remote::Site>,
    pub left_tabs: Vec<PathBuf>,
    pub right_tabs: Vec<PathBuf>,
    pub left_active: usize,
    pub right_active: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            show_hidden: false,
            dirs_first: true,
            delete_to_trash: true,
            confirm_delete: true,
            editor: String::new(),
            terminal: String::new(),
            theme: ThemeChoice::System,
            font_scale: 1.0,
            drive_bar: DriveBar::Both,
            hotlist: Vec::new(),
            sites: Vec::new(),
            left_tabs: Vec::new(),
            right_tabs: Vec::new(),
            left_active: 0,
            right_active: 0,
        }
    }
}

/// How the drive/location selector above each panel looks.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
pub enum DriveBar {
    /// Dropdown only.
    Dropdown,
    /// A row of buttons, one per drive.
    Buttons,
    /// Button row and dropdown, like Total Commander.
    #[default]
    Both,
}

pub const STORAGE_KEY: &str = "miery_config";

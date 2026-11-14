//! User preferences, persisted as `settings.json` in the app data dir.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model;
use crate::signatures;

/// Date formats offered for the Date tool (chrono `strftime` patterns).
pub const DATE_FORMATS: &[&str] = &["%d.%m.%Y", "%d/%m/%Y", "%m/%d/%Y", "%Y-%m-%d", "%-d %b %Y", "%B %-d, %Y"];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub date_format: String,
    /// Look for new versions about once a day (see `updater.rs`).
    pub check_for_updates: bool,
    /// Whether the tools and signatures (left) and the page thumbnails (right) are shown.
    pub show_left_sidebar: bool,
    pub show_right_sidebar: bool,
    /// What the highlighter and the pen were last set to (see `model::PEN_COLORS` and the like).
    pub highlight_color: u32,
    pub pen_color: u32,
    pub pen_width: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            date_format: DATE_FORMATS[0].to_string(),
            check_for_updates: true,
            show_left_sidebar: true,
            show_right_sidebar: true,
            highlight_color: model::HIGHLIGHT_COLORS[0].1,
            pen_color: model::PEN_COLORS[0].1,
            pen_width: model::PEN_WIDTHS[1].1,
        }
    }
}

fn path() -> PathBuf {
    signatures::app_data_dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        let mut settings: Self = std::fs::read(path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        // Ignore formats we no longer offer (e.g. a hand-edited file).
        if !DATE_FORMATS.contains(&settings.date_format.as_str()) {
            settings.date_format = Self::default().date_format;
        }
        if !model::HIGHLIGHT_COLORS.iter().any(|c| c.1 == settings.highlight_color) {
            settings.highlight_color = Self::default().highlight_color;
        }
        if !model::PEN_COLORS.iter().any(|c| c.1 == settings.pen_color) {
            settings.pen_color = Self::default().pen_color;
        }
        if !model::PEN_WIDTHS.iter().any(|w| w.1 == settings.pen_width) {
            settings.pen_width = Self::default().pen_width;
        }
        settings
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Today's date in the chosen format.
    pub fn today(&self) -> String {
        format_today(&self.date_format)
    }
}

pub fn format_today(format: &str) -> String {
    chrono::Local::now().format(format).to_string()
}

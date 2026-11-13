//! User preferences, persisted as `settings.json` in the app data dir.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::signatures;

/// Date formats offered for the Date tool (chrono `strftime` patterns).
pub const DATE_FORMATS: &[&str] = &["%d.%m.%Y", "%d/%m/%Y", "%m/%d/%Y", "%Y-%m-%d", "%-d %b %Y", "%B %-d, %Y"];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub date_format: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            date_format: DATE_FORMATS[0].to_string(),
        }
    }
}

fn path() -> PathBuf {
    signatures::app_data_dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        let settings: Self = std::fs::read(path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        // Ignore formats we no longer offer (e.g. a hand-edited file).
        if DATE_FORMATS.contains(&settings.date_format.as_str()) {
            settings
        } else {
            Self::default()
        }
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

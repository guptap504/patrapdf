//! Core data model shared by the UI and the PDF writer.
//!
//! All overlay geometry is stored in *view space*: PDF points, origin at the
//! top-left of the page as it is displayed (i.e. after the page's /Rotate and
//! crop box have been applied), y growing downwards. The writer converts view
//! space into PDF user space using each page's `view_to_user` matrix.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// The on-screen font for text fields. Saved PDFs use Helvetica; Arial (Windows)
/// and Liberation Sans (Linux) have the same metrics, so text lines up the same.
pub const TEXT_FONT: &str = if cfg!(target_os = "macos") {
    "Helvetica"
} else if cfg!(target_os = "windows") {
    "Arial"
} else {
    "Liberation Sans"
};

/// Line height used for text overlays, as a multiple of the font size.
pub const LINE_HEIGHT: f32 = 1.2;

/// Geometry of one page as it is displayed.
#[derive(Clone, Debug, PartialEq)]
pub struct PageGeom {
    /// Displayed width in points.
    pub width: f32,
    /// Displayed height in points.
    pub height: f32,
    /// Affine matrix `[a b c d e f]` mapping view space -> PDF user space.
    pub view_to_user: [f64; 6],
}

/// A signature (or any stamp image) from the signature library.
#[derive(Debug)]
pub struct Signature {
    /// PNG on disk with the (background-removed) signature.
    pub path: PathBuf,
    pub width_px: u32,
    pub height_px: u32,
}

impl Signature {
    pub fn aspect(&self) -> f32 {
        self.width_px as f32 / self.height_px.max(1) as f32
    }
}

#[derive(Clone, Debug)]
pub enum ItemKind {
    Text { text: String, font_size: f32 },
    Image { sig: Arc<Signature> },
    Check,
    /// A redaction box. On save, everything under it is removed from the file.
    Redact { fill: RedactFill },
}

/// The colour a redaction box covers things with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactFill {
    #[default]
    Black,
    /// For blanking out on white paper without a visible box.
    White,
}

impl RedactFill {
    /// The colour's value in 8-bit RGB.
    pub fn level(self) -> u8 {
        match self {
            RedactFill::Black => 0,
            RedactFill::White => 255,
        }
    }
}

/// An overlay placed on a page.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: u64,
    pub page: usize,
    /// Top-left corner in view space (points).
    pub x: f32,
    pub y: f32,
    /// Size in points. For text this is the box around its lines as measured
    /// (an estimate while the fonts it needs are still being found).
    pub w: f32,
    pub h: f32,
    pub kind: ItemKind,
}

impl Item {
    pub fn is_text(&self) -> bool {
        matches!(self.kind, ItemKind::Text { .. })
    }

    pub fn is_redaction(&self) -> bool {
        self.redaction().is_some()
    }

    /// The colour of a redaction box (nothing for other items).
    pub fn redaction(&self) -> Option<RedactFill> {
        match self.kind {
            ItemKind::Redact { fill } => Some(fill),
            _ => None,
        }
    }
}

/// Pages added to a document from another PDF: that file, and which of its
/// pages (from 0), in the order they were added.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageImport {
    pub path: PathBuf,
    pub pages: Vec<usize>,
}

/// A text watermark stamped across every page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Watermark {
    pub text: String,
    /// `0xRRGGBB`.
    pub color: u32,
    /// 0 (invisible) to 1 (opaque).
    pub opacity: f32,
    /// Corner to corner (true) or horizontal across the middle.
    pub diagonal: bool,
}

impl Default for Watermark {
    fn default() -> Self {
        Self {
            text: "CONFIDENTIAL".into(),
            color: 0x8a8a8a,
            opacity: 0.22,
            diagonal: true,
        }
    }
}

/// Font metrics needed to place text in the PDF exactly where the UI draws it.
#[derive(Clone, Copy, Debug)]
pub struct TextMetrics {
    /// Distance from the top of a line box to the baseline, as a multiple of
    /// the font size.
    pub baseline_ratio: f32,
}

impl Default for TextMetrics {
    fn default() -> Self {
        // Helvetica on macOS: ascent 0.77, descent 0.23 → (1.2 - 1.0)/2 + 0.77.
        Self {
            baseline_ratio: (LINE_HEIGHT - 1.0) / 2.0 + 0.77,
        }
    }
}

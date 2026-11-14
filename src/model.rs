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
    /// A box of see-through colour, as left by a highlighter pen.
    Highlight { color: u32 },
    /// The outline of a box, or of the oval that fills it.
    Outline { oval: bool, color: u32, width: f32 },
    /// A line through `points` (as fractions of the item's box, from its top
    /// left): drawn freehand, or straight between two, with an arrow head at
    /// the end if `arrow`.
    Stroke { points: Arc<[[f32; 2]]>, color: u32, width: f32, arrow: bool },
}

/// The highlighter's colours, and the pen's (which shapes are drawn with too): `0xRRGGBB`.
pub const HIGHLIGHT_COLORS: &[(&str, u32)] = &[("Yellow", 0xffe14d), ("Green", 0x8ce99a), ("Pink", 0xffa8c9), ("Blue", 0x91c8ff)];
pub const PEN_COLORS: &[(&str, u32)] = &[("Red", 0xe5484d), ("Blue", 0x1e4fd6), ("Green", 0x2b9a66), ("Orange", 0xf76b15), ("Black", 0x111111)];
/// How wide the pen draws, in points.
pub const PEN_WIDTHS: &[(&str, f32)] = &[("Thin", 1.5), ("Medium", 3.), ("Thick", 6.)];

/// The ends of the two barbs of an arrow head, on a line `width` wide that
/// ends at `to` coming from `from`. `unit` is the length of a point.
pub fn arrow_barbs(from: (f32, f32), to: (f32, f32), width: f32, unit: f32) -> [(f32, f32); 2] {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let length = dx.hypot(dy).max(f32::EPSILON);
    let (ux, uy) = (dx / length, dy / length);
    let size = (width * 4.).max(8. * unit);
    // 28° to either side of the line.
    let (sin, cos) = (0.469, 0.883);
    [1., -1.].map(|side| (to.0 - size * (ux * cos - side * uy * sin), to.1 - size * (uy * cos + side * ux * sin)))
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

    /// The colour of something drawn on the page, and whether it is the
    /// highlighter's (`true`) or the pen's.
    pub fn color(&self) -> Option<(bool, u32)> {
        match self.kind {
            ItemKind::Highlight { color } => Some((true, color)),
            ItemKind::Outline { color, .. } | ItemKind::Stroke { color, .. } => Some((false, color)),
            _ => None,
        }
    }

    pub fn set_color(&mut self, to: u32) {
        if let ItemKind::Highlight { color } | ItemKind::Outline { color, .. } | ItemKind::Stroke { color, .. } = &mut self.kind {
            *color = to;
        }
    }

    /// How wide its line is, if it is drawn with the pen.
    pub fn pen_width(&self) -> Option<f32> {
        match self.kind {
            ItemKind::Outline { width, .. } | ItemKind::Stroke { width, .. } => Some(width),
            _ => None,
        }
    }

    pub fn set_pen_width(&mut self, to: f32) {
        if let ItemKind::Outline { width, .. } | ItemKind::Stroke { width, .. } = &mut self.kind {
            *width = to;
        }
    }

    /// Whether its width and height are resized separately (other items keep their shape).
    pub fn stretches(&self) -> bool {
        matches!(self.kind, ItemKind::Redact { .. } | ItemKind::Highlight { .. } | ItemKind::Outline { .. } | ItemKind::Stroke { .. })
    }

    /// The points a stroke runs through, in view space.
    pub fn stroke_points(&self) -> Vec<(f32, f32)> {
        match &self.kind {
            ItemKind::Stroke { points, .. } => points.iter().map(|[x, y]| (self.x + x * self.w, self.y + y * self.h)).collect(),
            _ => Vec::new(),
        }
    }

    /// Make a stroke run through `through` (view space): its box is the one
    /// around them, never too thin to take hold of.
    pub fn fit_stroke(&mut self, through: &[(f32, f32)]) {
        const MIN: f32 = 8.;
        let span = |values: &mut dyn Iterator<Item = f32>| {
            let (low, high) = values.fold((f32::MAX, f32::MIN), |(low, high), v| (low.min(v), high.max(v)));
            if high - low < MIN { ((low + high - MIN) / 2., MIN) } else { (low, high - low) }
        };
        if through.is_empty() {
            return;
        }
        let ((x, w), (y, h)) = (span(&mut through.iter().map(|p| p.0)), span(&mut through.iter().map(|p| p.1)));
        (self.x, self.y, self.w, self.h) = (x, y, w, h);
        if let ItemKind::Stroke { points, .. } = &mut self.kind {
            *points = through.iter().map(|&(px, py)| [(px - x) / w, (py - y) / h]).collect();
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

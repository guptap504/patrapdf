//! Signature library: imported signature images, stored as background-free
//! PNGs in the user's data directory so they are available across sessions.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use image::{GenericImageView, Rgba, RgbaImage};

use crate::model::Signature;

pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg"];

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

pub fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// Root of the app's data directory: `~/Library/Application Support/pdf-signer`
/// on macOS, `%APPDATA%\pdf-signer` on Windows, `$XDG_DATA_HOME/pdf-signer`
/// (`~/.local/share/pdf-signer`) on Linux.
/// Set `PDF_SIGNER_DATA_DIR` to use a different directory (handy for development).
#[cfg(not(test))]
pub fn app_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PDF_SIGNER_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
    } else if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
    };
    base.join("pdf-signer")
}

/// Tests must never touch the user's real library or recents.
#[cfg(test)]
pub fn app_data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("pdf-signer-test-{}", std::process::id()))
}

pub fn library_dir() -> PathBuf {
    app_data_dir().join("signatures")
}

/// Load all signatures from the library directory (oldest first).
pub fn load_library() -> Vec<Arc<Signature>> {
    let Ok(entries) = std::fs::read_dir(library_dir()) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    paths.sort();
    paths.into_iter().filter_map(|p| open_signature(&p).ok()).collect()
}

pub fn open_signature(path: &Path) -> Result<Arc<Signature>> {
    let (width_px, height_px) = image::image_dimensions(path)?;
    Ok(Arc::new(Signature {
        path: path.to_path_buf(),
        width_px,
        height_px,
    }))
}

/// Import an image into the library: knock out the paper background, crop to
/// the ink, and save it as a PNG.
pub fn import(source: &Path) -> Result<Arc<Signature>> {
    let img = image::open(source).with_context(|| format!("could not open {}", source.display()))?;
    let mut rgba = img.to_rgba8();

    let has_transparency = rgba.pixels().any(|p| p.0[3] < 250);
    if !has_transparency {
        remove_background(&mut rgba);
    }
    let cropped = crop_to_content(&rgba).context("the image appears to be blank")?;

    // Keep the library light: signatures don't need to be huge.
    let cropped = if cropped.width() > 1200 {
        let h = (cropped.height() as f32 * 1200.0 / cropped.width() as f32).round() as u32;
        image::imageops::resize(&cropped, 1200, h.max(1), image::imageops::FilterType::Lanczos3)
    } else {
        cropped
    };

    let dir = library_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let dest = dir.join(format!("sig-{stamp}.png"));
    cropped.save(&dest).with_context(|| format!("could not save {}", dest.display()))?;
    open_signature(&dest)
}

pub fn delete(sig: &Signature) {
    let _ = std::fs::remove_file(&sig.path);
}

/// Turn light (paper-coloured) pixels transparent, with a soft ramp so the
/// ink edges stay anti-aliased.
fn remove_background(img: &mut RgbaImage) {
    const INK: f32 = 150.0; // at or below → fully opaque
    const PAPER: f32 = 225.0; // at or above → fully transparent
    for p in img.pixels_mut() {
        let [r, g, b, a] = p.0;
        let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        let coverage = ((PAPER - lum) / (PAPER - INK)).clamp(0.0, 1.0);
        let alpha = (coverage * a as f32).round() as u8;
        *p = Rgba([r, g, b, alpha]);
    }
}

fn crop_to_content(img: &RgbaImage) -> Result<RgbaImage> {
    let (w, h) = img.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p.0[3] > 16 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x0 > x1 || y0 > y1 {
        bail!("no visible content");
    }
    let pad = 4;
    let x0 = x0.saturating_sub(pad);
    let y0 = y0.saturating_sub(pad);
    let x1 = (x1 + pad).min(w - 1);
    let y1 = (y1 + pad).min(h - 1);
    Ok(img.view(x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image())
}

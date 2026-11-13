//! Recently edited documents, shown in the start-up gallery.
//!
//! An entry is recorded every time a document is saved. It remembers the
//! original PDF, the saved (flattened) copy, a thumbnail of the saved first
//! page, and the placed fields, so reopening the entry resumes editing with
//! every field still movable and editable.
//!
//! Layout on disk (inside the app data dir):
//! ```text
//! recents/index.json        list of entries, most recent first
//! recents/thumbs/*.png      first-page thumbnails
//! recents/assets/*.png      copies of the signature images used by entries
//! ```

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::model::{Item, ItemKind, Signature, Watermark};
use crate::{pdf, signatures};

const MAX_ENTRIES: usize = 40;
/// Thumbnail width in pixels (displayed at roughly half that on Retina screens).
const THUMB_WIDTH_PX: f32 = 360.;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecentEntry {
    /// Stable id derived from the original's path.
    pub id: String,
    pub original: PathBuf,
    pub output: PathBuf,
    /// Unix timestamp (seconds) of the last save.
    pub saved_at: i64,
    pub page_count: usize,
    pub thumbnail: Option<PathBuf>,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub items: Vec<SavedItem>,
    #[serde(default)]
    pub watermark: Option<Watermark>,
    /// Page order after deleting/reordering (indices into the original's pages);
    /// `None` when unchanged.
    #[serde(default)]
    pub order: Option<Vec<usize>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedItem {
    pub page: usize,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub kind: SavedKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SavedKind {
    Text { text: String, font_size: f32 },
    Image { path: PathBuf },
    Check,
    Redact,
}

/// A rendered first-page thumbnail, ready to be written to disk.
pub struct Thumbnail {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

fn dir() -> PathBuf {
    signatures::app_data_dir().join("recents")
}

fn index_path() -> PathBuf {
    dir().join("index.json")
}

fn hash_hex(data: impl Hash) -> String {
    let mut h = DefaultHasher::new();
    data.hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn load() -> Vec<RecentEntry> {
    std::fs::read(index_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_index(entries: &[RecentEntry]) -> Result<()> {
    std::fs::create_dir_all(dir())?;
    let json = serde_json::to_vec_pretty(entries)?;
    // Write atomically so a crash can't leave a truncated index behind.
    let tmp = dir().join("index.json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, index_path())?;
    Ok(())
}

/// Render the first page of a PDF as a PNG thumbnail.
pub fn make_thumbnail(pdf_bytes: Vec<u8>) -> Option<Thumbnail> {
    let loaded = pdf::load(pdf_bytes).ok()?;
    let scale = THUMB_WIDTH_PX / loaded.pages[0].width.max(1.);
    let page = pdf::render_page(&loaded.pdf, 0, scale)?;
    let mut rgba = page.bgra;
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let img = image::RgbaImage::from_raw(page.width, page.height, rgba)?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(Thumbnail {
        png,
        width: page.width,
        height: page.height,
    })
}

/// Copy a signature image into the recents asset store (content-addressed, so
/// identical images are stored once). Returns the stored path.
fn store_asset(source: &Path) -> Result<PathBuf> {
    let assets = dir().join("assets");
    if source.starts_with(&assets) && source.exists() {
        return Ok(source.to_path_buf());
    }
    let bytes = std::fs::read(source).with_context(|| format!("could not read {}", source.display()))?;
    std::fs::create_dir_all(&assets)?;
    let dest = assets.join(format!("{}.png", hash_hex(&bytes)));
    if !dest.exists() {
        std::fs::write(&dest, &bytes)?;
    }
    Ok(dest)
}

/// Record (or update) the entry for `original` after a successful save.
/// Returns the updated list, most recent first.
pub fn record(
    mut entries: Vec<RecentEntry>,
    original: &Path,
    output: &Path,
    page_count: usize,
    items: &[Item],
    watermark: Option<&Watermark>,
    order: Option<&[usize]>,
    thumbnail: Option<Thumbnail>,
) -> Result<Vec<RecentEntry>> {
    let id = hash_hex(original);
    let saved_at = chrono::Utc::now().timestamp();

    let saved_items = items
        .iter()
        .filter_map(|item| {
            let kind = match &item.kind {
                ItemKind::Text { text, font_size } => SavedKind::Text {
                    text: text.clone(),
                    font_size: *font_size,
                },
                ItemKind::Image { sig } => SavedKind::Image {
                    path: store_asset(&sig.path).ok()?,
                },
                ItemKind::Check => SavedKind::Check,
                ItemKind::Redact => SavedKind::Redact,
            };
            Some(SavedItem {
                page: item.page,
                x: item.x,
                y: item.y,
                w: item.w,
                h: item.h,
                kind,
            })
        })
        .collect();

    // A fresh file name per save, so GPUI's image cache never shows a stale thumbnail.
    let (thumb_path, thumb_width, thumb_height) = match thumbnail {
        Some(t) => {
            let thumbs = dir().join("thumbs");
            std::fs::create_dir_all(&thumbs)?;
            let path = thumbs.join(format!("{id}-{saved_at}.png"));
            std::fs::write(&path, &t.png)?;
            (Some(path), t.width, t.height)
        }
        None => (None, 0, 0),
    };

    if let Some(old) = entries.iter().position(|e| e.id == id) {
        let old = entries.remove(old);
        if old.thumbnail != thumb_path {
            remove_file_quietly(old.thumbnail.as_deref());
        }
    }
    entries.insert(
        0,
        RecentEntry {
            id,
            original: original.to_path_buf(),
            output: output.to_path_buf(),
            saved_at,
            page_count,
            thumbnail: thumb_path,
            thumb_width,
            thumb_height,
            items: saved_items,
            watermark: watermark.cloned(),
            order: order.map(<[usize]>::to_vec),
        },
    );
    for dropped in entries.split_off(entries.len().min(MAX_ENTRIES)) {
        remove_file_quietly(dropped.thumbnail.as_deref());
    }

    write_index(&entries)?;
    collect_garbage(&entries);
    Ok(entries)
}

/// Remove an entry from the gallery (the PDFs themselves are untouched).
pub fn remove(mut entries: Vec<RecentEntry>, id: &str) -> Result<Vec<RecentEntry>> {
    if let Some(i) = entries.iter().position(|e| e.id == id) {
        let entry = entries.remove(i);
        remove_file_quietly(entry.thumbnail.as_deref());
    }
    write_index(&entries)?;
    collect_garbage(&entries);
    Ok(entries)
}

/// Turn a saved entry back into editor items. Images whose stored copy has
/// gone missing, or items on pages that no longer exist, are skipped.
pub fn restore_items(entry: &RecentEntry, page_count: usize, mut next_id: impl FnMut() -> u64) -> Vec<Item> {
    entry
        .items
        .iter()
        .filter(|i| i.page < page_count)
        .filter_map(|i| {
            let kind = match &i.kind {
                SavedKind::Text { text, font_size } => ItemKind::Text {
                    text: text.clone(),
                    font_size: *font_size,
                },
                SavedKind::Image { path } => {
                    let sig: Arc<Signature> = signatures::open_signature(path).ok()?;
                    ItemKind::Image { sig }
                }
                SavedKind::Check => ItemKind::Check,
                SavedKind::Redact => ItemKind::Redact,
            };
            Some(Item {
                id: next_id(),
                page: i.page,
                x: i.x,
                y: i.y,
                w: i.w,
                h: i.h,
                kind,
            })
        })
        .collect()
}

fn remove_file_quietly(path: Option<&Path>) {
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

/// Delete stored signature copies that no entry references any more.
fn collect_garbage(entries: &[RecentEntry]) {
    let used: HashSet<&Path> = entries
        .iter()
        .flat_map(|e| &e.items)
        .filter_map(|i| match &i.kind {
            SavedKind::Image { path } => Some(path.as_path()),
            _ => None,
        })
        .collect();
    let Ok(read) = std::fs::read_dir(dir().join("assets")) else { return };
    for file in read.flatten() {
        let path = file.path();
        if !used.contains(path.as_path()) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_restore_and_remove() {
        let tmp = signatures::app_data_dir().join("recents-test-src");
        std::fs::create_dir_all(&tmp).unwrap();
        let sig_path = tmp.join("sig.png");
        image::RgbaImage::from_pixel(40, 20, image::Rgba([0, 0, 0, 255])).save(&sig_path).unwrap();
        let sig = signatures::open_signature(&sig_path).unwrap();

        let original = tmp.join("form.pdf");
        let output = tmp.join("form-signed.pdf");
        let items = vec![
            Item { id: 1, page: 0, x: 10., y: 20., w: 50., h: 14., kind: ItemKind::Text { text: "Hi".into(), font_size: 12. } },
            Item { id: 2, page: 1, x: 30., y: 40., w: 80., h: 40., kind: ItemKind::Image { sig } },
            Item { id: 3, page: 5, x: 1., y: 1., w: 9., h: 9., kind: ItemKind::Check },
            Item { id: 4, page: 0, x: 5., y: 5., w: 90., h: 12., kind: ItemKind::Redact },
        ];
        let watermark = Watermark { text: "COPY".into(), ..Watermark::default() };
        let thumb = make_thumbnail(pdf::tests::sample_pdf()).expect("thumbnail");
        assert!(thumb.width > 0 && thumb.height > thumb.width);

        let entries = record(Vec::new(), &original, &output, 2, &items, Some(&watermark), Some(&[1, 0]), Some(thumb)).unwrap();
        assert_eq!(entries.len(), 1);
        // The signature was copied into the store, so deleting the library file is harmless.
        std::fs::remove_file(&sig_path).unwrap();

        let loaded = load();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].thumbnail.as_ref().is_some_and(|p| p.exists()));
        let mut n = 100;
        let restored = restore_items(&loaded[0], 2, || {
            n += 1;
            n
        });
        // The item on page 5 no longer fits a 2-page document.
        assert_eq!(restored.len(), 3);
        assert!(matches!(&restored[0].kind, ItemKind::Text { text, .. } if text == "Hi"));
        assert!(matches!(&restored[1].kind, ItemKind::Image { .. }));
        assert!(restored[2].is_redaction());
        assert_eq!(restored[1].id, 102);
        assert_eq!(loaded[0].watermark.as_ref(), Some(&watermark));
        assert_eq!(loaded[0].order.as_deref(), Some(&[1, 0][..]));

        // Saving the same original again updates the entry instead of adding one.
        let entries = record(loaded, &original, &output, 2, &restored, None, None, None).unwrap();
        assert_eq!(entries[0].watermark, None);
        assert_eq!(entries.len(), 1);

        let id = entries[0].id.clone();
        let entries = remove(entries, &id).unwrap();
        assert!(entries.is_empty());
        assert!(load().is_empty());
        let assets_left = std::fs::read_dir(dir().join("assets")).map(|r| r.count()).unwrap_or(0);
        assert_eq!(assets_left, 0, "unused signature copies should be cleaned up");
    }
}

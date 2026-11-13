//! PDF rendering (hayro) and overlay writing (lopdf).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_syntax::Pdf;
use hayro::kurbo::Affine;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::model::{Item, ItemKind, LINE_HEIGHT, PageGeom, TextMetrics};

// ---------------------------------------------------------------------------
// Loading & rendering
// ---------------------------------------------------------------------------

pub struct LoadedPdf {
    /// Original file bytes (the writer starts from these).
    pub bytes: Arc<Vec<u8>>,
    pub pdf: Arc<Pdf>,
    pub pages: Vec<PageGeom>,
}

pub fn load(bytes: Vec<u8>) -> Result<LoadedPdf> {
    let bytes = Arc::new(bytes);
    let pdf = Pdf::new(bytes.as_ref().clone()).map_err(|e| anyhow!("could not read PDF: {e:?}"))?;
    let pages: Vec<PageGeom> = pdf
        .pages()
        .iter()
        .map(|page| {
            let (width, height) = page.render_dimensions();
            let user_to_view: Affine = page.initial_transform(true).to_kurbo();
            PageGeom {
                width,
                height,
                view_to_user: user_to_view.inverse().as_coeffs(),
            }
        })
        .collect();
    if pages.is_empty() {
        bail!("the PDF has no pages");
    }
    Ok(LoadedPdf {
        bytes,
        pdf: Arc::new(pdf),
        pages,
    })
}

/// A rendered page bitmap in BGRA order (what GPUI expects).
pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub fn render_page(pdf: &Pdf, index: usize, scale: f32) -> Option<RenderedPage> {
    let pages = pdf.pages();
    let page = pages.get(index)?;
    let cache = RenderCache::new();
    let pixmap = hayro::render(
        page,
        &cache,
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        },
    );
    let (width, height) = (pixmap.width() as u32, pixmap.height() as u32);
    // Opaque white background, so premultiplied == straight alpha. Swap R/B.
    let mut bgra = pixmap.data_as_u8_slice().to_vec();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(RenderedPage {
        width,
        height,
        bgra,
    })
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

const FONT_NAME: &str = "PdfSignerHelv";

/// Produce a new PDF with all overlay items burned into the page content.
pub fn write(original: &[u8], pages: &[PageGeom], items: &[Item], metrics: TextMetrics) -> Result<Vec<u8>> {
    let mut doc = Document::load_mem(original).context("lopdf could not parse the PDF")?;
    if doc.is_encrypted() {
        // Most "encrypted" forms only use an owner password; try the empty user password.
        doc.decrypt("").map_err(|_| anyhow!("this PDF is password protected"))?;
        doc.trailer.remove(b"Encrypt");
    }

    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    if page_ids.len() != pages.len() {
        bail!(
            "page count mismatch between renderer ({}) and writer ({})",
            pages.len(),
            page_ids.len()
        );
    }

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });

    let mut images: HashMap<PathBuf, (ObjectId, String)> = HashMap::new();

    for (page_index, (&page_id, geom)) in page_ids.iter().zip(pages).enumerate() {
        let page_items: Vec<&Item> = items.iter().filter(|i| i.page == page_index).collect();
        if page_items.is_empty() {
            continue;
        }

        let v = Affine::new(geom.view_to_user);
        let mut ops = String::new();
        let mut used_font = false;
        let mut used_images: Vec<(String, ObjectId)> = Vec::new();

        for item in page_items {
            match &item.kind {
                ItemKind::Text { text, font_size } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    used_font = true;
                    // Local frame: origin at the item's top-left, y up.
                    let m = v * Affine::translate((item.x as f64, item.y as f64)) * Affine::FLIP_Y;
                    ops.push_str(&format!("q {} cm 0 g BT /{FONT_NAME} {} Tf\n", fmt_m(m), num(*font_size as f64)));
                    for (i, line) in text.split('\n').enumerate() {
                        let baseline = (metrics.baseline_ratio + i as f32 * LINE_HEIGHT) * font_size;
                        ops.push_str(&format!("1 0 0 1 0 {} Tm <{}> Tj\n", num(-baseline as f64), winansi_hex(line)));
                    }
                    ops.push_str("ET Q\n");
                }
                ItemKind::Image { sig } => {
                    let (img_id, name) = match images.get(&sig.path) {
                        Some(entry) => entry.clone(),
                        None => {
                            let id = add_image(&mut doc, &sig.path)?;
                            let entry = (id, format!("PdfSignerIm{}", images.len()));
                            images.insert(sig.path.clone(), entry.clone());
                            entry
                        }
                    };
                    if !used_images.iter().any(|(n, _)| *n == name) {
                        used_images.push((name.clone(), img_id));
                    }
                    let m = v
                        * Affine::translate((item.x as f64, (item.y + item.h) as f64))
                        * Affine::FLIP_Y
                        * Affine::scale_non_uniform(item.w as f64, item.h as f64);
                    ops.push_str(&format!("q {} cm /{name} Do Q\n", fmt_m(m)));
                }
                ItemKind::Check => {
                    let s = item.w as f64;
                    let m = v * Affine::translate((item.x as f64, (item.y + item.h) as f64)) * Affine::FLIP_Y;
                    let p = |x: f64, y: f64| format!("{} {}", num(x * s), num(y * s));
                    ops.push_str(&format!(
                        "q {} cm 0 G {} w 1 J 1 j {} m {} l {} l S Q\n",
                        fmt_m(m),
                        num(s * 0.12),
                        p(0.15, 0.5),
                        p(0.4, 0.2),
                        p(0.85, 0.85),
                    ));
                }
            }
        }

        if ops.is_empty() {
            continue;
        }

        add_resources(&mut doc, page_id, used_font.then_some(font_id), &used_images)?;
        append_content(&mut doc, page_id, ops.into_bytes())?;
    }

    doc.compress();
    let mut out = Vec::new();
    doc.save_to(&mut out).context("could not serialize PDF")?;
    Ok(out)
}

fn num(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

fn fmt_m(m: Affine) -> String {
    m.as_coeffs().iter().map(|c| num(*c)).collect::<Vec<_>>().join(" ")
}

/// Encode a line as a hex string in WinAnsiEncoding (unsupported chars → '?').
fn winansi_hex(s: &str) -> String {
    s.chars().map(|c| format!("{:02X}", winansi(c))).collect()
}

fn winansi(c: char) -> u8 {
    match c {
        '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => c as u8,
        '\t' => b' ',
        '€' => 0x80,
        '‚' => 0x82,
        'ƒ' => 0x83,
        '„' => 0x84,
        '…' => 0x85,
        '†' => 0x86,
        '‡' => 0x87,
        'ˆ' => 0x88,
        '‰' => 0x89,
        'Š' => 0x8a,
        '‹' => 0x8b,
        'Œ' => 0x8c,
        'Ž' => 0x8e,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '˜' => 0x98,
        '™' => 0x99,
        'š' => 0x9a,
        '›' => 0x9b,
        'œ' => 0x9c,
        'ž' => 0x9e,
        'Ÿ' => 0x9f,
        _ => b'?',
    }
}

/// Embed a PNG/JPEG as an RGB image XObject with an alpha soft mask.
fn add_image(doc: &mut Document, path: &PathBuf) -> Result<ObjectId> {
    let img = image::open(path)
        .with_context(|| format!("could not read image {}", path.display()))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    let mut alpha = Vec::with_capacity((w * h) as usize);
    for p in img.pixels() {
        rgb.extend_from_slice(&p.0[..3]);
        alpha.push(p.0[3]);
    }

    let mut smask = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 8,
        },
        alpha,
    );
    let _ = smask.compress();
    let smask_id = doc.add_object(smask);

    let mut stream = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Interpolate" => true,
            "SMask" => smask_id,
        },
        rgb,
    );
    let _ = stream.compress();
    Ok(doc.add_object(stream))
}

/// Resolve a (possibly referenced) dictionary into an owned copy.
fn resolve_dict(doc: &Document, obj: &Object) -> Option<Dictionary> {
    match obj {
        Object::Dictionary(d) => Some(d.clone()),
        Object::Reference(id) => doc.get_dictionary(*id).ok().cloned(),
        _ => None,
    }
}

/// Find the page's effective /Resources (walking up the page tree for inheritance).
fn effective_resources(doc: &Document, page_id: ObjectId) -> Dictionary {
    let mut current = Some(page_id);
    let mut guard = 0;
    while let Some(id) = current {
        guard += 1;
        if guard > 64 {
            break;
        }
        let Ok(dict) = doc.get_dictionary(id) else { break };
        if let Ok(res) = dict.get(b"Resources") {
            if let Some(d) = resolve_dict(doc, res) {
                return d;
            }
        }
        current = dict.get(b"Parent").and_then(|p| p.as_reference()).ok();
    }
    Dictionary::new()
}

fn add_resources(
    doc: &mut Document,
    page_id: ObjectId,
    font: Option<ObjectId>,
    images: &[(String, ObjectId)],
) -> Result<()> {
    let mut res = effective_resources(doc, page_id);

    if let Some(font_id) = font {
        let mut fonts = res
            .get(b"Font")
            .ok()
            .and_then(|o| resolve_dict(doc, o))
            .unwrap_or_default();
        fonts.set(FONT_NAME, font_id);
        res.set("Font", fonts);
    }

    if !images.is_empty() {
        let mut xobjects = res
            .get(b"XObject")
            .ok()
            .and_then(|o| resolve_dict(doc, o))
            .unwrap_or_default();
        for (name, id) in images {
            xobjects.set(name.as_bytes().to_vec(), *id);
        }
        res.set("XObject", xobjects);
    }

    doc.get_dictionary_mut(page_id)?.set("Resources", res);
    Ok(())
}

/// Wrap the existing content in q/Q (so stray graphics state can't leak into
/// our overlay) and append the overlay content stream.
fn append_content(doc: &mut Document, page_id: ObjectId, overlay: Vec<u8>) -> Result<()> {
    let existing: Vec<Object> = match doc.get_dictionary(page_id)?.get(b"Contents") {
        Ok(Object::Reference(id)) => match doc.get_object(*id) {
            Ok(Object::Array(arr)) => arr.clone(),
            _ => vec![Object::Reference(*id)],
        },
        Ok(Object::Array(arr)) => arr.clone(),
        _ => vec![],
    };

    let open = doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
    let close = doc.add_object(Stream::new(Dictionary::new(), b"\nQ\n".to_vec()));
    let mut overlay = Stream::new(Dictionary::new(), overlay);
    let _ = overlay.compress();
    let overlay = doc.add_object(overlay);

    let mut contents = vec![Object::Reference(open)];
    contents.extend(existing);
    contents.push(Object::Reference(close));
    contents.push(Object::Reference(overlay));
    doc.get_dictionary_mut(page_id)?.set("Contents", contents);
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::Signature;

    /// Two pages: a plain Letter page, and a rotated page with an offset crop box.
    pub(crate) fn sample_pdf() -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Times-Roman",
        });
        let mut kids = vec![];
        for (i, rotate) in [0i64, 90].into_iter().enumerate() {
            let content = format!(
                "BT /F1 24 Tf 72 700 Td (Page {} - Name:) Tj ET 0.5 G 160 690 m 400 690 l S",
                i + 1
            );
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
            let mut page = dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Rotate" => rotate,
            };
            if rotate != 0 {
                page.set("CropBox", vec![20.into(), 30.into(), 600.into(), 780.into()]);
            }
            kids.push(Object::Reference(doc.add_object(page)));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 2,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn round_trip() {
        let bytes = sample_pdf();
        let loaded = load(bytes.clone()).unwrap();
        assert_eq!(loaded.pages.len(), 2);
        assert_eq!((loaded.pages[1].width, loaded.pages[1].height), (750.0, 580.0));

        let out_dir = std::env::var("PDF_SIGNER_TEST_OUT").ok().map(PathBuf::from);

        // A small red/transparent test "signature".
        let sig_path = std::env::temp_dir().join("pdf_signer_test_sig.png");
        let mut sig = image::RgbaImage::new(200, 80);
        for (x, y, p) in sig.enumerate_pixels_mut() {
            let on = (y as i32 - 40 - ((x as f32 / 15.0).sin() * 20.0) as i32).abs() < 4;
            *p = if on { image::Rgba([20, 40, 160, 255]) } else { image::Rgba([0, 0, 0, 0]) };
        }
        sig.save(&sig_path).unwrap();
        let sig = Arc::new(Signature { path: sig_path, width_px: 200, height_px: 80 });

        let mut items = vec![];
        for page in 0..2 {
            items.push(Item {
                id: 1,
                page,
                x: 165.0,
                y: 75.0,
                w: 100.0,
                h: 20.0,
                kind: ItemKind::Text { text: "Jane Q. Doe — ok\nline two".into(), font_size: 16.0 },
            });
            items.push(Item { id: 2, page, x: 300.0, y: 200.0, w: 150.0, h: 60.0, kind: ItemKind::Image { sig: sig.clone() } });
            items.push(Item { id: 3, page, x: 72.0, y: 200.0, w: 24.0, h: 24.0, kind: ItemKind::Check });
        }

        let written = write(&bytes, &loaded.pages, &items, TextMetrics::default()).unwrap();
        let reloaded = load(written.clone()).unwrap();
        assert_eq!(reloaded.pages.len(), 2);

        if let Some(dir) = out_dir {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("sample.pdf"), &bytes).unwrap();
            std::fs::write(dir.join("out.pdf"), &written).unwrap();
            for i in 0..2 {
                let r = render_page(&reloaded.pdf, i, 1.5).unwrap();
                let mut rgba = r.bgra.clone();
                for px in rgba.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                image::RgbaImage::from_raw(r.width, r.height, rgba)
                    .unwrap()
                    .save(dir.join(format!("page{i}.png")))
                    .unwrap();
            }
        }
    }
}

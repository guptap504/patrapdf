//! PDF rendering (hayro) and writing (lopdf): overlays, watermarks and redaction.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_syntax::Pdf;
use hayro::kurbo::Affine;
use hayro::vello_cpu::color::palette::css::{TRANSPARENT, WHITE};
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::model::{Item, ItemKind, LINE_HEIGHT, PageGeom, TextMetrics, Watermark};

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
    render(pdf, index, scale, false)
}

fn render(pdf: &Pdf, index: usize, scale: f32, transparent: bool) -> Option<RenderedPage> {
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
            bg_color: if transparent { TRANSPARENT } else { WHITE },
        },
    );
    let (width, height) = (pixmap.width() as u32, pixmap.height() as u32);
    let mut bgra = pixmap.data_as_u8_slice().to_vec();
    for px in bgra.chunks_exact_mut(4) {
        // hayro's pixels are premultiplied; GPUI wants straight alpha. (With the
        // opaque white background this is a no-op.)
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
        px.swap(0, 2);
    }
    Some(RenderedPage {
        width,
        height,
        bgra,
    })
}

/// Render just the watermark for a page of the given (displayed) size, on a
/// transparent background, exactly as the writer will draw it.
pub fn render_watermark(width: f32, height: f32, watermark: &Watermark, scale: f32) -> Option<RenderedPage> {
    let flip = Affine::new([1., 0., 0., -1., 0., height as f64]);
    let ops = watermark_ops(width, height, flip, watermark)?;
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let (font_id, gs_id) = add_watermark_resources(&mut doc, watermark);
    let content = doc.add_object(Stream::new(dictionary! {}, ops.into_bytes()));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content,
        "MediaBox" => vec![0.into(), 0.into(), (width as f64).into(), (height as f64).into()],
        "Resources" => dictionary! {
            "Font" => dictionary! { WM_FONT => font_id },
            "ExtGState" => dictionary! { WM_GS => gs_id },
        },
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).ok()?;
    let pdf = Pdf::new(bytes).ok()?;
    render(&pdf, 0, scale, true)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

const FONT_NAME: &str = "PdfSignerHelv";
const WM_FONT: &str = "PdfSignerHelvBold";
const WM_GS: &str = "PdfSignerWmGS";
const PAGE_IMAGE: &str = "PdfSignerPage";
/// Resolution of redacted pages, which are rebuilt as images: 3 × 72 = 216 dpi.
const REDACT_SCALE: f32 = 3.0;

/// Produce a new PDF with the watermark and all overlay items burned into the
/// page content, then apply any redactions.
pub fn write(
    original: &[u8],
    pages: &[PageGeom],
    items: &[Item],
    watermark: Option<&Watermark>,
    metrics: TextMetrics,
) -> Result<Vec<u8>> {
    let watermark = watermark.filter(|w| !w.text.trim().is_empty());
    let flattened = write_overlays(original, pages, items, watermark, metrics)?;
    let redactions: Vec<&Item> = items.iter().filter(|i| i.is_redaction() && i.w >= 1. && i.h >= 1.).collect();
    if redactions.is_empty() {
        return Ok(flattened);
    }
    redact(&flattened, pages, &redactions)
}

fn write_overlays(
    original: &[u8],
    pages: &[PageGeom],
    items: &[Item],
    watermark: Option<&Watermark>,
    metrics: TextMetrics,
) -> Result<Vec<u8>> {
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

    let watermark_res = watermark.map(|w| add_watermark_resources(&mut doc, w));
    let mut images: HashMap<PathBuf, (ObjectId, String)> = HashMap::new();

    for (page_index, (&page_id, geom)) in page_ids.iter().zip(pages).enumerate() {
        let page_items: Vec<&Item> = items.iter().filter(|i| i.page == page_index).collect();
        if page_items.is_empty() && watermark.is_none() {
            continue;
        }

        let v = Affine::new(geom.view_to_user);
        let mut ops = String::new();
        let mut used_font = false;
        let mut used_images: Vec<(String, ObjectId)> = Vec::new();

        // The watermark goes over the page content but under the filled-in fields.
        let mut used_watermark = false;
        if let Some(wm) = watermark {
            if let Some(wm_ops) = watermark_ops(geom.width, geom.height, v, wm) {
                ops.push_str(&wm_ops);
                used_watermark = true;
            }
        }

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
                // Applied afterwards by `redact`, on top of everything else.
                ItemKind::Redact => {}
            }
        }

        if ops.is_empty() {
            continue;
        }

        let mut fonts = Vec::new();
        if used_font {
            fonts.push((FONT_NAME, font_id));
        }
        let mut states = Vec::new();
        if let (true, Some((wm_font, wm_gs))) = (used_watermark, watermark_res) {
            fonts.push((WM_FONT, wm_font));
            states.push((WM_GS, wm_gs));
        }
        add_resources(&mut doc, page_id, &fonts, &states, &used_images)?;
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
    fonts: &[(&str, ObjectId)],
    graphics_states: &[(&str, ObjectId)],
    images: &[(String, ObjectId)],
) -> Result<()> {
    let mut res = effective_resources(doc, page_id);

    for (category, entries) in [("Font", fonts), ("ExtGState", graphics_states)] {
        if entries.is_empty() {
            continue;
        }
        let mut dict = res
            .get(category.as_bytes())
            .ok()
            .and_then(|o| resolve_dict(doc, o))
            .unwrap_or_default();
        for (name, id) in entries {
            dict.set(*name, *id);
        }
        res.set(category, dict);
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

// ---------------------------------------------------------------------------
// Watermark
// ---------------------------------------------------------------------------

/// Helvetica-Bold font and the transparency state for the watermark.
fn add_watermark_resources(doc: &mut Document, watermark: &Watermark) -> (ObjectId, ObjectId) {
    let font = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold",
        "Encoding" => "WinAnsiEncoding",
    });
    let opacity = watermark.opacity.clamp(0., 1.);
    let state = doc.add_object(dictionary! {
        "Type" => "ExtGState",
        "CA" => opacity,
        "ca" => opacity,
    });
    (font, state)
}

/// Content-stream operators that draw the watermark on a page of the given
/// displayed size. `v` maps view space to the page's user space.
fn watermark_ops(width: f32, height: f32, v: Affine, watermark: &Watermark) -> Option<String> {
    const CAP_HEIGHT: f64 = 0.718; // Helvetica-Bold, in em
    let text = watermark.text.trim();
    let em: f64 = text.chars().map(|c| HELVETICA_BOLD_WIDTHS[winansi(c) as usize] as f64).sum::<f64>() / 1000.;
    if em <= 0. {
        return None;
    }
    let (w, h) = (width as f64, height as f64);
    let (angle, span) = if watermark.diagonal {
        ((h / w).atan(), w.hypot(h) * 0.62)
    } else {
        (0., w * 0.8)
    };
    let size = (span / em).min(w.min(h) * 0.28);
    // Local frame: origin at the page centre, x along the text, y up. In view
    // space (y down), rotating by -angle tilts the text up to the right.
    let m = v * Affine::translate((w / 2., h / 2.)) * Affine::rotate(-angle) * Affine::FLIP_Y;
    let [r, g, b] = [16, 8, 0].map(|shift| num(((watermark.color >> shift) & 0xff) as f64 / 255.));
    Some(format!(
        "q /{WM_GS} gs {r} {g} {b} rg {} cm BT /{WM_FONT} {} Tf 1 0 0 1 {} {} Tm <{}> Tj ET Q\n",
        fmt_m(m),
        num(size),
        num(-em * size / 2.),
        num(-CAP_HEIGHT * size / 2.),
        winansi_hex(text),
    ))
}

/// Advance widths (1/1000 em) of Helvetica-Bold, indexed by WinAnsi code
/// (from the standard Adobe font metrics, as bundled with hayro).
#[rustfmt::skip]
const HELVETICA_BOLD_WIDTHS: [u16; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611,
    975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584, 556,
    333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611,
    611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584, 0,
    556, 0, 278, 556, 500, 1000, 556, 556, 333, 1000, 667, 333, 1000, 0, 611, 0,
    0, 278, 278, 500, 500, 350, 556, 1000, 333, 1000, 556, 333, 944, 0, 500, 667,
    0, 333, 556, 556, 556, 556, 280, 556, 333, 737, 370, 556, 584, 0, 737, 333,
    400, 584, 333, 333, 333, 611, 556, 278, 333, 333, 365, 556, 834, 834, 834, 611,
    722, 722, 722, 722, 722, 722, 1000, 722, 667, 667, 667, 667, 278, 278, 278, 278,
    722, 722, 778, 778, 778, 778, 778, 584, 778, 722, 722, 722, 722, 667, 667, 611,
    556, 556, 556, 556, 556, 556, 889, 556, 556, 556, 556, 556, 278, 278, 278, 278,
    611, 611, 611, 611, 611, 611, 611, 584, 611, 611, 611, 611, 611, 556, 611, 556,
];

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

/// Permanently remove everything under the redaction boxes.
///
/// Painting a black box over a page leaves the text underneath in the file
/// (selectable, searchable, extractable). Instead, every page with a redaction
/// is rebuilt as a single image of the page with the boxes burned in, and the
/// page's original text, fonts, images, annotations and form fields are
/// dropped. Unreferenced objects are then pruned, so none of it survives in
/// the saved file. Pages without redactions are untouched.
fn redact(bytes: &[u8], pages: &[PageGeom], redactions: &[&Item]) -> Result<Vec<u8>> {
    let pdf = Pdf::new(bytes.to_vec()).map_err(|e| anyhow!("could not re-read PDF for redaction: {e:?}"))?;
    let mut doc = Document::load_mem(bytes).context("lopdf could not parse the PDF")?;
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let redacted_pages: BTreeSet<usize> = redactions.iter().map(|r| r.page).filter(|&p| p < pages.len()).collect();
    let mut removed_annots: HashSet<ObjectId> = HashSet::new();

    for page_index in redacted_pages {
        let geom = &pages[page_index];
        let page_id = page_ids[page_index];
        let rendered = render_page(&pdf, page_index, REDACT_SCALE)
            .ok_or_else(|| anyhow!("could not render page {} for redaction", page_index + 1))?;
        let (pw, ph) = (rendered.width as usize, rendered.height as usize);
        let mut rgb: Vec<u8> = rendered.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();

        // Burn the boxes in, rounding outwards so no partly covered pixel survives.
        let (sx, sy) = (pw as f32 / geom.width, ph as f32 / geom.height);
        for r in redactions.iter().filter(|r| r.page == page_index) {
            let x0 = ((r.x * sx).floor().max(0.) as usize).min(pw);
            let x1 = (((r.x + r.w) * sx).ceil().max(0.) as usize).min(pw);
            let y0 = ((r.y * sy).floor().max(0.) as usize).min(ph);
            let y1 = (((r.y + r.h) * sy).ceil().max(0.) as usize).min(ph);
            for y in y0..y1 {
                rgb[(y * pw + x0) * 3..(y * pw + x1) * 3].fill(0);
            }
        }

        let mut image = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => pw as i64,
                "Height" => ph as i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
            },
            rgb,
        );
        let _ = image.compress();
        let image_id = doc.add_object(image);

        // Draw the image over the displayed (cropped, rotated) area of the page.
        let m = Affine::new(geom.view_to_user)
            * Affine::translate((0., geom.height as f64))
            * Affine::FLIP_Y
            * Affine::scale_non_uniform(geom.width as f64, geom.height as f64);
        let mut content = Stream::new(dictionary! {}, format!("q {} cm /{PAGE_IMAGE} Do Q\n", fmt_m(m)).into_bytes());
        let _ = content.compress();
        let content_id = doc.add_object(content);

        let annots = doc.get_dictionary(page_id)?.get(b"Annots").ok().cloned();
        let annots = match annots {
            Some(Object::Reference(id)) => doc.get_object(id).ok().cloned(),
            other => other,
        };
        if let Some(Object::Array(annots)) = annots {
            removed_annots.extend(annots.iter().filter_map(|a| a.as_reference().ok()));
        }

        let page = doc.get_dictionary_mut(page_id)?;
        // Anything else on the page that could carry the original content.
        for key in ["Annots", "Thumb", "Metadata", "PieceInfo", "StructParents", "Group", "AA", "B"] {
            page.remove(key.as_bytes());
        }
        page.set("Contents", content_id);
        // An explicit dictionary also stops resources being inherited from the page tree.
        page.set("Resources", dictionary! { "XObject" => dictionary! { PAGE_IMAGE => image_id } });
    }

    strip_document_text(&mut doc, &removed_annots);
    doc.prune_objects();
    doc.compress();
    let mut out = Vec::new();
    doc.save_to(&mut out).context("could not serialize PDF")?;
    Ok(out)
}

/// Remove document-level copies of redacted content: form fields whose
/// widgets were on redacted pages (their values live in the field), and the
/// tagged-PDF structure tree (which can hold alternate/actual text).
fn strip_document_text(doc: &mut Document, removed_annots: &HashSet<ObjectId>) {
    let Ok(catalog_id) = doc.trailer.get(b"Root").and_then(Object::as_reference) else { return };
    if let Ok(catalog) = doc.get_dictionary_mut(catalog_id) {
        catalog.remove(b"StructTreeRoot");
        catalog.remove(b"MarkInfo");
    }

    let Some(form) = doc.get_dictionary(catalog_id).ok().and_then(|c| c.get(b"AcroForm").ok().cloned()) else {
        return;
    };
    let form_id = form.as_reference().ok();
    let Some(mut form) = resolve_dict(doc, &form) else { return };
    let Ok(Object::Array(fields)) = form.get(b"Fields").cloned() else { return };
    let kept: Vec<Object> = fields.into_iter().filter(|f| keep_field(doc, f, removed_annots, 0)).collect();
    form.set("Fields", kept);
    match form_id {
        Some(id) => {
            doc.objects.insert(id, Object::Dictionary(form));
        }
        None => {
            if let Ok(catalog) = doc.get_dictionary_mut(catalog_id) {
                catalog.set("AcroForm", form);
            }
        }
    }
}

/// Whether a form field survives redaction: not if it is (or all of its
/// widgets are) on a redacted page. Prunes removed kids from `/Kids`.
fn keep_field(doc: &mut Document, field: &Object, removed: &HashSet<ObjectId>, depth: usize) -> bool {
    let Ok(id) = field.as_reference() else { return true };
    if removed.contains(&id) {
        return false;
    }
    if depth > 32 {
        return true;
    }
    let Ok(Object::Array(kids)) = doc.get_dictionary(id).and_then(|d| d.get(b"Kids")).cloned() else {
        return true;
    };
    let kept: Vec<Object> = kids.into_iter().filter(|k| keep_field(doc, k, removed, depth + 1)).collect();
    if kept.is_empty() {
        return false;
    }
    if let Ok(dict) = doc.get_dictionary_mut(id) {
        dict.set("Kids", kept);
    }
    true
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

        let watermark = Watermark::default();
        let written = write(&bytes, &loaded.pages, &items, Some(&watermark), TextMetrics::default()).unwrap();
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

    /// Every string the file contains, with streams decompressed.
    fn all_text(pdf: &[u8]) -> String {
        let mut doc = Document::load_mem(pdf).unwrap();
        doc.decompress();
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The sample with a filled-in form field on page 1.
    fn sample_with_form_field() -> Vec<u8> {
        let mut doc = Document::load_mem(&sample_pdf()).unwrap();
        let page1 = doc.get_pages()[&1];
        let field = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => Object::string_literal("ssn"),
            "V" => Object::string_literal("secret-field-value"),
            "Rect" => vec![300.into(), 600.into(), 500.into(), 620.into()],
            "P" => page1,
        });
        doc.get_dictionary_mut(page1).unwrap().set("Annots", vec![field.into()]);
        let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(catalog)
            .unwrap()
            .set("AcroForm", dictionary! { "Fields" => vec![field.into()] });
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn redaction_removes_content_from_the_file() {
        let bytes = sample_with_form_field();
        assert!(all_text(&bytes).contains("Page 1 - Name:"));
        assert!(all_text(&bytes).contains("secret-field-value"));
        let loaded = load(bytes.clone()).unwrap();

        let typed = "typed-secret";
        let items = vec![
            // Something the user typed on page 1, later covered by a redaction.
            Item { id: 1, page: 0, x: 80., y: 120., w: 100., h: 16., kind: ItemKind::Text { text: typed.into(), font_size: 12. } },
            Item { id: 2, page: 0, x: 60., y: 60., w: 300., h: 90., kind: ItemKind::Redact },
            // A field on the other page must survive as real text.
            Item { id: 3, page: 1, x: 80., y: 120., w: 100., h: 16., kind: ItemKind::Text { text: "kept".into(), font_size: 12. } },
        ];
        let out = write(&bytes, &loaded.pages, &items, Some(&Watermark::default()), TextMetrics::default()).unwrap();
        if let Some(dir) = std::env::var_os("PDF_SIGNER_TEST_OUT").map(PathBuf::from) {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("redacted.pdf"), &out).unwrap();
        }

        let text = all_text(&out);
        assert!(!text.contains("Page 1 - Name:"), "redacted page text must be gone");
        assert!(!text.contains(&winansi_hex(typed)), "overlay text on a redacted page must be gone");
        assert!(!text.contains("secret-field-value"), "form values on a redacted page must be gone");
        assert!(text.contains("Page 2 - Name:"), "other pages keep their text");
        assert!(text.contains(&winansi_hex("kept")));

        // The redacted page looks the same, except for the black box.
        let reloaded = load(out.clone()).unwrap();
        assert_eq!(reloaded.pages.len(), 2);
        let page = render_page(&reloaded.pdf, 0, 1.0).unwrap();
        let pixel = |x: u32, y: u32| {
            let i = ((y * page.width + x) * 4) as usize;
            [page.bgra[i], page.bgra[i + 1], page.bgra[i + 2]]
        };
        assert_eq!(pixel(200, 100), [0, 0, 0], "inside the box");
        assert_eq!(pixel(400, 600), [255, 255, 255], "blank area stays white");
        // The rule under the name (y ≈ 102, x 160–400) continues outside the box.
        let rule = (95..110).any(|y| pixel(380, y)[0] < 200);
        assert!(rule, "content outside the box is still drawn");

        // Rotated, cropped page 2 can be redacted too.
        let items = vec![Item { id: 1, page: 1, x: 0., y: 0., w: 750., h: 580., kind: ItemKind::Redact }];
        let out = write(&bytes, &loaded.pages, &items, None, TextMetrics::default()).unwrap();
        assert!(!all_text(&out).contains("Page 2 - Name:"));
        let reloaded = load(out).unwrap();
        assert_eq!((reloaded.pages[1].width, reloaded.pages[1].height), (750., 580.));
        let page = render_page(&reloaded.pdf, 1, 0.5).unwrap();
        assert!(page.bgra.chunks_exact(4).all(|p| p[..3] == [0, 0, 0]), "fully redacted page is black");
    }

    #[test]
    fn watermark_is_drawn_on_every_page_and_previews_identically() {
        let bytes = sample_pdf();
        let loaded = load(bytes.clone()).unwrap();
        let watermark = Watermark { text: "DRAFT".into(), color: 0xff0000, opacity: 0.5, diagonal: true };
        let out = write(&bytes, &loaded.pages, &[], Some(&watermark), TextMetrics::default()).unwrap();
        let text = all_text(&out);
        assert_eq!(text.matches(&winansi_hex("DRAFT")).count(), 2, "one watermark per page");
        assert!(text.contains("Helvetica-Bold"));

        // Rendered output: reddish, semi-transparent pixels around the page centre.
        let reloaded = load(out).unwrap();
        for index in 0..2 {
            let page = render_page(&reloaded.pdf, index, 1.0).unwrap();
            let tinted = page
                .bgra
                .chunks_exact(4)
                .filter(|p| p[2] > 200 && p[0] < 180 && p[0] > 60)
                .count();
            assert!(tinted > 500, "page {index}: expected a visible watermark, found {tinted} tinted pixels");
        }

        // The editor's preview: same shape, transparent elsewhere.
        let geom = &loaded.pages[0];
        let preview = render_watermark(geom.width, geom.height, &watermark, 1.0).unwrap();
        assert_eq!((preview.width, preview.height), (612, 792));
        let alpha: Vec<u8> = preview.bgra.chunks_exact(4).map(|p| p[3]).collect();
        assert_eq!(alpha[0], 0, "corners are transparent");
        assert!(alpha.iter().any(|&a| a > 100), "the text is drawn");

        // Blank text means no watermark.
        let blank = Watermark { text: "  ".into(), ..watermark };
        let out = write(&bytes, &loaded.pages, &[], Some(&blank), TextMetrics::default()).unwrap();
        assert!(!all_text(&out).contains("Helvetica-Bold"));
    }
}

//! PDF rendering (hayro) and writing (lopdf): overlays, watermarks and redaction.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_syntax::{DecryptionError, LoadPdfError, Pdf};
use hayro::kurbo::Affine;
use hayro::vello_cpu::color::palette::css::{TRANSPARENT, WHITE};
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::fonts::{self, Embedder, FontBook};
use crate::forms::{self, Field, FieldKind, FieldValue, Widget};
use crate::model::{Item, ItemKind, LINE_HEIGHT, PageGeom, RedactFill, TextMetrics, Watermark};

// ---------------------------------------------------------------------------
// Loading & rendering
// ---------------------------------------------------------------------------

pub struct LoadedPdf {
    /// The file's bytes, decrypted if it needed a password (the writer starts from these).
    pub bytes: Arc<Vec<u8>>,
    pub pdf: Arc<Pdf>,
    pub pages: Vec<PageGeom>,
    /// The password the file was opened with, if it needed one. Saved copies
    /// are protected with it too.
    pub password: Option<String>,
    /// Form fields that can be filled in.
    pub form: Arc<Vec<Field>>,
}

/// A PDF that can't be opened without (the right) password.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordError {
    Required,
    Incorrect,
}

impl std::fmt::Display for PasswordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Required => "this PDF is password protected",
            Self::Incorrect => "the password is incorrect",
        })
    }
}

impl std::error::Error for PasswordError {}

pub fn load(bytes: Vec<u8>) -> Result<LoadedPdf> {
    load_with_password(bytes, None)
}

/// Open a PDF, decrypting it with `password` if it needs one. Fails with a
/// [`PasswordError`] when a password is needed but missing or wrong.
pub fn load_with_password(bytes: Vec<u8>, password: Option<&str>) -> Result<LoadedPdf> {
    load_inner(bytes, password, false)
}

/// Open a PDF for the editor: like [`load_with_password`], but the pages
/// render without the form fields that can be filled in, as the editor draws
/// those itself (with the values being typed).
pub fn open_for_editing(bytes: Vec<u8>, password: Option<&str>) -> Result<LoadedPdf> {
    load_inner(bytes, password, true)
}

fn load_inner(bytes: Vec<u8>, password: Option<&str>, hide_fields: bool) -> Result<LoadedPdf> {
    let bytes = Arc::new(bytes);
    let (bytes, pdf, password) = match Pdf::new(bytes.clone()) {
        Ok(pdf) => (bytes, pdf, None),
        Err(LoadPdfError::Decryption(DecryptionError::PasswordProtected)) => {
            let Some(password) = password else { return Err(PasswordError::Required.into()) };
            // Work on a decrypted copy from here on: the renderer and the writer
            // both see the same plain file. (lopdf takes the owner password too.)
            let plain = Arc::new(decrypt(&bytes, password)?);
            let pdf = Pdf::new(plain.clone()).map_err(|e| anyhow!("could not read the decrypted PDF: {e:?}"))?;
            (plain, pdf, Some(password.to_string()))
        }
        Err(LoadPdfError::Decryption(e)) => bail!("this PDF's encryption isn't supported ({e:?})"),
        Err(e) => bail!("could not read PDF: {e:?}"),
    };
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
    let mut pdf = pdf;
    let mut form = Vec::new();
    if has_form(&pdf) {
        let mut doc = Document::load_mem(&bytes).context("lopdf could not parse the PDF")?;
        form = forms::read(&doc, &pages);
        if hide_fields && !form.is_empty() {
            for widget in form.iter().flat_map(|f| &f.widgets) {
                if let Ok(dict) = doc.get_dictionary_mut(widget.id) {
                    let flags = dict.get(b"F").and_then(Object::as_i64).unwrap_or(0);
                    dict.set("F", flags | 2);
                }
            }
            let shown = save_document(doc)?;
            pdf = Pdf::new(shown).map_err(|e| anyhow!("could not read PDF: {e:?}"))?;
        }
    }
    Ok(LoadedPdf {
        bytes,
        pdf: Arc::new(pdf),
        pages,
        password,
        form: Arc::new(form),
    })
}

/// Whether the document has interactive form fields.
fn has_form(pdf: &Pdf) -> bool {
    use hayro::hayro_syntax::object::{Array, Dict};
    let xref = pdf.xref();
    xref.get::<Dict>(xref.root_id())
        .and_then(|root| root.get::<Dict>(b"AcroForm"))
        .and_then(|form| form.get::<Array>(b"Fields"))
        .is_some_and(|fields| fields.iter::<hayro::hayro_syntax::object::Object>().next().is_some())
}

/// The file decrypted with `password` (the user or the owner password).
fn decrypt(bytes: &[u8], password: &str) -> Result<Vec<u8>> {
    let mut doc = Document::load_mem_with_options(bytes, lopdf::LoadOptions::with_password(password)).map_err(|e| match e {
        lopdf::Error::InvalidPassword => anyhow::Error::from(PasswordError::Incorrect),
        e => anyhow!("could not decrypt the PDF: {e}"),
    })?;
    // Loading normally decrypts already; this covers files where it couldn't.
    if doc.is_encrypted() {
        doc.decrypt(password).map_err(|e| anyhow!("could not decrypt the PDF: {e}"))?;
    }
    doc.trailer.remove(b"Encrypt");
    save_document(doc)
}

/// Encrypt a finished PDF with AES-256, with `password` as both the user and
/// the owner password (so it opens with the password it was opened with).
fn protect(bytes: &[u8], password: &str) -> Result<Vec<u8>> {
    use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};

    let mut doc = Document::load_mem(bytes).context("lopdf could not parse the PDF")?;
    let mut key = [0u8; 32];
    let mut id = [0u8; 16];
    getrandom::fill(&mut key).and_then(|_| getrandom::fill(&mut id)).map_err(|e| anyhow!("no secure random numbers: {e}"))?;
    let filter: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
    let state = EncryptionState::try_from(EncryptionVersion::V5 {
        encrypt_metadata: true,
        crypt_filters: std::collections::BTreeMap::from([(b"StdCF".to_vec(), filter)]),
        file_encryption_key: &key,
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password: password,
        user_password: password,
        permissions: Permissions::all(),
    })
    .map_err(|e| anyhow!("could not set up encryption: {e}"))?;
    // Streams are compressed first: encrypted data doesn't compress.
    doc.compress();
    if !doc.trailer.has(b"ID") {
        let id = Object::String(id.to_vec(), lopdf::StringFormat::Hexadecimal);
        doc.trailer.set("ID", vec![id.clone(), id]);
    }
    doc.encrypt(&state).map_err(|e| anyhow!("could not encrypt the PDF: {e}"))?;
    let mut out = Vec::new();
    doc.save_to(&mut out).context("could not serialize PDF")?;
    Ok(out)
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
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let mut fonts = Fonts::default();
    let mut resources = PageResources::default();
    let wm = WatermarkResources::add(&mut doc, watermark);
    let ops = watermark_ops(&mut doc, width, height, flip, watermark, wm, &mut fonts, &mut resources)?;
    fonts.finish(&mut doc).ok()?;
    let content = doc.add_object(Stream::new(dictionary! {}, ops.into_bytes()));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content,
        "MediaBox" => vec![0.into(), 0.into(), (width as f64).into(), (height as f64).into()],
        "Resources" => resources.merge_into(&doc, Dictionary::new()),
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

const HELV: &str = "PdfSignerHelv";
const HELV_BOLD: &str = "PdfSignerHelvBold";
const WM_GS: &str = "PdfSignerWmGS";
const PAGE_IMAGE: &str = "PdfSignerPage";
/// Resolution of redacted pages, which are rebuilt as images: 3 × 72 = 216 dpi.
const REDACT_SCALE: f32 = 3.0;

/// Produce a new PDF with the watermark and all overlay items burned into the
/// page content, then apply any redactions.
///
/// `source_pages` are the original document's pages; `order` lists, for each
/// page of the output, which original page it is (pages left out are deleted).
/// Item page numbers refer to positions in `order`.
#[cfg(test)]
pub fn write(
    original: &[u8],
    source_pages: &[PageGeom],
    order: &[usize],
    items: &[Item],
    watermark: Option<&Watermark>,
    metrics: TextMetrics,
) -> Result<Vec<u8>> {
    let edits = Edits { order, items, watermark, metrics, ..Edits::default() };
    Ok(save(original, source_pages, &edits)?.bytes)
}

/// Everything to apply to a copy of the document.
#[derive(Default)]
pub struct Edits<'a> {
    /// For each page of the output, which original page it is.
    pub order: &'a [usize],
    pub items: &'a [Item],
    pub watermark: Option<&'a Watermark>,
    pub metrics: TextMetrics,
    /// Encrypt the result with this password.
    pub password: Option<&'a str>,
    /// The document's form fields, and the values to give them (same order).
    pub form: &'a [Field],
    pub values: &'a [FieldValue],
}

pub struct Written {
    pub bytes: Vec<u8>,
    /// Characters no installed font could draw (left out of the file).
    pub missing: BTreeSet<char>,
}

/// Produce the edited copy of a document: see [`write`].
pub fn save(original: &[u8], source_pages: &[PageGeom], edits: &Edits) -> Result<Written> {
    let Edits { order, items, watermark, metrics, password, form, values } = *edits;
    let identity = order.len() == source_pages.len() && order.iter().enumerate().all(|(i, &p)| i == p);
    let (arranged, pages): (Vec<u8>, Vec<PageGeom>) = if identity {
        (original.to_vec(), source_pages.to_vec())
    } else {
        let pages = order
            .iter()
            .map(|&i| source_pages.get(i).cloned().ok_or_else(|| anyhow!("page {} does not exist", i + 1)))
            .collect::<Result<_>>()?;
        (arrange_pages(original, order)?, pages)
    };
    let pages = pages.as_slice();
    let watermark = watermark.filter(|w| !w.text.trim().is_empty());
    let form = if values.len() == form.len() { form.iter().zip(values).collect() } else { Vec::new() };
    let (mut bytes, missing) = write_overlays(&arranged, pages, items, watermark, metrics, &form)?;
    let redactions: Vec<&Item> = items.iter().filter(|i| i.is_redaction() && i.w >= 1. && i.h >= 1.).collect();
    if !redactions.is_empty() {
        bytes = redact(&bytes, pages, &redactions)?;
    }
    if let Some(password) = password {
        bytes = protect(&bytes, password)?;
    }
    Ok(Written { bytes, missing })
}

fn write_overlays(
    original: &[u8],
    pages: &[PageGeom],
    items: &[Item],
    watermark: Option<&Watermark>,
    metrics: TextMetrics,
    form: &[(&Field, &FieldValue)],
) -> Result<(Vec<u8>, BTreeSet<char>)> {
    let mut doc = open_document(original)?;

    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    if page_ids.len() != pages.len() {
        bail!(
            "page count mismatch between renderer ({}) and writer ({})",
            pages.len(),
            page_ids.len()
        );
    }

    let mut fonts = Fonts::default();
    fill_form(&mut doc, form, metrics, &mut fonts)?;
    let watermark_res = watermark.map(|w| WatermarkResources::add(&mut doc, w));
    let mut images: HashMap<PathBuf, (ObjectId, String)> = HashMap::new();

    for (page_index, (&page_id, geom)) in page_ids.iter().zip(pages).enumerate() {
        let page_items: Vec<&Item> = items.iter().filter(|i| i.page == page_index).collect();
        if page_items.is_empty() && watermark.is_none() {
            continue;
        }

        let v = Affine::new(geom.view_to_user);
        let mut ops = String::new();
        let mut resources = PageResources::default();

        // The watermark goes over the page content but under the filled-in fields.
        if let (Some(wm), Some(res)) = (watermark, watermark_res) {
            if let Some(wm_ops) = watermark_ops(&mut doc, geom.width, geom.height, v, wm, res, &mut fonts, &mut resources) {
                ops.push_str(&wm_ops);
            }
        }

        for item in page_items {
            match &item.kind {
                ItemKind::Text { text, font_size } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    // Local frame: origin at the item's top-left, y up.
                    let m = v * Affine::translate((item.x as f64, item.y as f64)) * Affine::FLIP_Y;
                    ops.push_str(&format!("q {} cm 0 g BT\n", fmt_m(m)));
                    for (i, line) in text.split('\n').enumerate() {
                        let baseline = (metrics.baseline_ratio + i as f32 * LINE_HEIGHT) * font_size;
                        fonts.show_text(&mut doc, line, false, *font_size, (0., -baseline), &mut ops, &mut resources);
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
                    PageResources::add(&mut resources.images, &name, img_id);
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
                ItemKind::Redact { .. } => {}
            }
        }

        if ops.is_empty() {
            continue;
        }
        let merged = resources.merge_into(&doc, effective_resources(&doc, page_id));
        doc.get_dictionary_mut(page_id)?.set("Resources", merged);
        append_content(&mut doc, page_id, ops.into_bytes())?;
    }

    let missing = fonts.finish(&mut doc)?;
    Ok((save_document(doc)?, missing))
}

/// Writes text into content streams: in standard Helvetica when it can show
/// all of it (no font to embed), otherwise with embedded fonts.
#[derive(Default)]
struct Fonts {
    helvetica: [Option<ObjectId>; 2],
    embedder: Option<Embedder<'static>>,
    missing: BTreeSet<char>,
}

impl Fonts {
    /// The standard font (regular or bold) as a document object.
    fn helvetica(&mut self, doc: &mut Document, bold: bool) -> ObjectId {
        *self.helvetica[bold as usize].get_or_insert_with(|| {
            doc.add_object(dictionary! {
                "Type" => "Font",
                "Subtype" => "Type1",
                "BaseFont" => if bold { "Helvetica-Bold" } else { "Helvetica" },
                "Encoding" => "WinAnsiEncoding",
            })
        })
    }

    /// Lay out a line that Helvetica can't show.
    fn shape(&mut self, text: &str, bold: bool) -> fonts::Line {
        let line = FontBook::system().shape(text, bold);
        self.missing.extend(line.missing.iter().copied());
        line
    }

    /// Draw a shaped line (between BT and ET) with its baseline starting at `at`.
    fn show_shaped(&mut self, doc: &mut Document, line: &fonts::Line, size: f32, at: (f32, f32), ops: &mut String, res: &mut PageResources) {
        let embedder = self.embedder.get_or_insert_with(|| Embedder::new(FontBook::system()));
        embedder.show(doc, line, size, at, ops, &mut res.fonts);
    }

    /// Draw one line of text (between BT and ET) with its baseline starting at `at`.
    #[allow(clippy::too_many_arguments)]
    fn show_text(&mut self, doc: &mut Document, text: &str, bold: bool, size: f32, at: (f32, f32), ops: &mut String, res: &mut PageResources) {
        if text.is_empty() {
            return;
        }
        if winansi_encodable(text) {
            let name = if bold { HELV_BOLD } else { HELV };
            let font = self.helvetica(doc, bold);
            PageResources::add(&mut res.fonts, name, font);
            ops.push_str(&format!(
                "/{name} {} Tf 1 0 0 1 {} {} Tm <{}> Tj\n",
                num(size as f64),
                num(at.0 as f64),
                num(at.1 as f64),
                winansi_hex(text)
            ));
        } else {
            let line = self.shape(text, bold);
            self.show_shaped(doc, &line, size, at, ops, res);
        }
    }

    /// Write the embedded fonts; returns the characters no font could show.
    fn finish(self, doc: &mut Document) -> Result<BTreeSet<char>> {
        if let Some(embedder) = self.embedder {
            embedder.finish(doc)?;
        }
        Ok(self.missing)
    }
}

/// Resources a page's added content refers to, by name.
#[derive(Default)]
struct PageResources {
    fonts: Vec<(String, ObjectId)>,
    states: Vec<(String, ObjectId)>,
    images: Vec<(String, ObjectId)>,
}

impl PageResources {
    fn add(list: &mut Vec<(String, ObjectId)>, name: &str, id: ObjectId) {
        if !list.iter().any(|(n, _)| n == name) {
            list.push((name.to_string(), id));
        }
    }

    /// `res` (a page's resource dictionary) with these resources added.
    fn merge_into(&self, doc: &Document, mut res: Dictionary) -> Dictionary {
        for (category, entries) in [("Font", &self.fonts), ("ExtGState", &self.states), ("XObject", &self.images)] {
            if entries.is_empty() {
                continue;
            }
            let mut dict = res
                .get(category.as_bytes())
                .ok()
                .and_then(|o| resolve_dict(doc, o))
                .unwrap_or_default();
            for (name, id) in entries {
                dict.set(name.as_bytes().to_vec(), *id);
            }
            res.set(category, dict);
        }
        res
    }
}

fn open_document(bytes: &[u8]) -> Result<Document> {
    let mut doc = Document::load_mem(bytes).context("lopdf could not parse the PDF")?;
    if doc.is_encrypted() {
        // Most "encrypted" forms only use an owner password; try the empty user password.
        doc.decrypt("").map_err(|_| anyhow!("this PDF is password protected"))?;
        doc.trailer.remove(b"Encrypt");
    }
    Ok(doc)
}

fn save_document(mut doc: Document) -> Result<Vec<u8>> {
    doc.compress();
    let mut out = Vec::new();
    doc.save_to(&mut out).context("could not serialize PDF")?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Page order
// ---------------------------------------------------------------------------

/// Page attributes a page can inherit from its ancestors in the page tree.
const INHERITABLE: [&str; 4] = ["Resources", "MediaBox", "CropBox", "Rotate"];

/// Rebuild the page tree as `order` (indices into the original pages).
///
/// The new tree is flat, so each kept page first gets its own copy of any
/// attribute it used to inherit. Deleted pages are removed from the file, not
/// just from the page list: references to them (bookmarks, links, named
/// destinations) are cleared, form fields living on them are dropped, as is the
/// structure tree, and everything left unreferenced is pruned.
fn arrange_pages(original: &[u8], order: &[usize]) -> Result<Vec<u8>> {
    let mut doc = open_document(original)?;
    let ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    if order.is_empty() {
        bail!("a PDF needs at least one page");
    }
    let mut kept = Vec::with_capacity(order.len());
    for &i in order {
        let id = *ids.get(i).ok_or_else(|| anyhow!("page {} does not exist", i + 1))?;
        if kept.contains(&id) {
            bail!("page {} appears twice", i + 1);
        }
        kept.push(id);
    }
    let removed: HashSet<ObjectId> = ids.iter().filter(|id| !kept.contains(id)).copied().collect();

    let catalog_id = doc.trailer.get(b"Root").and_then(Object::as_reference)?;
    let root_id = doc.get_dictionary(catalog_id)?.get(b"Pages").and_then(Object::as_reference)?;

    for &id in &kept {
        for key in INHERITABLE {
            if doc.get_dictionary(id)?.has(key.as_bytes()) {
                continue;
            }
            if let Some(value) = inherited(&doc, id, key) {
                doc.get_dictionary_mut(id)?.set(key, value);
            }
        }
    }
    for &id in &kept {
        doc.get_dictionary_mut(id)?.set("Parent", root_id);
    }
    let root = doc.get_dictionary_mut(root_id)?;
    root.set("Kids", kept.iter().map(|&id| Object::Reference(id)).collect::<Vec<_>>());
    root.set("Count", kept.len() as i64);

    if !removed.is_empty() {
        let mut removed_annots = HashSet::new();
        for &id in &removed {
            removed_annots.extend(page_annotations(&doc, id));
        }
        strip_form_fields(&mut doc, &removed_annots);
        if let Ok(catalog) = doc.get_dictionary_mut(catalog_id) {
            catalog.remove(b"StructTreeRoot");
            catalog.remove(b"MarkInfo");
        }
        for object in doc.objects.values_mut() {
            clear_references(object, &removed);
        }
        doc.prune_objects();
    }
    save_document(doc)
}

/// The value of `key` inherited from the page's ancestors, if any.
fn inherited(doc: &Document, page_id: ObjectId, key: &str) -> Option<Object> {
    let mut current = doc.get_dictionary(page_id).ok()?.get(b"Parent").and_then(Object::as_reference).ok();
    for _ in 0..64 {
        let dict = doc.get_dictionary(current?).ok()?;
        if let Ok(value) = dict.get(key.as_bytes()) {
            return Some(value.clone());
        }
        current = dict.get(b"Parent").and_then(Object::as_reference).ok();
    }
    None
}

/// Replace references to any of `targets` with null, recursively.
fn clear_references(object: &mut Object, targets: &HashSet<ObjectId>) {
    match object {
        Object::Reference(id) if targets.contains(id) => *object = Object::Null,
        Object::Array(items) => items.iter_mut().for_each(|o| clear_references(o, targets)),
        Object::Dictionary(dict) => dict.iter_mut().for_each(|(_, o)| clear_references(o, targets)),
        Object::Stream(stream) => stream.dict.iter_mut().for_each(|(_, o)| clear_references(o, targets)),
        _ => {}
    }
}

/// The annotation objects of a page.
fn page_annotations(doc: &Document, page_id: ObjectId) -> Vec<ObjectId> {
    let annots = doc.get_dictionary(page_id).ok().and_then(|p| p.get(b"Annots").ok().cloned());
    let annots = match annots {
        Some(Object::Reference(id)) => doc.get_object(id).ok().cloned(),
        other => other,
    };
    match annots {
        Some(Object::Array(annots)) => annots.iter().filter_map(|a| a.as_reference().ok()).collect(),
        _ => Vec::new(),
    }
}

fn num(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

fn fmt_m(m: Affine) -> String {
    m.as_coeffs().iter().map(|c| num(*c)).collect::<Vec<_>>().join(" ")
}

/// Whether the standard fonts (in WinAnsiEncoding) can show all of `s`.
fn winansi_encodable(s: &str) -> bool {
    s.chars().all(|c| c == '?' || winansi(c) != b'?')
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

/// The transparency state for the watermark.
#[derive(Clone, Copy)]
struct WatermarkResources {
    state: ObjectId,
}

impl WatermarkResources {
    fn add(doc: &mut Document, watermark: &Watermark) -> Self {
        let opacity = watermark.opacity.clamp(0., 1.);
        let state = doc.add_object(dictionary! {
            "Type" => "ExtGState",
            "CA" => opacity,
            "ca" => opacity,
        });
        Self { state }
    }
}

/// Content-stream operators that draw the watermark on a page of the given
/// displayed size. `v` maps view space to the page's user space.
#[allow(clippy::too_many_arguments)]
fn watermark_ops(
    doc: &mut Document,
    width: f32,
    height: f32,
    v: Affine,
    watermark: &Watermark,
    wm: WatermarkResources,
    fonts: &mut Fonts,
    res: &mut PageResources,
) -> Option<String> {
    let text = watermark.text.trim();
    // Width in em, and a shaped line when Helvetica can't show the text.
    let (em, shaped) = if winansi_encodable(text) {
        (text.chars().map(|c| HELVETICA_BOLD_WIDTHS[winansi(c) as usize] as f64).sum::<f64>() / 1000., None)
    } else {
        let line = fonts.shape(text, true);
        (line.width as f64, Some(line))
    };
    if em <= 0. {
        return None;
    }
    const CAP_HEIGHT: f64 = 0.718; // Helvetica-Bold, in em (close enough for other fonts)
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
    PageResources::add(&mut res.states, WM_GS, wm.state);
    let mut ops = format!("q /{WM_GS} gs {r} {g} {b} rg {} cm BT\n", fmt_m(m));
    let at = ((-em * size / 2.) as f32, (-CAP_HEIGHT * size / 2.) as f32);
    match shaped {
        Some(line) => fonts.show_shaped(doc, &line, size as f32, at, &mut ops, res),
        None => fonts.show_text(doc, text, true, size as f32, at, &mut ops, res),
    }
    ops.push_str("ET Q\n");
    Some(ops)
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
// Form fields
// ---------------------------------------------------------------------------

/// Write changed field values into the form: the value itself (so other apps
/// read it and the form stays fillable) and a new appearance showing it.
fn fill_form(doc: &mut Document, form: &[(&Field, &FieldValue)], metrics: TextMetrics, fonts: &mut Fonts) -> Result<()> {
    let mut changed = false;
    for &(field, value) in form {
        // Unchanged fields keep their original appearance; fields on deleted
        // pages are gone.
        if *value == field.value || doc.get_dictionary(field.id).is_err() {
            continue;
        }
        changed = true;
        match value {
            FieldValue::Text(text) => {
                let dict = doc.get_dictionary_mut(field.id)?;
                dict.set("V", lopdf::text_string(text));
                if let FieldKind::Choice { combo: false, options, .. } = &field.kind {
                    match options.iter().position(|o| o.value == *text) {
                        Some(i) => dict.set("I", vec![Object::Integer(i as i64)]),
                        None => {
                            dict.remove(b"I");
                        }
                    }
                }
                for widget in &field.widgets {
                    if doc.get_dictionary(widget.id).is_err() {
                        continue;
                    }
                    let appearance = text_appearance(doc, field, widget, value, metrics, fonts);
                    let appearance = doc.add_object(appearance);
                    let dict = doc.get_dictionary_mut(widget.id)?;
                    // Only a normal appearance: rollover/down ones would show the old value.
                    dict.set("AP", dictionary! { "N" => appearance });
                }
            }
            FieldValue::On(selected) => {
                let state = selected.clone().unwrap_or_else(|| "Off".into());
                doc.get_dictionary_mut(field.id)?.set("V", Object::Name(state.into_bytes()));
                for widget in &field.widgets {
                    let Ok(dict) = doc.get_dictionary(widget.id) else { continue };
                    let on = widget.on_state.clone().unwrap_or_else(|| "Yes".into());
                    if !has_appearance(doc, dict, &on) {
                        let (on_ap, off_ap) = button_appearances(widget);
                        let (on_ap, off_ap) = (doc.add_object(on_ap), doc.add_object(off_ap));
                        let mut normal = Dictionary::new();
                        normal.set(on.as_bytes().to_vec(), on_ap);
                        normal.set("Off", off_ap);
                        doc.get_dictionary_mut(widget.id)?.set("AP", dictionary! { "N" => normal });
                    }
                    let shown = if selected.as_deref() == Some(on.as_str()) { on } else { "Off".into() };
                    doc.get_dictionary_mut(widget.id)?.set("AS", Object::Name(shown.into_bytes()));
                }
            }
        }
    }
    if changed {
        // An XFA form inside the file would take precedence over the fields in
        // some apps, showing the old values.
        let catalog_id = doc.trailer.get(b"Root").and_then(Object::as_reference)?;
        let form_ref = doc.get_dictionary(catalog_id)?.get(b"AcroForm").ok().cloned();
        match form_ref {
            Some(Object::Reference(id)) => {
                if let Ok(form) = doc.get_dictionary_mut(id) {
                    form.remove(b"XFA");
                }
            }
            Some(Object::Dictionary(_)) => {
                if let Ok(Object::Dictionary(form)) = doc.get_dictionary_mut(catalog_id)?.get_mut(b"AcroForm") {
                    form.remove(b"XFA");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether a button widget has a normal appearance for `state`.
fn has_appearance(doc: &Document, widget: &Dictionary, state: &str) -> bool {
    widget
        .get(b"AP")
        .ok()
        .and_then(|ap| resolve_dict(doc, ap))
        .and_then(|ap| ap.get(b"N").ok().and_then(|n| resolve_dict(doc, n)))
        .is_some_and(|normal| normal.has(state.as_bytes()))
}

/// Width of `text` at `size`, as the writer will draw it.
pub fn measure_text(text: &str, size: f32) -> f32 {
    if winansi_encodable(text) {
        text.chars().map(|c| HELVETICA_WIDTHS[winansi(c) as usize] as f32).sum::<f32>() / 1000. * size
    } else {
        FontBook::system().shape(text, false).width * size
    }
}

/// The start of a widget's appearance: the form XObject dictionary, and its
/// background and border.
fn appearance_frame(widget: &Widget) -> (Dictionary, String) {
    let (w, h) = widget.size;
    let mut dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "BBox" => vec![0.into(), 0.into(), Object::Real(w), Object::Real(h)],
    };
    // Turned widgets: the viewer fits the rotated box onto the widget's rect.
    let matrix = match widget.rotation {
        90 => Some([0, 1, -1, 0]),
        180 => Some([-1, 0, 0, -1]),
        270 => Some([0, -1, 1, 0]),
        _ => None,
    };
    if let Some([a, b, c, d]) = matrix {
        dict.set("Matrix", vec![a.into(), b.into(), c.into(), d.into(), 0.into(), 0.into()]);
    }
    let mut ops = String::new();
    let (w, h) = (w as f64, h as f64);
    if let Some(bg) = widget.background {
        ops.push_str(&format!("{} rg 0 0 {} {} re f\n", color(bg), num(w), num(h)));
    }
    if let (Some(bc), bw) = (widget.border, widget.border_width as f64) {
        if bw > 0. {
            ops.push_str(&format!(
                "{} RG {} w {} {} {} {} re S\n",
                color(bc),
                num(bw),
                num(bw / 2.),
                num(bw / 2.),
                num(w - bw),
                num(h - bw)
            ));
        }
    }
    (dict, ops)
}

fn color([r, g, b]: [f32; 3]) -> String {
    format!("{} {} {}", num(r as f64), num(g as f64), num(b as f64))
}

/// The appearance of a text or choice field showing `value`.
fn text_appearance(
    doc: &mut Document,
    field: &Field,
    widget: &Widget,
    value: &FieldValue,
    metrics: TextMetrics,
    fonts: &mut Fonts,
) -> Stream {
    let (mut dict, mut ops) = appearance_frame(widget);
    let h = widget.size.1;
    let layout = forms::layout(field, widget, value, metrics.baseline_ratio, &mut measure_text);
    let mut resources = PageResources::default();
    ops.push_str("/Tx BMC\nq\n");
    let [cx, cy, cw, ch] = layout.clip;
    if let Some((top, height)) = layout.highlight {
        ops.push_str(&format!(
            "{} rg {} {} {} {} re f\n",
            color(forms::LIST_HIGHLIGHT),
            num(cx as f64),
            num((h - top - height) as f64),
            num(cw as f64),
            num(height as f64)
        ));
    }
    ops.push_str(&format!(
        "{} {} {} {} re W n\n{} rg BT\n",
        num(cx as f64),
        num((h - cy - ch) as f64),
        num(cw as f64),
        num(ch as f64),
        color(widget.text_color)
    ));
    for run in &layout.runs {
        fonts.show_text(doc, &run.text, false, layout.size, (run.x, h - run.baseline), &mut ops, &mut resources);
    }
    ops.push_str("ET\nQ\nEMC\n");
    dict.set("Resources", resources.merge_into(doc, Dictionary::new()));
    let mut stream = Stream::new(dict, ops.into_bytes());
    let _ = stream.compress();
    stream
}

/// "On" and "Off" appearances for a check box or radio button that has none.
fn button_appearances(widget: &Widget) -> (Stream, Stream) {
    let (dict, frame) = appearance_frame(widget);
    let (w, h) = widget.size;
    let d = (w.min(h) - 2. * (widget.border_width + 1.)).max(1.);
    let (x0, y0) = ((w - d) / 2., (h - d) / 2.);
    let p = |(x, y): (f32, f32)| format!("{} {}", num((x0 + x * d) as f64), num((y0 + y * d) as f64));
    let mut mark = format!("q {} rg {} RG 1 J 1 j\n", color(widget.text_color), color(widget.text_color));
    match widget.symbol.mark() {
        forms::Mark::Lines(lines) => {
            mark.push_str(&format!("{} w\n", num((d * 0.12) as f64)));
            for line in lines {
                for (i, &point) in line.iter().enumerate() {
                    mark.push_str(&format!("{} {}\n", p(point), if i == 0 { "m" } else { "l" }));
                }
                mark.push_str("S\n");
            }
        }
        forms::Mark::Polygon(points) => {
            for (i, &point) in points.iter().enumerate() {
                mark.push_str(&format!("{} {}\n", p(point), if i == 0 { "m" } else { "l" }));
            }
            mark.push_str("h f\n");
        }
        forms::Mark::Disc(r) => {
            // A circle from four Bézier curves.
            let (cx, cy, r) = ((w / 2.) as f64, (h / 2.) as f64, (r * d) as f64);
            let k = r * 0.5523;
            mark.push_str(&format!(
                "{} {} m {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c f\n",
                num(cx + r), num(cy),
                num(cx + r), num(cy + k), num(cx + k), num(cy + r), num(cx), num(cy + r),
                num(cx - k), num(cy + r), num(cx - r), num(cy + k), num(cx - r), num(cy),
                num(cx - r), num(cy - k), num(cx - k), num(cy - r), num(cx), num(cy - r),
                num(cx + k), num(cy - r), num(cx + r), num(cy - k), num(cx + r), num(cy),
            ));
        }
    }
    mark.push_str("Q\n");
    let on = Stream::new(dict.clone(), format!("{frame}{mark}").into_bytes());
    let off = Stream::new(dict, frame.into_bytes());
    (on, off)
}

/// Advance widths (1/1000 em) of Helvetica, indexed by WinAnsi code (from the
/// standard Adobe font metrics, as bundled with hayro).
#[rustfmt::skip]
const HELVETICA_WIDTHS: [u16; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556,
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556,
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556,
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, 0,
    556, 0, 222, 556, 333, 1000, 556, 556, 333, 1000, 667, 333, 1000, 0, 611, 0,
    0, 222, 222, 333, 333, 350, 556, 1000, 333, 1000, 500, 333, 944, 0, 500, 667,
    0, 333, 556, 556, 556, 556, 260, 556, 333, 737, 370, 556, 584, 0, 737, 333,
    400, 584, 333, 333, 333, 556, 537, 278, 333, 333, 365, 556, 834, 834, 834, 611,
    667, 667, 667, 667, 667, 667, 1000, 722, 667, 667, 667, 667, 278, 278, 278, 278,
    722, 722, 778, 778, 778, 778, 778, 584, 778, 722, 722, 722, 722, 667, 667, 611,
    556, 556, 556, 556, 556, 556, 889, 500, 556, 556, 556, 556, 278, 278, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 584, 611, 556, 556, 556, 556, 500, 556, 500,
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
            let fill = match r.kind {
                ItemKind::Redact { fill } => fill,
                _ => RedactFill::Black,
            };
            let x0 = ((r.x * sx).floor().max(0.) as usize).min(pw);
            let x1 = (((r.x + r.w) * sx).ceil().max(0.) as usize).min(pw);
            let y0 = ((r.y * sy).floor().max(0.) as usize).min(ph);
            let y1 = (((r.y + r.h) * sy).ceil().max(0.) as usize).min(ph);
            for y in y0..y1 {
                rgb[(y * pw + x0) * 3..(y * pw + x1) * 3].fill(fill.level());
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

        removed_annots.extend(page_annotations(&doc, page_id));

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
    save_document(doc)
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
    strip_form_fields(doc, removed_annots);
}

/// Drop form fields that are (or whose widgets all are) among `removed_annots`.
fn strip_form_fields(doc: &mut Document, removed_annots: &HashSet<ObjectId>) {
    let Ok(catalog_id) = doc.trailer.get(b"Root").and_then(Object::as_reference) else { return };

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
        let written = write(&bytes, &loaded.pages, &[0, 1], &items, Some(&watermark), TextMetrics::default()).unwrap();
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
            Item { id: 2, page: 0, x: 60., y: 60., w: 300., h: 90., kind: ItemKind::Redact { fill: RedactFill::Black } },
            // A field on the other page must survive as real text.
            Item { id: 3, page: 1, x: 80., y: 120., w: 100., h: 16., kind: ItemKind::Text { text: "kept".into(), font_size: 12. } },
        ];
        let out = write(&bytes, &loaded.pages, &[0, 1], &items, Some(&Watermark::default()), TextMetrics::default()).unwrap();
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
        let items = vec![Item { id: 1, page: 1, x: 0., y: 0., w: 750., h: 580., kind: ItemKind::Redact { fill: RedactFill::White } }];
        let out = write(&bytes, &loaded.pages, &[0, 1], &items, None, TextMetrics::default()).unwrap();
        assert!(!all_text(&out).contains("Page 2 - Name:"));
        let reloaded = load(out).unwrap();
        assert_eq!((reloaded.pages[1].width, reloaded.pages[1].height), (750., 580.));
        let page = render_page(&reloaded.pdf, 1, 0.5).unwrap();
        assert!(page.bgra.chunks_exact(4).all(|p| p[..3] == [255, 255, 255]), "a white redaction leaves a blank page");
    }

    #[test]
    fn watermark_is_drawn_on_every_page_and_previews_identically() {
        let bytes = sample_pdf();
        let loaded = load(bytes.clone()).unwrap();
        let watermark = Watermark { text: "DRAFT".into(), color: 0xff0000, opacity: 0.5, diagonal: true };
        let out = write(&bytes, &loaded.pages, &[0, 1], &[], Some(&watermark), TextMetrics::default()).unwrap();
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
        let out = write(&bytes, &loaded.pages, &[0, 1], &[], Some(&blank), TextMetrics::default()).unwrap();
        assert!(!all_text(&out).contains("Helvetica-Bold"));
    }

    /// `bytes` encrypted the way other apps do it (RC4, 128-bit), with separate
    /// user and owner passwords.
    pub(crate) fn encrypted(bytes: &[u8], user: &str, owner: &str) -> Vec<u8> {
        use lopdf::{EncryptionState, EncryptionVersion, Permissions};
        let mut doc = Document::load_mem(bytes).unwrap();
        let id = Object::String(b"0123456789abcdef".to_vec(), lopdf::StringFormat::Hexadecimal);
        doc.trailer.set("ID", vec![id.clone(), id]);
        let state = EncryptionState::try_from(EncryptionVersion::V2 {
            document: &doc,
            owner_password: owner,
            user_password: user,
            key_length: 128,
            permissions: Permissions::PRINTABLE,
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn password_error(result: Result<LoadedPdf>) -> Option<PasswordError> {
        result.err().and_then(|e| e.downcast_ref::<PasswordError>().copied())
    }

    #[test]
    fn password_protected_files_open_with_their_password_and_stay_protected() {
        let bytes = encrypted(&sample_with_form_field(), "open sesame", "owner secret");
        assert!(!String::from_utf8_lossy(&bytes).contains("Page 1 - Name:"), "the content is encrypted");
        assert_eq!(password_error(load(bytes.clone())), Some(PasswordError::Required));
        assert_eq!(password_error(load_with_password(bytes.clone(), Some("wrong"))), Some(PasswordError::Incorrect));

        // Either password opens it.
        for password in ["open sesame", "owner secret"] {
            let loaded = load_with_password(bytes.clone(), Some(password)).unwrap();
            assert_eq!(loaded.pages.len(), 2);
            assert_eq!(loaded.password.as_deref(), Some(password));
            let page = render_page(&loaded.pdf, 0, 1.0).unwrap();
            assert!(page.bgra.chunks_exact(4).filter(|p| p[0] < 100).count() > 200, "the page renders");
        }

        // The saved copy needs the same password, and has everything in it.
        let loaded = load_with_password(bytes, Some("open sesame")).unwrap();
        let items = [Item { id: 1, page: 0, x: 80., y: 120., w: 60., h: 16., kind: ItemKind::Text { text: "signed".into(), font_size: 12. } }];
        let edits = Edits { order: &[0, 1], items: &items, password: loaded.password.as_deref(), ..Edits::default() };
        let out = save(&loaded.bytes, &loaded.pages, &edits).unwrap().bytes;
        assert!(!String::from_utf8_lossy(&out).contains("Page 1 - Name:"));
        assert_eq!(password_error(load(out.clone())), Some(PasswordError::Required));
        let reopened = load_with_password(out, Some("open sesame")).unwrap();
        let text = all_text(&reopened.bytes);
        assert!(text.contains("Page 1 - Name:"));
        assert!(text.contains("secret-field-value"));
        assert!(text.contains(&winansi_hex("signed")));
    }

    #[test]
    fn files_with_only_an_owner_password_open_without_asking() {
        let bytes = encrypted(&sample_pdf(), "", "owner secret");
        let loaded = load(bytes).unwrap();
        assert_eq!(loaded.password, None);
        let out = write(&loaded.bytes, &loaded.pages, &[0, 1], &[], None, TextMetrics::default()).unwrap();
        assert!(all_text(&out).contains("Page 2 - Name:"));
    }

    /// Inked (not white or pale) pixels in a view-space box of a rendered page (scale 1).
    fn ink(page: &RenderedPage, x: std::ops::Range<u32>, y: std::ops::Range<u32>) -> usize {
        y.flat_map(|y| x.clone().map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let i = ((y * page.width + x) * 4) as usize;
                page.bgra[i..i + 3].iter().any(|&c| c < 128)
            })
            .count()
    }

    #[test]
    fn text_in_other_scripts_is_written_with_an_embedded_font() {
        // Greek and Cyrillic: in the default fonts of every desktop OS.
        let text = "Ελληνικά Кириллица";
        if !FontBook::system().shape(text, false).missing.is_empty() {
            eprintln!("skipping: no installed font for {text}");
            return;
        }
        let bytes = sample_pdf();
        let loaded = load(bytes.clone()).unwrap();
        let items = [
            Item { id: 1, page: 0, x: 80., y: 300., w: 200., h: 20., kind: ItemKind::Text { text: text.into(), font_size: 16. } },
            Item { id: 2, page: 1, x: 80., y: 300., w: 60., h: 20., kind: ItemKind::Text { text: "plain".into(), font_size: 16. } },
        ];
        let edits = Edits { order: &[0, 1], items: &items, ..Edits::default() };
        let written = save(&bytes, &loaded.pages, &edits).unwrap();
        assert!(written.missing.is_empty());
        let all = all_text(&written.bytes);
        assert!(all.contains("/Identity-H") && all.contains("/ToUnicode"), "a CID font");
        assert!(all.contains("FontFile2") || all.contains("FontFile3"), "the font is embedded");
        assert!(all.contains("<0395>"), "copying the text gives back Ε");
        assert!(page_content(&written.bytes, 2).contains(&winansi_hex("plain")), "Latin text still uses Helvetica");

        let reloaded = load(written.bytes).unwrap();
        let page = render_page(&reloaded.pdf, 0, 1.0).unwrap();
        assert!(ink(&page, 80..300, 300..320) > 150, "the text is drawn");
    }

    #[test]
    fn watermarks_in_other_scripts_render() {
        let text = "Секретно";
        if !FontBook::system().shape(text, true).missing.is_empty() {
            eprintln!("skipping: no installed font for {text}");
            return;
        }
        let watermark = Watermark { text: text.into(), color: 0, opacity: 1., diagonal: false };
        let preview = render_watermark(612., 792., &watermark, 1.0).unwrap();
        let drawn = preview.bgra.chunks_exact(4).filter(|p| p[3] > 128).count();
        assert!(drawn > 2000, "only {drawn} pixels drawn");
    }

    /// The sample with a form: text fields, a check box, radio buttons, a combo
    /// box, a read-only field, and a field on the rotated second page.
    pub(crate) fn sample_form() -> Vec<u8> {
        let mut doc = Document::load_mem(&sample_pdf()).unwrap();
        let pages = doc.get_pages();
        let (page1, page2) = (pages[&1], pages[&2]);
        let rect = |r: [i64; 4]| r.iter().map(|&v| Object::Integer(v)).collect::<Vec<_>>();
        let mut stream = |text: &str| doc.add_object(Stream::new(dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => rect([0, 0, 15, 15]) }, text.as_bytes().to_vec()));
        let (yes, off, s_on, l_on, off2) = (stream("0 0 1 rg 2 2 11 11 re f"), stream(""), stream("0 g 3 3 6 6 re f"), stream("0 g 3 3 6 6 re f"), stream(""));
        fn field(doc: &mut Document, fields: &mut Vec<Object>, annots: &mut Vec<Object>, dict: Dictionary) -> ObjectId {
            let id = doc.add_object(dict);
            fields.push(Object::Reference(id));
            annots.push(Object::Reference(id));
            id
        }
        let mut fields = Vec::new();
        let mut annots1 = Vec::new();
        field(&mut doc, &mut fields, &mut annots1, dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "T" => Object::string_literal("name"),
            "Rect" => rect([100, 600, 300, 620]), "DA" => Object::string_literal("/Helv 0 Tf 0 g"),
            "MK" => dictionary! { "BC" => vec![0.into(), 0.into(), 1.into()] }, "P" => page1,
        });
        field(&mut doc, &mut fields, &mut annots1, dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "T" => Object::string_literal("notes"), "Ff" => 4096,
            "Rect" => rect([100, 450, 300, 520]), "DA" => Object::string_literal("/Helv 10 Tf 0 0 1 rg"),
        });
        field(&mut doc, &mut fields, &mut annots1, dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Btn", "T" => Object::string_literal("agree"),
            "Rect" => rect([100, 400, 115, 415]), "V" => "Off", "AS" => "Off",
            "AP" => dictionary! { "N" => dictionary! { "Yes" => yes, "Off" => off } },
        });
        // A radio group: the field, and one widget per button.
        let group = doc.new_object_id();
        let kid = |doc: &mut Document, r: [i64; 4], on: &str, ap: ObjectId| {
            let mut normal = Dictionary::new();
            normal.set(on.as_bytes().to_vec(), ap);
            normal.set("Off", off2);
            doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Widget", "Parent" => group, "Rect" => rect(r), "AP" => dictionary! { "N" => normal }, "AS" => if on == "S" { on } else { "Off" } })
        };
        let (k1, k2) = (kid(&mut doc, [100, 360, 112, 372], "S", s_on), kid(&mut doc, [140, 360, 152, 372], "L", l_on));
        doc.objects.insert(group, Object::Dictionary(dictionary! {
            "FT" => "Btn", "T" => Object::string_literal("size"), "Ff" => 49152, "V" => "S", "Kids" => vec![k1.into(), k2.into()],
        }));
        fields.push(group.into());
        annots1.extend([k1.into(), k2.into()]);
        field(&mut doc, &mut fields, &mut annots1, dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Ch", "T" => Object::string_literal("country"), "Ff" => 131072,
            "Opt" => vec![
                vec![Object::string_literal("US"), Object::string_literal("United States")].into(),
                vec![Object::string_literal("FR"), Object::string_literal("France")].into(),
            ],
            "V" => Object::string_literal("FR"), "Rect" => rect([100, 300, 250, 318]), "DA" => Object::string_literal("/Helv 12 Tf 0 g"),
        });
        field(&mut doc, &mut fields, &mut annots1, dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "T" => Object::string_literal("id"), "Ff" => 1,
            "V" => Object::string_literal("A-1"), "Rect" => rect([400, 600, 500, 620]),
        });
        let mut annots2 = Vec::new();
        field(&mut doc, &mut fields, &mut annots2, dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "T" => Object::string_literal("page2"),
            "Rect" => rect([100, 100, 300, 120]), "DA" => Object::string_literal("/Helv 12 Tf 0 g"),
        });
        // Laid out for the turned page, as form editors do: tall in the file, wide on screen.
        field(&mut doc, &mut fields, &mut annots2, dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "T" => Object::string_literal("upright"),
            "Rect" => rect([400, 300, 420, 500]), "DA" => Object::string_literal("/Helv 12 Tf 0 g"),
            "MK" => dictionary! { "R" => 90 },
        });
        doc.get_dictionary_mut(page1).unwrap().set("Annots", annots1);
        doc.get_dictionary_mut(page2).unwrap().set("Annots", annots2);
        let helv = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(catalog).unwrap().set(
            "AcroForm",
            dictionary! {
                "Fields" => fields,
                "DA" => Object::string_literal("/Helv 0 Tf 0 g"),
                "DR" => dictionary! { "Font" => dictionary! { "Helv" => helv } },
                "XFA" => Object::string_literal("<xdp/>"),
            },
        );
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn value_of<'a>(form: &'a [Field], name: &str) -> (&'a Field, FieldValue) {
        let field = form.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("no field {name}"));
        (field, field.value.clone())
    }

    #[test]
    fn form_fields_are_read() {
        let loaded = load(sample_form()).unwrap();
        let names: Vec<&str> = loaded.form.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["name", "notes", "agree", "size", "country", "page2", "upright"], "read-only fields are left out");

        let (name, _) = value_of(&loaded.form, "name");
        assert_eq!(name.widgets[0].rect, [100., 172., 200., 20.], "in view space");
        assert_eq!(name.widgets[0].font_size, 0.);
        assert_eq!(name.widgets[0].border, Some([0., 0., 1.]));
        let (notes, _) = value_of(&loaded.form, "notes");
        assert_eq!(notes.kind, FieldKind::Text { multiline: true, comb: false });
        assert_eq!(notes.widgets[0].text_color, [0., 0., 1.]);
        let (agree, value) = value_of(&loaded.form, "agree");
        assert_eq!((value, agree.widgets[0].on_state.as_deref()), (FieldValue::On(None), Some("Yes")));
        let (size, value) = value_of(&loaded.form, "size");
        assert_eq!(value, FieldValue::On(Some("S".into())));
        assert_eq!(size.kind, FieldKind::Radio { no_toggle_off: true });
        let states: Vec<_> = size.widgets.iter().map(|w| w.on_state.clone().unwrap()).collect();
        assert_eq!(states, ["S", "L"]);
        let (country, value) = value_of(&loaded.form, "country");
        assert_eq!(country.display_text(&value), "France");
        // On the rotated page, the widget is upright in view space.
        let (page2, _) = value_of(&loaded.form, "page2");
        assert_eq!(page2.widgets[0].page, 1);
        let [x, y, w, h] = page2.widgets[0].rect;
        assert!(w < h, "a wide field on a page turned 90° shows tall: {x} {y} {w} {h}");
        assert!(!page2.widgets[0].upright);
        let (upright, _) = value_of(&loaded.form, "upright");
        let [_, _, w, h] = upright.widgets[0].rect;
        assert!(upright.widgets[0].upright && w > h);
        assert_eq!(upright.widgets[0].size, (200., 20.));
        assert!(name.widgets[0].upright);
    }

    /// Fields and their values as another app would read them from `pdf`.
    fn saved_values(pdf: &[u8]) -> Vec<(String, FieldValue)> {
        load(pdf.to_vec()).unwrap().form.iter().map(|f| (f.name.clone(), f.value.clone())).collect()
    }

    #[test]
    fn filled_in_fields_are_saved_in_the_form() {
        let bytes = sample_form();
        let loaded = load(bytes.clone()).unwrap();
        let mut values: Vec<FieldValue> = loaded.form.iter().map(|f| f.value.clone()).collect();
        let set = |values: &mut Vec<FieldValue>, name: &str, value: FieldValue| {
            let i = loaded.form.iter().position(|f| f.name == name).unwrap();
            values[i] = value;
        };
        set(&mut values, "name", FieldValue::Text("Jane Doe".into()));
        set(&mut values, "notes", FieldValue::Text("A note long enough to wrap onto another line\nand a second".into()));
        set(&mut values, "agree", FieldValue::On(Some("Yes".into())));
        set(&mut values, "size", FieldValue::On(Some("L".into())));
        set(&mut values, "country", FieldValue::Text("US".into()));
        let edits = Edits { order: &[0, 1], form: &loaded.form, values: &values, ..Edits::default() };
        let out = save(&bytes, &loaded.pages, &edits).unwrap().bytes;

        // Other apps see the new values in a form that is still fillable.
        let saved = saved_values(&out);
        let expected: Vec<(String, FieldValue)> = loaded.form.iter().map(|f| f.name.clone()).zip(values.clone()).collect();
        assert_eq!(saved, expected);
        let doc = Document::load_mem(&out).unwrap();
        let form = doc.catalog().unwrap().get(b"AcroForm").unwrap().as_dict().unwrap();
        assert!(!form.has(b"XFA"), "stale XFA data is dropped");
        let (size, _) = value_of(&loaded.form, "size");
        let state = |w: &Widget| doc.get_dictionary(w.id).unwrap().get(b"AS").unwrap().as_name().unwrap().to_vec();
        assert_eq!(state(&size.widgets[0]), b"Off");
        assert_eq!(state(&size.widgets[1]), b"L");

        // The new appearances show the values.
        let page = render_page(&load(out.clone()).unwrap().pdf, 0, 1.0).unwrap();
        assert!(ink(&page, 102..298, 174..190) > 60, "the name is drawn in its field");
        assert!(ink(&page, 102..298, 274..340) > 200, "the note is drawn, wrapped");
        let all = all_text(&out);
        assert!(all.contains(&winansi_hex("Jane Doe")));
        assert!(all.contains(&winansi_hex("United States")), "a choice shows its label");

        // In the editor's copy, the fields that can be filled in aren't drawn
        // (the editor draws them itself); read-only ones still are.
        let shown = open_for_editing(bytes.clone(), None).unwrap();
        let blank = render_page(&shown.pdf, 0, 1.0).unwrap();
        let original = render_page(&load(bytes.clone()).unwrap().pdf, 0, 1.0).unwrap();
        // The selected radio button (x 100–112, y 420–432 on screen).
        assert!(ink(&original, 100..112, 420..432) > 10, "drawn normally");
        assert_eq!(ink(&blank, 100..112, 420..432), 0, "hidden in the editor's copy");

        // Deleting the first page drops its fields; the rest are still filled.
        let edits = Edits { order: &[1], form: &loaded.form, values: &values, ..Edits::default() };
        let out = save(&bytes, &loaded.pages, &edits).unwrap().bytes;
        let saved = saved_values(&out);
        assert_eq!(saved.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["page2", "upright"]);
    }

    #[test]
    fn form_values_in_other_scripts_embed_a_font() {
        let value = "Ελλάδα";
        if !FontBook::system().shape(value, false).missing.is_empty() {
            eprintln!("skipping: no installed font for {value}");
            return;
        }
        let bytes = sample_form();
        let loaded = load(bytes.clone()).unwrap();
        let mut values: Vec<FieldValue> = loaded.form.iter().map(|f| f.value.clone()).collect();
        values[0] = FieldValue::Text(value.into());
        let edits = Edits { order: &[0, 1], form: &loaded.form, values: &values, ..Edits::default() };
        let out = save(&bytes, &loaded.pages, &edits).unwrap().bytes;
        assert_eq!(saved_values(&out)[0].1, FieldValue::Text(value.into()), "stored as a Unicode text string");
        assert!(all_text(&out).contains("/Identity-H"));
        let page = render_page(&load(out).unwrap().pdf, 0, 1.0).unwrap();
        assert!(ink(&page, 102..298, 174..190) > 40);
    }

    /// The decompressed content of one page of `pdf` (1-based).
    fn page_content(pdf: &[u8], page: u32) -> String {
        let mut doc = Document::load_mem(pdf).unwrap();
        doc.decompress();
        let id = doc.get_pages()[&page];
        String::from_utf8_lossy(&doc.get_page_content(id)).into_owned()
    }

    #[test]
    fn pages_can_be_reordered_and_deleted() {
        let bytes = sample_with_form_field();
        let loaded = load(bytes.clone()).unwrap();
        let note = |page| Item { id: 1, page, x: 80., y: 120., w: 60., h: 16., kind: ItemKind::Text { text: "note".into(), font_size: 12. } };

        // Swap the pages; a field on (new) page 1 lands on what was page 2.
        let out = write(&bytes, &loaded.pages, &[1, 0], &[note(0)], None, TextMetrics::default()).unwrap();
        let swapped = load(out.clone()).unwrap();
        assert_eq!(swapped.pages.len(), 2);
        // Page 2 kept its rotation and crop box, which it used to inherit from the tree.
        assert_eq!((swapped.pages[0].width, swapped.pages[0].height), (750., 580.));
        assert_eq!(swapped.pages[0].view_to_user, loaded.pages[1].view_to_user);
        assert_eq!((swapped.pages[1].width, swapped.pages[1].height), (612., 792.));
        assert!(page_content(&out, 1).contains("Page 2 - Name:"));
        assert!(page_content(&out, 1).contains(&winansi_hex("note")));
        assert!(page_content(&out, 2).contains("Page 1 - Name:"));
        // Fonts inherited from the page tree still resolve: it renders text, not nothing.
        let page = render_page(&swapped.pdf, 1, 1.0).unwrap();
        assert!(page.bgra.chunks_exact(4).filter(|p| p[0] < 100).count() > 200);

        // Delete page 1: its content and its form field are gone from the file.
        let out = write(&bytes, &loaded.pages, &[1], &[], None, TextMetrics::default()).unwrap();
        let text = all_text(&out);
        assert_eq!(load(out).unwrap().pages.len(), 1);
        assert!(!text.contains("Page 1 - Name:"), "deleted page content must be gone");
        assert!(!text.contains("secret-field-value"), "fields on a deleted page must be gone");
        assert!(text.contains("Page 2 - Name:"));

        // Nonsense orders are refused rather than producing a broken file.
        assert!(write(&bytes, &loaded.pages, &[], &[], None, TextMetrics::default()).is_err());
        assert!(write(&bytes, &loaded.pages, &[0, 0], &[], None, TextMetrics::default()).is_err());
        assert!(write(&bytes, &loaded.pages, &[2], &[], None, TextMetrics::default()).is_err());
    }
}

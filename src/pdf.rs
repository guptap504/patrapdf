//! PDF rendering (hayro) and writing (lopdf): overlays, watermarks and redaction.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

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
use crate::scan::{self, Fix, Look};

// ---------------------------------------------------------------------------
// Loading & rendering
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LoadedPdf {
    /// The file's bytes, decrypted if it needed a password (the writer starts from these).
    pub bytes: Arc<Vec<u8>>,
    /// `bytes` as the writer reads them, kept from the first time that was
    /// needed (to open the file, or to save it), so they're parsed only once.
    parsed: Arc<OnceLock<Document>>,
    pub pdf: Arc<Pdf>,
    /// The pages as the file has them.
    own_pages: Arc<Vec<PageGeom>>,
    /// The pages as they are shown: as the file has them, cropped as `fixes`
    /// says and then turned by `turns`.
    pub pages: Vec<PageGeom>,
    /// What has been done to each page since it was opened: cropped, or its
    /// colours cleaned up. Such a page is a picture of itself, shown and saved.
    pub fixes: Vec<Fix>,
    /// The quarter turns clockwise each page has been given since it was
    /// opened (0 to 3). Saved copies have their pages turned the same.
    pub turns: Vec<u8>,
    /// The password the file was opened with, if it needed one. Saved copies
    /// are protected with it too.
    pub password: Option<String>,
    /// Form fields that can be filled in.
    pub form: Arc<Vec<Field>>,
}

impl LoadedPdf {
    /// The document for the writer to change: a copy of the parsed file.
    fn document(&self) -> Result<Document> {
        if let Some(doc) = self.parsed.get() {
            return Ok(doc.clone());
        }
        let doc = open_document(&self.bytes)?;
        Ok(self.parsed.get_or_init(|| doc).clone())
    }

    /// Produce the edited copy of this document (see [`apply`]).
    pub fn save(&self, edits: &Edits) -> Result<Written> {
        apply(self.document()?, &self.pages, &self.turns, Some((&self.pdf, &self.fixes)), edits)
    }

    /// How a page is shown, for drawing it (see [`render_pages`]).
    pub fn shown(&self, page: usize) -> Shown {
        Shown { turns: self.turns.get(page).copied().unwrap_or(0), fix: self.fixes.get(page).copied().unwrap_or_default() }
    }

    /// Whether a page can be cropped or have its colours cleaned up: not one
    /// with form fields to fill in, which a picture of it would not have.
    pub fn can_fix(&self, page: usize) -> bool {
        page < self.pages.len() && !self.form.iter().flat_map(|field| &field.widgets).any(|widget| widget.page == page)
    }

    /// Have a page cropped and coloured as `fix` says, in place of what was
    /// done to it before. It stays turned as it is.
    pub fn fix(&mut self, page: usize, fix: Fix) {
        if !self.can_fix(page) || self.fixes[page] == fix {
            return;
        }
        self.fixes[page] = fix;
        let own = &self.own_pages[page];
        let unturned = match fix.crop {
            Some(crop) => picture_geom(crop.width, crop.height),
            None if fix.is_none() => own.clone(),
            None => picture_geom(own.width, own.height),
        };
        self.pages[page] = unturned.turned(self.turns[page]);
    }

    /// Have every page cropped and coloured as `fixes` says (pages it doesn't reach: not at all).
    pub fn set_fixes(&mut self, fixes: &[Fix]) {
        for page in 0..self.pages.len() {
            self.fix(page, fixes.get(page).copied().unwrap_or_default());
        }
    }

    /// Turn a page `quarters` quarter turns clockwise (3 is one the other
    /// way). The form fields on it turn with it.
    pub fn turn(&mut self, page: usize, quarters: u8) {
        let quarters = quarters % 4;
        if quarters == 0 || page >= self.pages.len() {
            return;
        }
        let was = self.pages[page].clone();
        self.pages[page] = was.turned(quarters);
        self.turns[page] = (self.turns[page] + quarters) % 4;
        if self.form.iter().flat_map(|field| &field.widgets).any(|widget| widget.page == page) {
            let now = &self.pages[page];
            for widget in Arc::make_mut(&mut self.form).iter_mut().flat_map(|field| &mut field.widgets) {
                if widget.page == page {
                    widget.follow_page(&was, now);
                }
            }
        }
    }

    /// Have every page turned as `turns` says (pages it doesn't reach: not at all).
    pub fn set_turns(&mut self, turns: &[u8]) {
        for page in 0..self.pages.len() {
            let wanted = turns.get(page).copied().unwrap_or(0) % 4;
            self.turn(page, (wanted + 4 - self.turns[page]) % 4);
        }
    }

    /// This document with pages of others added after its own: for each of
    /// `additions`, those pages (from 0) of that document, in that order. Its
    /// own pages, and its form, stay exactly as they were, so what has been
    /// placed on them or typed into them still applies.
    pub fn with_pages(&self, additions: &[(&LoadedPdf, &[usize])]) -> Result<LoadedPdf> {
        let mut doc = self.document()?;
        let mut added = 0;
        for (source, pages) in additions {
            append_pages(&mut doc, &source.document()?, pages)?;
            added += pages.len();
        }
        let mut merged = load_inner(save_document(&mut doc, None)?, None, true)?;
        if merged.pages.len() != self.pages.len() + added || merged.form.len() != self.form.len() {
            bail!("the pages could not be added to this PDF");
        }
        merged.password = self.password.clone();
        merged.set_fixes(&self.fixes);
        merged.set_turns(&self.turns);
        Ok(merged)
    }
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

/// Open a PDF as it is in the file, for tests to look at what was saved.
#[cfg(test)]
pub fn load(bytes: Vec<u8>) -> Result<LoadedPdf> {
    load_with_password(bytes, None)
}

/// Like [`load`], for a PDF that may need `password`.
#[cfg(test)]
pub fn load_with_password(bytes: Vec<u8>, password: Option<&str>) -> Result<LoadedPdf> {
    load_inner(bytes, password, false)
}

/// Open a PDF for the editor, decrypting it with `password` if it needs one.
/// Fails with a [`PasswordError`] when a password is needed but missing or
/// wrong. The pages render without the form fields that can be filled in, as
/// the editor draws those itself (with the values being typed).
pub fn open_for_editing(bytes: Vec<u8>, password: Option<&str>) -> Result<LoadedPdf> {
    load_inner(bytes, password, true)
}

/// Open a PDF to take pages from (see [`LoadedPdf::with_pages`]), decrypting
/// it with `password` if it needs one.
pub fn open_as_source(bytes: Vec<u8>, password: Option<&str>) -> Result<LoadedPdf> {
    load_inner(bytes, password, false)
}

fn load_inner(bytes: Vec<u8>, password: Option<&str>, hide_fields: bool) -> Result<LoadedPdf> {
    let bytes = Arc::new(bytes);
    // The file as lopdf reads it, once that has been needed.
    let mut parsed: Option<Document> = None;
    let (bytes, pdf, password) = match Pdf::new(bytes.clone()) {
        Ok(pdf) => (bytes, pdf, None),
        Err(LoadPdfError::Decryption(DecryptionError::PasswordProtected)) => {
            let Some(password) = password else { return Err(PasswordError::Required.into()) };
            // Work on a decrypted copy from here on: the renderer and the writer
            // both see the same plain file. (lopdf takes the owner password too.)
            let (doc, plain) = decrypt(&bytes, password)?;
            parsed = Some(doc);
            let plain = Arc::new(plain);
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
        let doc = match parsed.take() {
            Some(doc) => doc,
            None => Document::load_mem(&bytes).context("lopdf could not parse the PDF")?,
        };
        form = forms::read(&doc, &pages);
        if hide_fields && !form.is_empty() {
            // Only what is shown changes; the writer starts from the file as it is.
            let mut shown = doc.clone();
            for widget in form.iter().flat_map(|f| &f.widgets) {
                if let Ok(dict) = shown.get_dictionary_mut(widget.id) {
                    let flags = dict.get(b"F").and_then(Object::as_i64).unwrap_or(0);
                    dict.set("F", flags | forms::ANNOT_HIDDEN);
                }
            }
            let shown = save_document(&mut shown, None)?;
            pdf = Pdf::new(shown).map_err(|e| anyhow!("could not read PDF: {e:?}"))?;
        }
        parsed = Some(doc);
    }
    // One that still has to be decrypted is left for the writer to open (see `open_document`).
    let parsed = parsed.filter(|doc| !doc.is_encrypted());
    Ok(LoadedPdf {
        bytes,
        parsed: Arc::new(parsed.map(OnceLock::from).unwrap_or_default()),
        pdf: Arc::new(pdf),
        turns: vec![0; pages.len()],
        fixes: vec![Fix::default(); pages.len()],
        own_pages: Arc::new(pages.clone()),
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

/// The file decrypted with `password` (the user or the owner password): as a
/// document, and as the bytes of that document.
fn decrypt(bytes: &[u8], password: &str) -> Result<(Document, Vec<u8>)> {
    let mut doc = Document::load_mem_with_options(bytes, lopdf::LoadOptions::with_password(password)).map_err(|e| match e {
        lopdf::Error::InvalidPassword => anyhow::Error::from(PasswordError::Incorrect),
        e => anyhow!("could not decrypt the PDF: {e}"),
    })?;
    // Loading normally decrypts already; this covers files where it couldn't.
    if doc.is_encrypted() {
        doc.decrypt(password).map_err(|e| anyhow!("could not decrypt the PDF: {e}"))?;
    }
    doc.trailer.remove(b"Encrypt");
    doc.encryption_state = None;
    let plain = save_document(&mut doc, None)?;
    Ok((doc, plain))
}

/// Encrypt a finished document with AES-256, with `password` as both the user
/// and the owner password (so it opens with the password it was opened with).
fn encrypt(doc: &mut Document, password: &str) -> Result<()> {
    use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};

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
    if !doc.trailer.has(b"ID") {
        let id = Object::String(id.to_vec(), lopdf::StringFormat::Hexadecimal);
        doc.trailer.set("ID", vec![id.clone(), id]);
    }
    doc.encrypt(&state).map_err(|e| anyhow!("could not encrypt the PDF: {e}"))
}

/// A rendered page bitmap in BGRA order (what GPUI expects).
pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl RenderedPage {
    /// The picture turned `quarters` quarter turns clockwise.
    pub fn turned(self, quarters: u8) -> RenderedPage {
        let (w, h) = (self.width as usize, self.height as usize);
        let quarters = quarters % 4;
        if quarters == 0 || self.bgra.len() != w * h * 4 {
            return self;
        }
        let (width, height) = if quarters == 2 { (w, h) } else { (h, w) };
        let mut bgra = vec![0; self.bgra.len()];
        for (y, row) in bgra.chunks_exact_mut(width * 4).enumerate() {
            for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                // Where this pixel was before the turn.
                let (from_x, from_y) = match quarters {
                    1 => (y, h - 1 - x),
                    2 => (w - 1 - x, h - 1 - y),
                    _ => (w - 1 - y, x),
                };
                let from = (from_y * w + from_x) * 4;
                pixel.copy_from_slice(&self.bgra[from..from + 4]);
            }
        }
        RenderedPage { width: width as u32, height: height as u32, bgra }
    }
}

/// A finished PDF as pictures of its pages: for a printer that is told what
/// goes on the paper dot by dot, as Windows' are.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct Pictures(Pdf);

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl Pictures {
    pub fn of(bytes: Vec<u8>) -> Result<Self> {
        Pdf::new(bytes).map(Self).map_err(|e| anyhow!("could not read the PDF to print: {e:?}"))
    }

    /// The width and height of each page in points, as it is shown.
    pub fn sizes(&self) -> Vec<(f32, f32)> {
        self.0.pages().iter().map(|page| page.render_dimensions()).collect()
    }

    /// A page drawn at `scale` pixels to the point.
    pub fn page(&self, index: usize, scale: f32) -> Option<RenderedPage> {
        render_page(&self.0, index, scale)
    }
}

// (They are drawn on a thread of their own.)
const _: fn() = || {
    fn sent_between_threads<T: Send>() {}
    sent_between_threads::<Pictures>();
};

pub fn render_page(pdf: &Pdf, index: usize, scale: f32) -> Option<RenderedPage> {
    render(pdf, &RenderCache::new(), index, scale, false)
}

/// How a page is shown: what has been done to it (see [`Fix`]), and then the
/// quarter turns clockwise it has been given.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shown {
    pub turns: u8,
    pub fix: Fix,
}

/// Render several pages of a document (index, scale, and how to show it),
/// handing each to `each` as it's done; `each` says whether to go on. What
/// drawing one page works out (the document's fonts, glyph shapes and
/// bitmaps, colour profiles) is kept for the rest of them.
pub fn render_pages(
    pdf: &Pdf,
    pages: impl IntoIterator<Item = (usize, f32, Shown)>,
    mut each: impl FnMut(usize, Option<RenderedPage>) -> bool,
) {
    let cache = RenderCache::new();
    for (index, scale, shown) in pages {
        if !each(index, render_fixed(pdf, &cache, index, scale, &shown.fix).map(|page| page.turned(shown.turns))) {
            break;
        }
    }
}

/// The geometry of a page that is a picture of itself, `width` × `height`
/// points: as it is written by [`fix_pages`].
fn picture_geom(width: f32, height: f32) -> PageGeom {
    PageGeom { width, height, view_to_user: [1., 0., 0., -1., 0., height as f64] }
}

/// Render a page as the file has it, cropped and coloured as `fix` says:
/// `scale` pixels to a point of the page that makes.
fn render_fixed<'a>(pdf: &'a Pdf, cache: &RenderCache<'a>, index: usize, scale: f32, fix: &Fix) -> Option<RenderedPage> {
    let mut page = render(pdf, cache, index, scale, false)?;
    if let Some(crop) = fix.crop {
        let (width, height) = pdf.pages().get(index)?.render_dimensions();
        // The corners in the picture's pixels.
        let (across, down) = (page.width as f32 / width.max(1.), page.height as f32 / height.max(1.));
        let quad = crop.quad.map(|(x, y)| (x * across, y * down));
        let scale = render_scale(crop.width, crop.height, scale);
        let (w, h) = (((crop.width * scale).round() as u32).max(1), ((crop.height * scale).round() as u32).max(1));
        let bgra = scan::straighten(&page.bgra, page.width as usize, page.height as usize, quad, w as usize, h as usize)?;
        page = RenderedPage { width: w, height: h, bgra };
    }
    scan::recolour(&mut page.bgra, page.width as usize, page.height as usize, fix.look);
    Some(page)
}

/// Render the first page of a PDF (one that opens without a password),
/// `width_px` pixels wide.
pub fn render_first_page(bytes: Vec<u8>, width_px: f32) -> Option<RenderedPage> {
    let pdf = Pdf::new(bytes).ok()?;
    let (width, _) = pdf.pages().first()?.render_dimensions();
    render_page(&pdf, 0, width_px / width.max(1.))
}

/// The most pixels a page is drawn with (they take four bytes each), and the
/// most along either side. A page too large for its scale is drawn at a
/// smaller one: a file can declare a page of any size.
pub const MAX_PAGE_PIXELS: f32 = 48e6;
const MAX_PAGE_SIDE: f32 = 32000.;

/// The scale a `width` × `height` point page is drawn at when `scale` is asked for.
pub fn render_scale(width: f32, height: f32, scale: f32) -> f32 {
    let (width, height) = (width.max(1.), height.max(1.));
    scale.min((MAX_PAGE_PIXELS / (width * height)).sqrt()).min(MAX_PAGE_SIDE / width.max(height))
}

fn render<'a>(pdf: &'a Pdf, cache: &RenderCache<'a>, index: usize, scale: f32, transparent: bool) -> Option<RenderedPage> {
    let page = pdf.pages().get(index)?;
    let (width, height) = page.render_dimensions();
    let scale = render_scale(width, height, scale);
    let pixmap = hayro::render(
        page,
        cache,
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
    let state = watermark_state(&mut doc, watermark);
    let ops = watermark_ops(&mut doc, width, height, flip, watermark, state, &mut fonts, &mut resources)?;
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
    render(&pdf, &RenderCache::new(), 0, scale, true)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

const HELV: &str = "PatraPDFHelv";
const HELV_BOLD: &str = "PatraPDFHelvBold";
const WM_GS: &str = "PatraPDFWmGS";
/// The graphics state highlights are drawn with: like ink, they darken what is under them.
const HIGHLIGHT_GS: &str = "PatraPDFHlGS";
const PAGE_IMAGE: &str = "PatraPDFPage";
/// Resolution of redacted pages, which are rebuilt as images: 3 × 72 = 216 dpi.
const REDACT_SCALE: f32 = 3.0;
/// Cropped and recoloured pages are pictures too: at the resolution of the
/// largest picture already on them (a scan is one picture), between that of
/// redacted pages and 300 dpi. In black and white, where every pixel is ink
/// or paper, always 300 dpi.
const SHARPEST_PICTURE: f32 = 300. / 72.;
/// How closely such a picture is kept (JPEG quality, of 100).
const PICTURE_QUALITY: u8 = 85;

/// [`save`] with just pages, items and a watermark, for tests.
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

/// [`apply`] to a file's bytes, for tests (the app saves a [`LoadedPdf`]).
#[cfg(test)]
pub fn save(original: &[u8], source_pages: &[PageGeom], edits: &Edits) -> Result<Written> {
    apply(open_document(original)?, source_pages, &[], None, edits)
}

/// Produce the edited copy of a document: a new PDF with the form filled in,
/// the watermark and all overlay items burned into the page content, and then
/// any redactions applied.
///
/// `source_pages` are the original document's pages as they are shown, which
/// is cropped and coloured as `fixes` says (for each page that has been, with
/// the document as the renderer reads it, to draw them from) and then turned
/// by `turns` (quarter turns clockwise, for each that has been turned);
/// `edits.order` lists, for each page of the output, which original page it
/// is (pages left out are deleted). Item page numbers refer to positions in
/// `order`.
fn apply(mut doc: Document, source_pages: &[PageGeom], turns: &[u8], fixes: Option<(&Pdf, &[Fix])>, edits: &Edits) -> Result<Written> {
    let Edits { order, items, watermark, metrics, password, form, values } = *edits;
    if let Some((pdf, fixes)) = fixes {
        fix_pages(&mut doc, pdf, fixes)?;
    }
    turn_pages(&mut doc, turns)?;
    let identity = order.len() == source_pages.len() && order.iter().enumerate().all(|(i, &p)| i == p);
    let pages: Vec<PageGeom> = if identity {
        source_pages.to_vec()
    } else {
        let pages = order
            .iter()
            .map(|&i| source_pages.get(i).cloned().ok_or_else(|| anyhow!("page {} does not exist", i + 1)))
            .collect::<Result<_>>()?;
        arrange_pages(&mut doc, order)?;
        pages
    };
    let pages = pages.as_slice();
    let watermark = watermark.filter(|w| !w.text.trim().is_empty());
    let filled: Vec<_> = if values.len() == form.len() { forms::changed(form, values).collect() } else { Vec::new() };
    let redactions: Vec<(&Item, RedactFill)> =
        items.iter().filter(|i| i.w >= 1. && i.h >= 1.).filter_map(|i| Some((i, i.redaction()?))).collect();
    let missing = write_overlays(&mut doc, pages, items, watermark, metrics, &filled)?;
    if !redactions.is_empty() {
        // Redacted pages become pictures of themselves as written so far.
        let written = save_document(&mut doc, None)?;
        redact(&mut doc, written, pages, &redactions)?;
    }
    Ok(Written { bytes: save_document(&mut doc, password)?, missing })
}

fn write_overlays(
    doc: &mut Document,
    pages: &[PageGeom],
    items: &[Item],
    watermark: Option<&Watermark>,
    metrics: TextMetrics,
    filled: &[(&Field, &FieldValue)],
) -> Result<BTreeSet<char>> {
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    if page_ids.len() != pages.len() {
        bail!(
            "page count mismatch between renderer ({}) and writer ({})",
            pages.len(),
            page_ids.len()
        );
    }

    let mut fonts = Fonts::default();
    fill_form(doc, filled, metrics, &mut fonts)?;
    let watermark = watermark.map(|w| (w, watermark_state(doc, w)));
    let mut images: HashMap<PathBuf, (ObjectId, String)> = HashMap::new();
    let mut highlight_state: Option<ObjectId> = None;

    for (page_index, (&page_id, geom)) in page_ids.iter().zip(pages).enumerate() {
        let page_items: Vec<&Item> = items.iter().filter(|i| i.page == page_index).collect();
        if page_items.is_empty() && watermark.is_none() {
            continue;
        }

        let v = Affine::new(geom.view_to_user);
        let mut ops = String::new();
        let mut resources = PageResources::default();

        // The watermark goes over the page content but under the filled-in fields.
        if let Some((wm, state)) = watermark {
            if let Some(wm_ops) = watermark_ops(doc, geom.width, geom.height, v, wm, state, &mut fonts, &mut resources) {
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
                        fonts.show_text(doc, line, false, *font_size, (0., -baseline), &mut ops, &mut resources);
                    }
                    ops.push_str("ET Q\n");
                }
                ItemKind::Image { sig } => {
                    let (img_id, name) = match images.get(&sig.path) {
                        Some(entry) => entry.clone(),
                        None => {
                            let id = add_image(doc, &sig.path)?;
                            let entry = (id, format!("PatraPDFIm{}", images.len()));
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
                    let m = v * Affine::translate((item.x as f64, (item.y + item.h) as f64)) * Affine::FLIP_Y;
                    let mark = mark_ops(&forms::Symbol::Check.mark(), (0., 0.), item.w);
                    ops.push_str(&format!("q {} cm 0 G 1 J 1 j\n{mark}Q\n", fmt_m(m)));
                }
                // Applied afterwards by `redact`, on top of everything else.
                ItemKind::Redact { .. } => {}
                // Annotations are drawn in view space (y down), which `v` takes to the page's.
                ItemKind::Highlight { color } => {
                    let state = *highlight_state.get_or_insert_with(|| doc.add_object(dictionary! { "Type" => "ExtGState", "BM" => "Multiply" }));
                    PageResources::add(&mut resources.states, HIGHLIGHT_GS, state);
                    let area = [item.x, item.y, item.w, item.h].map(|n| num(n as f64)).join(" ");
                    ops.push_str(&format!("q {} cm /{HIGHLIGHT_GS} gs {} rg {area} re f Q\n", fmt_m(v), rgb(*color)));
                }
                ItemKind::Outline { oval, color, width } => {
                    // The line's outer edge runs along the box.
                    let inset = (width / 2.).min(item.w / 2.).min(item.h / 2.);
                    let [x, y, w, h] = [item.x + inset, item.y + inset, item.w - 2. * inset, item.h - 2. * inset].map(f64::from);
                    let outline = if *oval { oval_ops(x, y, w, h) } else { format!("{} {} {} {} re", num(x), num(y), num(w), num(h)) };
                    ops.push_str(&format!("q {} cm {} RG {} w {outline} S Q\n", fmt_m(v), rgb(*color), num(*width as f64)));
                }
                ItemKind::Stroke { color, width, arrow, .. } => {
                    let points = item.stroke_points();
                    let [.., from, to] = points.as_slice() else { continue };
                    let trace = |points: &[(f32, f32)]| -> String {
                        let ops = points.iter().enumerate().map(|(i, p)| format!("{} {} {} ", num(p.0 as f64), num(p.1 as f64), if i == 0 { "m" } else { "l" }));
                        ops.collect::<String>() + "S "
                    };
                    let mut lines = trace(&points);
                    if *arrow {
                        let [left, right] = crate::model::arrow_barbs(*from, *to, *width, 1.);
                        lines.push_str(&trace(&[left, *to, right]));
                    }
                    // Round ends and corners, as a pen leaves.
                    ops.push_str(&format!("q {} cm {} RG {} w 1 J 1 j {lines}Q\n", fmt_m(v), rgb(*color), num(*width as f64)));
                }
            }
        }

        if ops.is_empty() {
            continue;
        }
        let merged = resources.merge_into(doc, effective_resources(doc, page_id));
        doc.get_dictionary_mut(page_id)?.set("Resources", merged);
        append_content(doc, page_id, ops.into_bytes())?;
    }

    fonts.finish(doc)
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

    /// Lay out a line of text, noting the characters no font can show.
    fn line<'a>(&mut self, text: &'a str, bold: bool) -> TextLine<'a> {
        let line = TextLine::new(text, bold);
        if let TextLine::Shaped(shaped) = &line {
            self.missing.extend(shaped.missing.iter().copied());
        }
        line
    }

    /// Draw a line (between BT and ET) with its baseline starting at `at`.
    fn show(&mut self, doc: &mut Document, line: &TextLine, size: f32, at: (f32, f32), ops: &mut String, res: &mut PageResources) {
        match line {
            TextLine::Standard { text: "", .. } => {}
            TextLine::Standard { text, bold } => {
                let name = if *bold { HELV_BOLD } else { HELV };
                let font = self.helvetica(doc, *bold);
                PageResources::add(&mut res.fonts, name, font);
                ops.push_str(&format!(
                    "/{name} {} Tf 1 0 0 1 {} {} Tm <{}> Tj\n",
                    num(size as f64),
                    num(at.0 as f64),
                    num(at.1 as f64),
                    winansi_hex(text)
                ));
            }
            TextLine::Shaped(shaped) => {
                let embedder = self.embedder.get_or_insert_with(|| Embedder::new(FontBook::system()));
                embedder.show(doc, shaped, size, at, ops, &mut res.fonts);
            }
        }
    }

    /// Draw one line of text (between BT and ET) with its baseline starting at `at`.
    #[allow(clippy::too_many_arguments)]
    fn show_text(&mut self, doc: &mut Document, text: &str, bold: bool, size: f32, at: (f32, f32), ops: &mut String, res: &mut PageResources) {
        let line = self.line(text, bold);
        self.show(doc, &line, size, at, ops, res);
    }

    /// Write the embedded fonts; returns the characters no font could show.
    fn finish(self, doc: &mut Document) -> Result<BTreeSet<char>> {
        if let Some(embedder) = self.embedder {
            embedder.finish(doc)?;
        }
        Ok(self.missing)
    }
}

/// A line of text as the writer draws it: in standard Helvetica when that can
/// show all of it, otherwise shaped with the installed fonts.
enum TextLine<'a> {
    Standard { text: &'a str, bold: bool },
    Shaped(Arc<fonts::Line>),
}

impl<'a> TextLine<'a> {
    fn new(text: &'a str, bold: bool) -> Self {
        Self::standard(text, bold).unwrap_or_else(|| Self::Shaped(FontBook::system().shape(text, bold)))
    }

    /// The line in standard Helvetica, if that can show all of it.
    fn standard(text: &'a str, bold: bool) -> Option<Self> {
        winansi_encodable(text).then_some(Self::Standard { text, bold })
    }

    /// Advance width of the line, in em.
    fn width(&self) -> f64 {
        match self {
            Self::Standard { text, bold } => {
                let widths = if *bold { &HELVETICA_BOLD_WIDTHS } else { &HELVETICA_WIDTHS };
                text.chars().map(|c| widths[winansi(c) as usize] as u32).sum::<u32>() as f64 / 1000.
            }
            Self::Shaped(shaped) => shaped.width as f64,
        }
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

/// The bytes of a finished document, encrypted with `password` if there is one.
fn save_document(doc: &mut Document, password: Option<&str>) -> Result<Vec<u8>> {
    // Streams are compressed before they're encrypted: encrypted data doesn't compress.
    doc.compress();
    if let Some(password) = password {
        encrypt(doc, password)?;
    }
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
fn arrange_pages(doc: &mut Document, order: &[usize]) -> Result<()> {
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
            if let Some(value) = inherited(doc, id, key) {
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
            removed_annots.extend(page_annotations(doc, id));
        }
        strip_form_fields(doc, &removed_annots);
        if let Ok(catalog) = doc.get_dictionary_mut(catalog_id) {
            catalog.remove(b"StructTreeRoot");
            catalog.remove(b"MarkInfo");
        }
        for object in doc.objects.values_mut() {
            clear_references(object, &removed);
        }
        doc.prune_objects();
    }
    Ok(())
}

/// Copy `pages` (from 0) of `source` to the end of `doc`, with everything
/// they're drawn with: fonts, images, annotations.
///
/// What belongs to the source as a whole stays behind: its bookmarks, its
/// tags, and its form. Form fields on the pages come along as they look, but
/// aren't part of `doc`'s form, so they can't be filled in. Links to pages
/// that weren't copied go nowhere.
fn append_pages(doc: &mut Document, source: &Document, pages: &[usize]) -> Result<()> {
    let source_pages: Vec<ObjectId> = source.get_pages().into_values().collect();
    let mut selected = Vec::with_capacity(pages.len());
    for &i in pages {
        let id = *source_pages.get(i).ok_or_else(|| anyhow!("page {} does not exist", i + 1))?;
        if selected.contains(&id) {
            bail!("page {} appears twice", i + 1);
        }
        selected.push(id);
    }
    if selected.is_empty() {
        bail!("no pages to add");
    }
    let catalog_id = doc.trailer.get(b"Root").and_then(Object::as_reference)?;
    let root_id = doc.get_dictionary(catalog_id)?.get(b"Pages").and_then(Object::as_reference)?;
    let page_count = doc.get_pages().len();

    let mut copier = Copier { source, left_behind: source_pages.iter().copied().collect(), ids: HashMap::new(), queue: Vec::new() };
    for &id in &selected {
        copier.ids.insert(id, doc.new_object_id());
        copier.queue.push(id);
    }
    while let Some(id) = copier.queue.pop() {
        let Ok(object) = source.get_object(id) else { continue };
        let mut object = object.clone();
        let is_page = selected.contains(&id);
        if let (true, Object::Dictionary(page)) = (is_page, &mut object) {
            // The new page stands on its own under `doc`'s page tree.
            for key in INHERITABLE {
                if !page.has(key.as_bytes()) {
                    if let Some(value) = inherited(source, id, key) {
                        page.set(key, value);
                    }
                }
            }
            page.remove(b"StructParents");
        }
        // A form field's widget comes as it looks. The field it belongs to
        // stays behind: it holds the values of all its widgets, on other pages too.
        if let Object::Dictionary(annotation) = &mut object {
            if annotation.get(b"Subtype").and_then(Object::as_name).is_ok_and(|subtype| subtype == b"Widget") {
                annotation.remove(b"Parent");
            }
        }
        copier.rewrite(&mut object, doc);
        if let (true, Object::Dictionary(page)) = (is_page, &mut object) {
            page.set("Parent", root_id);
        }
        doc.objects.insert(copier.ids[&id], object);
    }

    let new_kids = selected.iter().map(|id| Object::Reference(copier.ids[id]));
    let kids = match doc.get_dictionary(root_id)?.get(b"Kids")? {
        Object::Reference(id) => doc.get_object_mut(*id)?,
        _ => doc.get_dictionary_mut(root_id)?.get_mut(b"Kids")?,
    };
    kids.as_array_mut()?.extend(new_kids);
    doc.get_dictionary_mut(root_id)?.set("Count", (page_count + selected.len()) as i64);
    if source.version > doc.version {
        doc.version = source.version.clone();
    }
    Ok(())
}

/// Copies objects of one document into another (see [`append_pages`]).
struct Copier<'a> {
    source: &'a Document,
    /// The source's pages: only those already in `ids` are copied.
    left_behind: HashSet<ObjectId>,
    /// The copy's id for each object copied, or about to be.
    ids: HashMap<ObjectId, ObjectId>,
    /// Objects still to copy.
    queue: Vec<ObjectId>,
}

impl Copier<'_> {
    /// Point every reference in `object` at the copy of what it refers to
    /// (queueing that to be copied too), or at nothing if that stays behind.
    fn rewrite(&mut self, object: &mut Object, doc: &mut Document) {
        match object {
            Object::Reference(id) => match self.copy_of(*id, doc) {
                Some(new) => *id = new,
                None => *object = Object::Null,
            },
            Object::Array(items) => items.iter_mut().for_each(|o| self.rewrite(o, doc)),
            Object::Dictionary(dict) => dict.iter_mut().for_each(|(_, o)| self.rewrite(o, doc)),
            Object::Stream(stream) => stream.dict.iter_mut().for_each(|(_, o)| self.rewrite(o, doc)),
            _ => {}
        }
    }

    fn copy_of(&mut self, id: ObjectId, doc: &mut Document) -> Option<ObjectId> {
        if let Some(&new) = self.ids.get(&id) {
            return Some(new);
        }
        // Other pages, the page tree and the catalog would bring the whole file with them.
        let kind = self.source.get_dictionary(id).ok().and_then(|d| d.get(b"Type").and_then(Object::as_name).ok());
        if self.left_behind.contains(&id) || matches!(kind, Some(b"Page" | b"Pages" | b"Catalog")) || self.source.get_object(id).is_err() {
            return None;
        }
        let new = doc.new_object_id();
        self.ids.insert(id, new);
        self.queue.push(id);
        Some(new)
    }
}

/// Turn pages by quarter turns clockwise (`turns[i]` for page `i`): each is
/// shown that much further round than the file had it.
fn turn_pages(doc: &mut Document, turns: &[u8]) -> Result<()> {
    if turns.iter().all(|turns| turns % 4 == 0) {
        return Ok(());
    }
    let ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for (&id, &turns) in ids.iter().zip(turns).filter(|(_, turns)| *turns % 4 != 0) {
        let own = doc.get_dictionary(id)?.get(b"Rotate").ok().cloned();
        let was = own.or_else(|| inherited(doc, id, "Rotate")).and_then(|o| match doc.dereference(&o).ok()?.1 {
            Object::Integer(degrees) => Some(*degrees),
            Object::Real(degrees) => Some(*degrees as i64),
            _ => None,
        });
        let degrees = (was.unwrap_or(0) + 90 * i64::from(turns % 4)).rem_euclid(360);
        doc.get_dictionary_mut(id)?.set("Rotate", degrees);
    }
    Ok(())
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
pub(crate) fn page_annotations(doc: &Document, page_id: ObjectId) -> Vec<ObjectId> {
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

/// A number as written in content streams: up to four decimals.
pub(crate) fn num(v: f64) -> String {
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

/// The transparency state for the watermark, as a document object.
fn watermark_state(doc: &mut Document, watermark: &Watermark) -> ObjectId {
    let opacity = watermark.opacity.clamp(0., 1.);
    doc.add_object(dictionary! {
        "Type" => "ExtGState",
        "CA" => opacity,
        "ca" => opacity,
    })
}

/// Content-stream operators that draw the watermark on a page of the given
/// displayed size. `v` maps view space to the page's user space, and `state`
/// is the watermark's [`watermark_state`].
#[allow(clippy::too_many_arguments)]
fn watermark_ops(
    doc: &mut Document,
    width: f32,
    height: f32,
    v: Affine,
    watermark: &Watermark,
    state: ObjectId,
    fonts: &mut Fonts,
    res: &mut PageResources,
) -> Option<String> {
    let text = watermark.text.trim();
    let line = fonts.line(text, true);
    let em = line.width();
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
    PageResources::add(&mut res.states, WM_GS, state);
    let mut ops = format!("q /{WM_GS} gs {r} {g} {b} rg {} cm BT\n", fmt_m(m));
    let at = ((-em * size / 2.) as f32, (-CAP_HEIGHT * size / 2.) as f32);
    fonts.show(doc, &line, size as f32, at, &mut ops, res);
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

/// Write new field values into the form: the value itself (so other apps
/// read it and the form stays fillable) and a new appearance showing it.
/// Fields not among `filled` keep their original appearance.
fn fill_form(doc: &mut Document, filled: &[(&Field, &FieldValue)], metrics: TextMetrics, fonts: &mut Fonts) -> Result<()> {
    let mut changed = false;
    for &(field, value) in filled {
        // Fields on deleted pages are gone.
        if doc.get_dictionary(field.id).is_err() {
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
                    // Button widgets always have an "on" state (see `forms::read`).
                    let Some(on) = widget.on_state.clone() else { continue };
                    if !forms::normal_appearances(doc, dict).is_some_and(|normal| normal.has(on.as_bytes())) {
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
        if let Some(form) = acroform_mut(doc) {
            form.remove(b"XFA");
        }
    }
    Ok(())
}

/// The document's form dictionary, whether the catalog holds it or refers to it.
fn acroform_mut(doc: &mut Document) -> Option<&mut Dictionary> {
    let reference = doc.catalog().ok()?.get(b"AcroForm").ok()?.as_reference().ok();
    match reference {
        Some(id) => doc.get_dictionary_mut(id).ok(),
        None => doc.catalog_mut().ok()?.get_mut(b"AcroForm").ok()?.as_dict_mut().ok(),
    }
}

/// Width of `text` at `size`, as the writer will draw it.
pub fn measure_text(text: &str, size: f32) -> f32 {
    TextLine::new(text, false).width() as f32 * size
}

/// Like [`measure_text`], but without waiting for anything: `None` while the
/// text needs the installed fonts and they haven't been found yet.
pub fn measure_text_now(text: &str, size: f32) -> Option<f32> {
    let line = match TextLine::standard(text, false) {
        Some(line) => line,
        None => TextLine::Shaped(FontBook::loaded()?.shape(text, false)),
    };
    Some(line.width() as f32 * size)
}

/// Whether drawing `text` takes the installed fonts (standard Helvetica can't show it).
pub fn needs_installed_fonts(text: &str) -> bool {
    !winansi_encodable(text)
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

/// `0xRRGGBB` as the operands of a colour operator.
fn rgb(value: u32) -> String {
    color([16, 8, 0].map(|shift| ((value >> shift) & 0xff) as f32 / 255.))
}

/// The path of the oval that fills a box, from four Bézier curves.
fn oval_ops(x: f64, y: f64, w: f64, h: f64) -> String {
    let (rx, ry) = (w / 2., h / 2.);
    let (cx, cy, kx, ky) = (x + rx, y + ry, rx * 0.5523, ry * 0.5523);
    let curves = [
        [cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry],
        [cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy],
        [cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry],
        [cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy],
    ];
    let curves: String = curves.iter().map(|curve| curve.map(num).join(" ") + " c ").collect();
    format!("{} {} m {curves}h", num(cx + rx), num(cy))
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
    let (origin, side) = widget.mark_box(w, h);
    let paint = color(widget.text_color);
    let mark = mark_ops(&widget.symbol.mark(), origin, side);
    let on = Stream::new(dict.clone(), format!("{frame}q {paint} rg {paint} RG 1 J 1 j\n{mark}Q\n").into_bytes());
    let off = Stream::new(dict, frame.into_bytes());
    (on, off)
}

/// Operators that draw a check mark (or the like) in the square of side `d`
/// whose bottom-left corner is at (x0, y0), in the current colours.
fn mark_ops(mark: &forms::Mark, (x0, y0): (f32, f32), d: f32) -> String {
    let p = |(x, y): (f32, f32)| format!("{} {}", num((x0 + x * d) as f64), num((y0 + y * d) as f64));
    let trace = |points: &[(f32, f32)]| -> String {
        points.iter().enumerate().map(|(i, &point)| format!("{} {}\n", p(point), if i == 0 { "m" } else { "l" })).collect()
    };
    match mark {
        forms::Mark::Lines(lines) => {
            let strokes: String = lines.iter().map(|line| trace(line) + "S\n").collect();
            format!("{} w\n{strokes}", num((d * forms::MARK_STROKE) as f64))
        }
        forms::Mark::Polygon(points) => trace(points) + "h f\n",
        forms::Mark::Disc(r) => {
            let (cx, cy, r) = ((x0 + d / 2.) as f64, (y0 + d / 2.) as f64, (r * d) as f64);
            oval_ops(cx - r, cy - r, 2. * r, 2. * r) + " f\n"
        }
    }
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
fn redact(doc: &mut Document, written: Vec<u8>, pages: &[PageGeom], redactions: &[(&Item, RedactFill)]) -> Result<()> {
    let pdf = Pdf::new(written).map_err(|e| anyhow!("could not re-read PDF for redaction: {e:?}"))?;
    let cache = RenderCache::new();
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let redacted_pages: BTreeSet<usize> = redactions.iter().map(|(r, _)| r.page).filter(|&p| p < pages.len()).collect();
    let mut removed_annots: HashSet<ObjectId> = HashSet::new();

    for page_index in redacted_pages {
        let geom = &pages[page_index];
        let page_id = page_ids[page_index];
        let rendered = render(&pdf, &cache, page_index, REDACT_SCALE, false)
            .ok_or_else(|| anyhow!("could not render page {} for redaction", page_index + 1))?;
        let (pw, ph) = (rendered.width as usize, rendered.height as usize);
        let mut rgb: Vec<u8> = rendered.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();

        // Burn the boxes in, rounding outwards so no partly covered pixel survives.
        let (sx, sy) = (pw as f32 / geom.width, ph as f32 / geom.height);
        for (r, fill) in redactions.iter().filter(|(r, _)| r.page == page_index) {
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
        removed_annots.extend(show_only_picture(doc, page_id, image_id, m)?);
    }

    strip_document_text(doc, &removed_annots);
    doc.prune_objects();
    Ok(())
}

/// Have a page show one picture (placed by `m`, which takes the unit square
/// to where it goes on the page) and nothing else: its own content and
/// whatever else could carry that are taken off it. Returns the annotations
/// it had, which go with them.
fn show_only_picture(doc: &mut Document, page_id: ObjectId, image_id: ObjectId, m: Affine) -> Result<Vec<ObjectId>> {
    let mut content = Stream::new(dictionary! {}, format!("q {} cm /{PAGE_IMAGE} Do Q\n", fmt_m(m)).into_bytes());
    let _ = content.compress();
    let content_id = doc.add_object(content);
    let annotations = page_annotations(doc, page_id);
    let page = doc.get_dictionary_mut(page_id)?;
    // Anything else on the page that could carry the original content.
    for key in ["Annots", "Thumb", "Metadata", "PieceInfo", "StructParents", "Group", "AA", "B"] {
        page.remove(key.as_bytes());
    }
    page.set("Contents", content_id);
    // An explicit dictionary also stops resources being inherited from the page tree.
    page.set("Resources", dictionary! { "XObject" => dictionary! { PAGE_IMAGE => image_id } });
    Ok(annotations)
}

// ---------------------------------------------------------------------------
// Cropped and recoloured pages
// ---------------------------------------------------------------------------

/// Rebuild each page that was cropped or had its colours cleaned up (see
/// [`Fix`]) as a picture of itself: just what is left of it, as it is shown
/// (before it is turned). Like a redacted page, it keeps nothing of what it
/// was drawn with, so what was cropped off it is not in the file any more.
///
/// `pdf` is the document as the renderer reads it; `fixes[i]` is for page `i`.
fn fix_pages(doc: &mut Document, pdf: &Pdf, fixes: &[Fix]) -> Result<()> {
    if fixes.iter().all(Fix::is_none) {
        return Ok(());
    }
    let cache = RenderCache::new();
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let mut removed_annots: HashSet<ObjectId> = HashSet::new();
    for (index, (&page_id, fix)) in page_ids.iter().zip(fixes).enumerate().filter(|(_, (_, fix))| !fix.is_none()) {
        let own = pdf.pages().get(index).map(|page| page.render_dimensions()).ok_or_else(|| anyhow!("page {} does not exist", index + 1))?;
        let (width, height) = fix.crop.map_or(own, |crop| (crop.width, crop.height));
        let scale = if fix.look == Look::Mono { SHARPEST_PICTURE } else { picture_scale(doc, page_id, own) };
        let rendered =
            render_fixed(pdf, &cache, index, scale, fix).ok_or_else(|| anyhow!("could not draw page {} to crop or recolour it", index + 1))?;
        let image_id = doc.add_object(picture(&rendered, fix.look)?);
        let m = Affine::scale_non_uniform(width as f64, height as f64);
        removed_annots.extend(show_only_picture(doc, page_id, image_id, m)?);
        // The page is just as large as the picture, the right way up. (Set
        // outright, as a page inherits these from the page tree otherwise.)
        let page = doc.get_dictionary_mut(page_id)?;
        for key in ["BleedBox", "TrimBox", "ArtBox"] {
            page.remove(key.as_bytes());
        }
        let area: Vec<Object> = vec![0.into(), 0.into(), width.into(), height.into()];
        page.set("MediaBox", area.clone());
        page.set("CropBox", area);
        page.set("Rotate", 0);
    }
    strip_document_text(doc, &removed_annots);
    doc.prune_objects();
    Ok(())
}

/// How many pixels to a point a page is drawn with to be kept as a picture:
/// as many as the largest picture on it has, if it filled the page (`width`
/// × `height` points), within what a picture of a page is ever kept at.
fn picture_scale(doc: &Document, page_id: ObjectId, (width, height): (f32, f32)) -> f32 {
    let resources = effective_resources(doc, page_id);
    let pictures = resources.get(b"XObject").ok().and_then(|pictures| resolve_dict(doc, pictures));
    let pixels = |object: &Object| {
        let stream = doc.dereference(object).ok()?.1.as_stream().ok()?;
        stream.dict.get(b"Subtype").and_then(Object::as_name).ok().filter(|kind| *kind == b"Image")?;
        let side = |key: &[u8]| stream.dict.get(key).and_then(Object::as_i64).ok();
        Some(side(b"Width")? as f32 * side(b"Height")? as f32)
    };
    let most = pictures.iter().flat_map(|pictures| pictures.iter()).filter_map(|(_, object)| pixels(object)).fold(0., f32::max);
    (most / (width * height).max(1.)).sqrt().clamp(REDACT_SCALE, SHARPEST_PICTURE)
}

/// A drawn page as a picture in the file: a JPEG in colour or in grays, or,
/// in black and white, one bit a pixel.
fn picture(page: &RenderedPage, look: Look) -> Result<Stream> {
    let (width, height) = (page.width as usize, page.height as usize);
    let mut about = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => width as i64,
        "Height" => height as i64,
        "ColorSpace" => if look == Look::Original || look == Look::Enhanced { "DeviceRGB" } else { "DeviceGray" },
        "BitsPerComponent" => if look == Look::Mono { 1 } else { 8 },
    };
    if look == Look::Mono {
        // Each row starts a new byte; a set bit is paper.
        let across = width.div_ceil(8);
        let mut bits = vec![0u8; across * height];
        for (y, row) in page.bgra.chunks_exact(width * 4).enumerate() {
            for (x, pixel) in row.chunks_exact(4).enumerate() {
                bits[y * across + x / 8] |= u8::from(pixel[0] >= 128) << (7 - x % 8);
            }
        }
        let mut stream = Stream::new(about, bits);
        let _ = stream.compress();
        return Ok(stream);
    }
    let (pixels, kind): (Vec<u8>, _) = match look {
        Look::Gray => (page.bgra.chunks_exact(4).map(|p| p[0]).collect(), image::ExtendedColorType::L8),
        _ => (page.bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect(), image::ExtendedColorType::Rgb8),
    };
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, PICTURE_QUALITY)
        .encode(&pixels, page.width, page.height, kind)
        .context("could not keep a page as a picture")?;
    about.set("Filter", "DCTDecode");
    Ok(Stream::new(about, jpeg))
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

/// Drop form fields that are (or whose widgets all are) among `removed_annots`,
/// and the form's other records of what fields hold: the XFA form (a second
/// copy of every value) and the calculation order (which refers to fields).
fn strip_form_fields(doc: &mut Document, removed_annots: &HashSet<ObjectId>) {
    let Some(form) = acroform_mut(doc) else { return };
    form.remove(b"XFA");
    form.remove(b"CO");
    let Some(fields) = form.get(b"Fields").ok().cloned().and_then(|fields| array_of(doc, &fields)) else { return };
    let mut decided = HashMap::new();
    let kept: Vec<Object> = fields.into_iter().filter(|f| keep_field(doc, f, removed_annots, &mut decided, 0)).collect();
    if let Some(form) = acroform_mut(doc) {
        form.set("Fields", kept);
    }
}

/// The array `object` is, or refers to.
fn array_of(doc: &Document, object: &Object) -> Option<Vec<Object>> {
    match doc.dereference(object).ok()?.1 {
        Object::Array(items) => Some(items.clone()),
        _ => None,
    }
}

/// Whether a form field survives redaction: not if it is (or all of its
/// widgets are) on a redacted page. Prunes removed kids from `/Kids`.
///
/// `decided` holds the answer for each field looked at: a damaged (or crafted)
/// file can list one under many parents, and it is looked at once.
fn keep_field(doc: &mut Document, field: &Object, removed: &HashSet<ObjectId>, decided: &mut HashMap<ObjectId, bool>, depth: usize) -> bool {
    let Ok(id) = field.as_reference() else { return true };
    if removed.contains(&id) {
        return false;
    }
    if let Some(&keep) = decided.get(&id) {
        return keep;
    }
    decided.insert(id, true);
    if depth > 32 {
        return true;
    }
    let kids = doc.get_dictionary(id).ok().and_then(|d| d.get(b"Kids").ok().cloned()).and_then(|kids| array_of(doc, &kids));
    let Some(kids) = kids.filter(|kids| !kids.is_empty()) else { return true };
    let kept: Vec<Object> = kids.into_iter().filter(|k| keep_field(doc, k, removed, decided, depth + 1)).collect();
    let keep = !kept.is_empty();
    if let (true, Ok(dict)) = (keep, doc.get_dictionary_mut(id)) {
        dict.set("Kids", kept);
    }
    decided.insert(id, keep);
    keep
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::Signature;

    /// Two pages: a plain Letter page, and a rotated page with an offset crop box.
    #[test]
    fn a_finished_pdf_is_drawn_page_by_page_for_a_printer_that_takes_pictures() {
        let pictures = Pictures::of(sample_pdf()).unwrap();
        let sizes = pictures.sizes();
        assert!(!sizes.is_empty());
        for (index, (width, height)) in sizes.iter().enumerate() {
            let page = pictures.page(index, 2.).expect("the page is drawn");
            assert_eq!((page.width, page.height), ((width * 2.).round() as u32, (height * 2.).round() as u32));
            assert_eq!(page.bgra.len(), (page.width * page.height * 4) as usize);
        }
        assert!(pictures.page(sizes.len(), 1.).is_none());
        assert!(Pictures::of(b"not a pdf".to_vec()).is_err());
    }

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

        let out_dir = std::env::var("PATRAPDF_TEST_OUT").ok().map(PathBuf::from);

        // A small red/transparent test "signature".
        let sig_path = std::env::temp_dir().join("patrapdf_test_sig.png");
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

    /// The sample with a form laid out the roundabout ways a file may: a
    /// field with a widget on each page (through arrays that are objects of
    /// their own), an XFA copy of its value, and a calculation order.
    fn sample_with_indirect_form() -> Vec<u8> {
        let mut doc = Document::load_mem(&sample_pdf()).unwrap();
        let pages = doc.get_pages();
        let field = doc.new_object_id();
        let mut widgets = Vec::new();
        for page in [pages[&1], pages[&2]] {
            let widget = doc.add_object(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Widget",
                "Parent" => field,
                "Rect" => vec![300.into(), 600.into(), 500.into(), 620.into()],
                "P" => page,
            });
            doc.get_dictionary_mut(page).unwrap().set("Annots", vec![widget.into()]);
            widgets.push(Object::Reference(widget));
        }
        let kids = doc.add_object(Object::Array(widgets));
        let value = Object::string_literal("secret-field-value");
        doc.objects.insert(field, dictionary! { "FT" => "Tx", "T" => Object::string_literal("ssn"), "V" => value, "Kids" => kids }.into());
        let fields = doc.add_object(Object::Array(vec![field.into()]));
        let xfa = doc.add_object(Stream::new(dictionary! {}, b"<data><ssn>secret-xfa-value</ssn></data>".to_vec()));
        let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let form = dictionary! { "Fields" => fields, "XFA" => xfa, "CO" => vec![field.into()] };
        doc.get_dictionary_mut(catalog).unwrap().set("AcroForm", form);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn removed_fields_leave_nothing_behind_however_the_form_lists_them() {
        let bytes = sample_with_indirect_form();
        assert!(all_text(&bytes).contains("secret-field-value") && all_text(&bytes).contains("secret-xfa-value"));
        let loaded = load(bytes.clone()).unwrap();
        let gone = |out: &[u8]| !all_text(out).contains("secret-field-value") && !all_text(out).contains("secret-xfa-value");

        // The field has a widget on each page: redacting one page leaves it its other widget...
        let redaction = |page| Item { id: 1, page, x: 290., y: 160., w: 220., h: 40., kind: ItemKind::Redact { fill: RedactFill::Black } };
        let out = write(&bytes, &loaded.pages, &[0, 1], &[redaction(0)], None, TextMetrics::default()).unwrap();
        assert!(all_text(&out).contains("secret-field-value"), "the field is still on page 2");
        assert!(!all_text(&out).contains("secret-xfa-value"), "but the form's second copy of every value is gone");
        // ...and redacting both, or deleting both pages it is on, removes it.
        let out = write(&bytes, &loaded.pages, &[0, 1], &[redaction(0), redaction(1)], None, TextMetrics::default()).unwrap();
        assert!(gone(&out));
        let other = load(sample_pdf()).unwrap();
        let merged = loaded.with_pages(&[(&other, &[0])]).unwrap();
        let out = write(&merged.bytes, &merged.pages, &[2], &[], None, TextMetrics::default()).unwrap();
        assert!(gone(&out));
        assert_eq!(load(out).unwrap().pages.len(), 1);

        // A page added from this file looks the same, without the field's values behind it.
        let merged = other.with_pages(&[(&loaded, &[1])]).unwrap();
        assert_eq!(merged.pages.len(), 3);
        assert!(!all_text(&merged.bytes).contains("secret-field-value"));
    }

    #[test]
    fn a_form_that_lists_fields_many_times_over_still_opens() {
        // Each field has the next one as both of its kids: followed blindly, that is 2^40 visits.
        let mut doc = Document::load_mem(&sample_pdf()).unwrap();
        let page1 = doc.get_pages()[&1];
        let ids: Vec<ObjectId> = (0..40).map(|_| doc.new_object_id()).collect();
        for (i, &id) in ids.iter().enumerate() {
            let mut field = dictionary! { "T" => Object::string_literal(format!("f{i}")) };
            match ids.get(i + 1) {
                Some(&next) => field.set("Kids", vec![next.into(), next.into()]),
                None => {
                    field.set("FT", "Tx");
                    field.set("Subtype", "Widget");
                    field.set("Rect", vec![300.into(), 600.into(), 500.into(), 620.into()]);
                }
            }
            doc.objects.insert(id, field.into());
        }
        doc.get_dictionary_mut(page1).unwrap().set("Annots", vec![ids[39].into()]);
        let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(catalog).unwrap().set("AcroForm", dictionary! { "Fields" => vec![ids[0].into()] });
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let loaded = load(bytes.clone()).unwrap();
        let redaction = Item { id: 1, page: 0, x: 60., y: 60., w: 300., h: 90., kind: ItemKind::Redact { fill: RedactFill::Black } };
        let out = write(&bytes, &loaded.pages, &[0, 1], &[redaction], None, TextMetrics::default()).unwrap();
        assert_eq!(load(out).unwrap().pages.len(), 2);
    }

    #[test]
    fn a_page_is_never_drawn_with_more_pixels_than_the_limit() {
        assert_eq!(render_scale(612., 792., 6.), 6., "ordinary pages are drawn as asked");
        // A page the file says is 70 metres square, and one 70 metres long.
        let scale = render_scale(200000., 200000., 2.);
        assert!((200000. * scale).powi(2) <= MAX_PAGE_PIXELS * 1.01, "{scale}");
        let mut doc = Document::load_mem(&sample_pdf()).unwrap();
        let page1 = doc.get_pages()[&1];
        doc.get_dictionary_mut(page1).unwrap().set("MediaBox", vec![0.into(), 0.into(), 200000.into(), 2000.into()]);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let loaded = load(bytes.clone()).unwrap();
        let page = render_page(&loaded.pdf, 0, 2.).unwrap();
        assert!(page.width as f32 <= MAX_PAGE_SIDE + 1. && page.width > 30000, "{} x {}", page.width, page.height);
        assert_eq!(page.bgra.len(), (page.width * page.height * 4) as usize);
        // A redaction still lands where it was drawn: the top half goes black, the rest stays white.
        let redaction = Item { id: 1, page: 0, x: 0., y: 0., w: 200000., h: 1000., kind: ItemKind::Redact { fill: RedactFill::Black } };
        let out = write(&bytes, &loaded.pages, &[0, 1], &[redaction], None, TextMetrics::default()).unwrap();
        let page = render_page(&load(out).unwrap().pdf, 0, 1.).unwrap();
        let level = |x: u32, y: u32| page.bgra[((y * page.width + x) * 4) as usize];
        assert_eq!((level(page.width / 2, page.height / 4), level(page.width / 2, page.height * 3 / 4)), (0, 255));
    }

    #[test]
    fn annotations_are_drawn_into_the_page() {
        let bytes = sample_pdf();
        let loaded = load(bytes.clone()).unwrap();
        let stroke = |points: &[[f32; 2]], arrow| ItemKind::Stroke { points: points.into(), color: 0x1e4fd6, width: 4., arrow };
        let items = [
            // Over "Page 1 - Name:", which is set in 24pt from x = 72 on the baseline at y = 92.
            Item { id: 1, page: 0, x: 70., y: 70., w: 180., h: 28., kind: ItemKind::Highlight { color: 0xffe14d } },
            Item { id: 2, page: 0, x: 100., y: 200., w: 200., h: 100., kind: ItemKind::Outline { oval: false, color: 0xe5484d, width: 4. } },
            Item { id: 3, page: 0, x: 100., y: 350., w: 200., h: 100., kind: ItemKind::Outline { oval: true, color: 0x2b9a66, width: 4. } },
            Item { id: 4, page: 0, x: 100., y: 500., w: 200., h: 100., kind: stroke(&[[0., 0.], [0.5, 1.], [1., 0.]], false) },
            Item { id: 5, page: 0, x: 350., y: 500., w: 200., h: 8., kind: stroke(&[[0., 0.5], [1., 0.5]], true) },
        ];
        let out = write(&bytes, &loaded.pages, &[0, 1], &items, None, TextMetrics::default()).unwrap();
        let reloaded = load(out).unwrap();
        let page = render_page(&reloaded.pdf, 0, 1.0).unwrap();
        // Whether the pixel at (x, y) is `0xRRGGBB`, give or take rounding.
        let is = |x: u32, y: u32, color: u32| {
            let i = ((y * page.width + x) * 4) as usize;
            [16, 8, 0].iter().zip([2, 1, 0]).all(|(shift, channel)| page.bgra[i + channel].abs_diff((color >> shift) as u8) <= 2)
        };
        let (white, red, green, blue) = (0xffffff, 0xe5484d, 0x2b9a66, 0x1e4fd6);

        // A highlight colours the paper and leaves the text under it dark, like ink.
        assert!(is(245, 75, 0xffe14d), "the paper takes the highlight's colour");
        assert!((72..200).any(|x| (74..96).any(|y| is(x, y, 0x000000))), "the text under it is still black");
        assert!(is(260, 75, white), "and it ends at its box");

        // Outlines are drawn inside their boxes.
        assert!(is(102, 250, red) && is(298, 250, red) && is(200, 202, red) && is(200, 298, red), "the four sides");
        assert!(is(200, 250, white) && is(98, 250, white), "nothing inside or outside");
        assert!(is(102, 400, green) && is(200, 352, green) && is(298, 400, green), "the oval touches its box's sides");
        assert!(is(104, 354, white), "but not its corners");

        // A stroke runs through its points, in order.
        assert!(is(150, 550, blue) && is(200, 598, blue) && is(250, 550, blue), "down to the middle and up again");
        assert!(is(200, 520, white));
        // An arrow: its line, and the two barbs back from its tip.
        assert!(is(450, 504, blue), "the line");
        assert!(is(541, 499, blue) && is(541, 509, blue), "the barbs");
        assert!(is(520, 512, white));

        // Other pages are left alone, and the file's text is still there to be found.
        let other = render_page(&reloaded.pdf, 1, 1.0).unwrap();
        assert_eq!(other.bgra, render_page(&loaded.pdf, 1, 1.0).unwrap().bgra);
        let mut found = 0;
        crate::search::read_pages(&reloaded.pdf, [(0, 0)], |_, text| {
            found = text.find(&crate::search::fold("page 1 - name")).len();
            true
        });
        assert_eq!(found, 1);
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
        if let Some(dir) = std::env::var_os("PATRAPDF_TEST_OUT").map(PathBuf::from) {
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

        // So does a copy with a redaction, which is written out once more.
        let items = [Item { id: 1, page: 0, x: 60., y: 60., w: 300., h: 90., kind: ItemKind::Redact { fill: RedactFill::Black } }];
        let edits = Edits { order: &[0, 1], items: &items, password: Some("open sesame"), ..Edits::default() };
        let out = save(&loaded.bytes, &loaded.pages, &edits).unwrap().bytes;
        assert!(!String::from_utf8_lossy(&out).contains("Page 2 - Name:"));
        assert_eq!(password_error(load(out.clone())), Some(PasswordError::Required));
        let text = all_text(&load_with_password(out, Some("open sesame")).unwrap().bytes);
        assert!(!text.contains("Page 1 - Name:"), "the redacted page's text is gone");
        assert!(text.contains("Page 2 - Name:"));
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
            "MK" => dictionary! { "BC" => vec![0.into(), 0.into(), 1.into()] }, "BS" => dictionary! { "W" => 2 },
            "P" => page1,
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
        // The border is as wide as the widget says (1 when it doesn't say).
        assert_eq!(name.widgets[0].border_width, 2.);
        assert_eq!(upright.widgets[0].border_width, 0., "no border colour, no border");
    }

    /// How much of two pictures of a page differs by more than the smoothing
    /// of an edge does.
    fn difference(a: &RenderedPage, b: &RenderedPage) -> f32 {
        assert_eq!((a.width, a.height), (b.width, b.height));
        let far = a.bgra.chunks_exact(4).zip(b.bgra.chunks_exact(4)).filter(|(a, b)| a.iter().zip(*b).any(|(a, b)| a.abs_diff(*b) > 96));
        far.count() as f32 / (a.width * a.height) as f32
    }

    #[test]
    fn a_turned_page_is_saved_the_way_it_is_shown() {
        let bytes = sample_pdf();
        let original = load(bytes.clone()).unwrap();
        // (The second page is one the file already shows turned, and cropped.)
        for page in 0..2 {
            for quarters in 1..4u8 {
                let mut loaded = original.clone();
                loaded.turn(page, quarters);
                assert_eq!(loaded.turns[page], quarters);
                let shown = loaded.pages[page].clone();
                let was = &original.pages[page];
                let sides = if quarters == 2 { (was.width, was.height) } else { (was.height, was.width) };
                assert_eq!((shown.width, shown.height), sides);
                let picture = render_page(&original.pdf, page, 1.0).unwrap().turned(quarters);

                // Saved, the page is in the file the way it was shown: its
                // size, where everything on it is, and how it looks.
                let saved = load(loaded.save(&Edits { order: &[0, 1], ..Edits::default() }).unwrap().bytes).unwrap();
                assert_eq!((saved.pages[page].width, saved.pages[page].height), sides);
                for (saved, shown) in saved.pages[page].view_to_user.iter().zip(shown.view_to_user) {
                    assert!((saved - shown).abs() < 1e-6, "page {page} turned {quarters}: {:?} for {shown:?}", saved);
                }
                assert_eq!(saved.pages[1 - page], original.pages[1 - page], "the other page is as it was");
                let d = difference(&render_page(&saved.pdf, page, 1.0).unwrap(), &picture);
                assert!(d < 0.01, "page {page} turned {quarters} looks different saved: {d}");

                // What is put on it goes where it was put, whichever way it is written.
                let (x, y) = (shown.width * 0.5, shown.height * 0.25);
                let boxed = |kind| Item { id: 1, page, x: x - 20., y: y - 10., w: 40., h: 20., kind };
                for kind in [ItemKind::Redact { fill: RedactFill::Black }, ItemKind::Outline { oval: false, color: 0x0000ff, width: 6. }] {
                    let redacted = matches!(kind, ItemKind::Redact { .. });
                    let edits = Edits { order: &[0, 1], items: &[boxed(kind)], ..Edits::default() };
                    let saved = load(loaded.save(&edits).unwrap().bytes).unwrap();
                    let with = render_page(&saved.pdf, page, 1.0).unwrap();
                    let pixel = |x: f32, y: f32| {
                        let i = ((y as u32 * with.width + x as u32) * 4) as usize;
                        [with.bgra[i + 2], with.bgra[i + 1], with.bgra[i]]
                    };
                    if redacted {
                        assert_eq!(pixel(x, y), [0, 0, 0], "page {page} turned {quarters}: the middle of the box");
                        assert_eq!(pixel(x - 17., y - 7.), [0, 0, 0]);
                        assert_eq!(pixel(x + 17., y + 7.), [0, 0, 0]);
                    } else {
                        // The four sides of the rectangle, and nothing in the middle of it.
                        for (dx, dy) in [(-18., 0.), (18., 0.), (0., -8.), (0., 8.)] {
                            assert_eq!(pixel(x + dx, y + dy), [0, 0, 255], "page {page} turned {quarters}: side {dx} {dy}");
                        }
                    }
                    let d = difference(&with, &picture);
                    assert!(d > 0. && d < 0.03, "page {page} turned {quarters}: the rest of the page is as it was ({d})");
                }
            }
        }

        // Four quarter turns is no turn; turned back, the page is as it was.
        let mut loaded = original.clone();
        (0..4).for_each(|_| loaded.turn(1, 1));
        loaded.turn(0, 3);
        loaded.turn(0, 1);
        assert_eq!(loaded.turns, [0, 0]);
        for (turned, was) in loaded.pages.iter().zip(&original.pages) {
            assert_eq!((turned.width, turned.height), (was.width, was.height));
            turned.view_to_user.iter().zip(was.view_to_user).for_each(|(a, b)| assert!((a - b).abs() < 1e-9));
        }
        loaded.set_turns(&[2]);
        assert_eq!(loaded.turns, [2, 0]);
    }

    #[test]
    fn form_fields_turn_with_their_page() {
        let original = load(sample_form()).unwrap();
        for quarters in 1..4u8 {
            let mut loaded = original.clone();
            (0..loaded.pages.len()).for_each(|page| loaded.turn(page, quarters));
            // As another app (and this one, opening the copy) finds them on the turned pages.
            let saved = load(loaded.save(&Edits { order: &[0, 1], ..Edits::default() }).unwrap().bytes).unwrap();
            assert_eq!(saved.form.len(), loaded.form.len());
            let mut compared = 0;
            for (turned, read) in loaded.form.iter().flat_map(|f| &f.widgets).zip(saved.form.iter().flat_map(|f| &f.widgets)) {
                for (turned, read) in turned.rect.iter().zip(read.rect) {
                    assert!((turned - read).abs() < 1e-3, "turned {quarters}: {:?} for {:?}", turned, read);
                }
                assert_eq!((turned.upright, turned.size, turned.rotation), (read.upright, read.size, read.rotation));
                compared += 1;
            }
            assert!(compared > 4);
        }
        // A quarter turn stands the first page's fields on end, and rights the one on the page that was turned.
        let mut loaded = original.clone();
        (0..2).for_each(|page| loaded.turn(page, 3));
        let (name, _) = value_of(&loaded.form, "name");
        assert!(!name.widgets[0].upright && name.widgets[0].rect[2] < name.widgets[0].rect[3]);
        let (page2, _) = value_of(&loaded.form, "page2");
        assert!(page2.widgets[0].upright && page2.widgets[0].rect[2] > page2.widgets[0].rect[3]);
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

    #[test]
    fn pages_of_another_pdf_can_be_added() {
        let base = load(sample_pdf()).unwrap();
        let other = load(sample_with_form_field()).unwrap();

        // Its second page, then its first, after the document's own two.
        let merged = base.with_pages(&[(&other, &[1, 0])]).unwrap();
        assert_eq!(merged.pages.len(), 4);
        assert_eq!(merged.pages[..2], base.pages[..]);
        // The added pages are as they were: the rotation and crop box one of
        // them inherited from the other file's page tree, and its font.
        assert_eq!(merged.pages[2], other.pages[1]);
        assert_eq!(merged.pages[3], other.pages[0]);
        let page = render_page(&merged.pdf, 2, 1.0).unwrap();
        assert!(page.bgra.chunks_exact(4).filter(|p| p[0] < 100).count() > 200);
        // A field on an added page isn't one of this document's.
        assert!(merged.form.is_empty());

        // Saved like any other: reordered, with something placed on an added page.
        let note = Item { id: 1, page: 0, x: 80., y: 120., w: 60., h: 16., kind: ItemKind::Text { text: "note".into(), font_size: 12. } };
        let edits = Edits { order: &[3, 0, 1], items: &[note], ..Edits::default() };
        let out = merged.save(&edits).unwrap().bytes;
        assert_eq!(load(out.clone()).unwrap().pages.len(), 3);
        assert!(page_content(&out, 1).contains("Page 1 - Name:"));
        assert!(page_content(&out, 1).contains(&winansi_hex("note")));
        assert!(page_content(&out, 2).contains("Page 1 - Name:"));
        assert!(page_content(&out, 3).contains("Page 2 - Name:"));
        assert!(all_text(&out).contains("secret-field-value"), "the added page keeps what its field shows");

        // More can be added to the result, and from several files at once.
        let again = merged.with_pages(&[(&base, &[0]), (&other, &[0, 1])]).unwrap();
        assert_eq!(again.pages.len(), 7);
        assert_eq!(again.pages[..4], merged.pages[..]);

        // A protected document stays protected.
        let mut protected = base.clone();
        protected.password = Some("sesame".into());
        assert_eq!(protected.with_pages(&[(&other, &[0])]).unwrap().password.as_deref(), Some("sesame"));

        assert!(base.with_pages(&[(&other, &[2])]).is_err());
        assert!(base.with_pages(&[(&other, &[0, 0])]).is_err());
        assert!(base.with_pages(&[(&other, &[])]).is_err());
    }

    /// Whether a phrase is in the text of a page.
    fn reads(pdf: &Pdf, page: usize, phrase: &str) -> bool {
        let mut found = false;
        crate::search::read_pages(pdf, [(page, 0)], |_, text| {
            found = !text.find(&crate::search::fold(phrase)).is_empty();
            true
        });
        found
    }

    #[test]
    fn a_cropped_or_recoloured_page_is_saved_as_the_picture_it_is_shown_as() {
        let original = load(sample_pdf()).unwrap();
        let cache = RenderCache::new();
        // (The second page is one the file already shows turned, and cropped.)
        for page in 0..2 {
            let own = original.pages[page].clone();
            // Corners that make no rectangle, about the line of text on the page.
            let (w, h) = (own.width, own.height);
            let quad = [(w * 0.05, h * 0.04), (w * 0.9, h * 0.08), (w * 0.95, h * 0.6), (w * 0.1, h * 0.55)];
            let (width, height) = scan::size_of(&quad);
            let (width, height) = (width.round(), height.round());
            let crop = scan::Crop { quad, width, height };
            for (fix, turns) in [
                (Fix { crop: Some(crop), look: Look::Original }, 0),
                (Fix { crop: Some(crop), look: Look::Enhanced }, 1),
                (Fix { crop: None, look: Look::Gray }, 3),
                (Fix { crop: Some(crop), look: Look::Mono }, 2),
            ] {
                let what = format!("page {page}, {fix:?}, turned {turns}");
                let mut loaded = original.clone();
                loaded.turn(page, turns);
                loaded.fix(page, fix);
                assert_eq!((loaded.fixes[page], loaded.turns[page]), (fix, turns));
                // It is as large as what is kept of it, turned as it was.
                let kept = if fix.crop.is_some() { (width, height) } else { (w, h) };
                let sides = if turns % 2 == 1 { (kept.1, kept.0) } else { kept };
                let shown = loaded.pages[page].clone();
                assert_eq!((shown.width, shown.height), sides, "{what}");
                let picture = render_fixed(&original.pdf, &cache, page, 1.0, &fix).unwrap().turned(turns);
                assert_eq!((picture.width, picture.height), (sides.0.round() as u32, sides.1.round() as u32), "{what}");

                // Saved, the page is in the file the way it was shown: its
                // size, where everything on it is, and how it looks.
                let bytes = loaded.save(&Edits { order: &[0, 1], ..Edits::default() }).unwrap().bytes;
                let saved = load(bytes.clone()).unwrap();
                assert!((saved.pages[page].width - sides.0).abs() < 0.01 && (saved.pages[page].height - sides.1).abs() < 0.01, "{what}");
                for (saved, shown) in saved.pages[page].view_to_user.iter().zip(shown.view_to_user) {
                    assert!((saved - shown).abs() < 0.01, "{what}: {saved:?} for {shown:?}");
                }
                assert_eq!(saved.pages[1 - page], original.pages[1 - page], "the other page is as it was");
                let again = render_page(&saved.pdf, page, sides.0.round() / saved.pages[page].width).unwrap();
                let d = difference(&again, &picture);
                assert!(d < 0.02, "{what} looks different saved: {d}");

                // It is a picture: one image, and none of the text it had.
                // The other page still has its own.
                assert!(reads(&original.pdf, page, "name") && !reads(&saved.pdf, page, "name"), "{what}");
                assert!(reads(&saved.pdf, 1 - page, "name"), "{what}");
                let doc = Document::load_mem(&bytes).unwrap();
                let id = doc.get_pages().into_values().nth(page).unwrap();
                let resources = effective_resources(&doc, id);
                assert_eq!(resources.len(), 1, "{what}");
                let image = doc.get_object(resolve_dict(&doc, resources.get(b"XObject").unwrap()).unwrap().get(PAGE_IMAGE.as_bytes()).unwrap().as_reference().unwrap());
                let about = &image.unwrap().as_stream().unwrap().dict;
                let name = |key: &[u8]| String::from_utf8_lossy(about.get(key).and_then(Object::as_name).unwrap()).into_owned();
                let (space, filter, bits) = match fix.look {
                    Look::Mono => ("DeviceGray", "FlateDecode", 1),
                    Look::Gray => ("DeviceGray", "DCTDecode", 8),
                    _ => ("DeviceRGB", "DCTDecode", 8),
                };
                assert_eq!((name(b"ColorSpace"), name(b"Filter"), about.get(b"BitsPerComponent").and_then(Object::as_i64).unwrap()), (space.to_string(), filter.to_string(), bits), "{what}");
                // As sharp as a redacted page is at least; in black and white, as a fax.
                let sharp = about.get(b"Width").and_then(Object::as_i64).unwrap() as f32 / kept.0;
                assert!((sharp - if fix.look == Look::Mono { SHARPEST_PICTURE } else { REDACT_SCALE }).abs() < 0.02, "{what}: {sharp} pixels a point");

                // What is put on it goes where it was put, over the picture.
                let (x, y) = (shown.width * 0.5, shown.height * 0.75);
                let outline = Item { id: 1, page, x: x - 20., y: y - 10., w: 40., h: 20., kind: ItemKind::Outline { oval: false, color: 0x0000ff, width: 6. } };
                let saved = load(loaded.save(&Edits { order: &[0, 1], items: &[outline], ..Edits::default() }).unwrap().bytes).unwrap();
                let with = render_page(&saved.pdf, page, 1.0).unwrap();
                for (dx, dy) in [(-18., 0.), (18., 0.), (0., -8.), (0., 8.)] {
                    let i = (((y + dy) as u32 * with.width + (x + dx) as u32) * 4) as usize;
                    assert_eq!([with.bgra[i + 2], with.bgra[i + 1], with.bgra[i]], [0, 0, 255], "{what}: side {dx} {dy}");
                }
            }
        }

        // Nothing done to a page: it is saved as it is, text and all.
        let mut loaded = original.clone();
        loaded.fix(0, Fix { crop: None, look: Look::Mono });
        loaded.fix(0, Fix::default());
        assert_eq!(loaded.pages, original.pages);
        let saved = load(loaded.save(&Edits { order: &[0, 1], ..Edits::default() }).unwrap().bytes).unwrap();
        assert!(reads(&saved.pdf, 0, "name"));

        // A page with form fields to fill in stays as it is: a picture of it would have none.
        let mut form = load(sample_form()).unwrap();
        let with_fields = form.form.iter().flat_map(|field| &field.widgets).map(|widget| widget.page).next().unwrap();
        assert!(!form.can_fix(with_fields));
        let before = form.pages.clone();
        form.fix(with_fields, Fix { crop: None, look: Look::Gray });
        assert!(form.fixes.iter().all(Fix::is_none) && form.pages == before);
    }
}

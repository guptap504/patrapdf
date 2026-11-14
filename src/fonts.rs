//! Text in any script, for the PDF writer.
//!
//! Text that the standard Helvetica font can show is written with it (see
//! `pdf.rs`). Everything else comes here: each character gets an installed
//! font that has it (the font the editor shows text in first, then fonts known
//! to cover other scripts, then anything), runs of text are put in visual order
//! (right-to-left scripts) and shaped with rustybuzz (ligatures, joining
//! forms, marks), and only the glyphs used are embedded in the PDF, as CID
//! fonts with a ToUnicode map so the text can still be searched and copied.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow};
use fontdb::{Database, FaceInfo, ID, Style};
use lopdf::{Document, Object, ObjectId, Stream, dictionary};
use rustybuzz::ttf_parser;
use subsetter::GlyphRemapper;
use unicode_bidi::{BidiClass, BidiInfo, bidi_class};

use crate::pdf::num;

/// Font families to try first, in order. The first is the font the editor
/// draws text with (see `model::TEXT_FONT`), so Latin text looks the same.
const PREFERRED: &[&str] = if cfg!(target_os = "macos") {
    &[
        "Helvetica",
        "Helvetica Neue",
        "Lucida Grande",
        "PingFang SC",
        "Hiragino Sans",
        "Hiragino Kaku Gothic ProN",
        "Apple SD Gothic Neo",
        "Geeza Pro",
        "Arial Hebrew",
        "Kohinoor Devanagari",
        "Kohinoor Bangla",
        "Thonburi",
        "Arial Unicode MS",
        "Apple Symbols",
    ]
} else if cfg!(target_os = "windows") {
    &[
        "Arial",
        "Segoe UI",
        "Microsoft YaHei",
        "Yu Gothic",
        "Malgun Gothic",
        "Microsoft JhengHei",
        "Nirmala UI",
        "Leelawadee UI",
        "Ebrima",
        "Segoe UI Symbol",
        "Arial Unicode MS",
    ]
} else {
    &[
        "Liberation Sans",
        "Arimo",
        "DejaVu Sans",
        "Noto Sans",
        "Noto Sans CJK SC",
        "Noto Sans CJK JP",
        "Noto Sans CJK KR",
        "Noto Sans Arabic",
        "Noto Sans Hebrew",
        "Noto Sans Devanagari",
        "Noto Sans Bengali",
        "Noto Sans Thai",
        "Droid Sans Fallback",
        "WenQuanYi Micro Hei",
        "WenQuanYi Zen Hei",
        "FreeSans",
    ]
};

/// The installed fonts usable in PDFs, in order of preference.
pub struct FontBook {
    /// Locked only to look a face up, never while its data is being read.
    db: Mutex<Database>,
    /// Usable faces in order of preference, for regular and bold text.
    order: [Vec<ID>; 2],
    /// Which face each character was given (`None`: no font has it).
    choices: Mutex<HashMap<(char, bool), Option<ID>>>,
    /// Lines laid out before, regular and bold. The editor measures the same
    /// text on every frame, and the writer draws the lines it measured.
    lines: Mutex<[HashMap<String, Arc<Line>>; 2]>,
}

/// The line cache is emptied when it gets to this many lines.
const MAX_CACHED_LINES: usize = 2048;

static SYSTEM: OnceLock<FontBook> = OnceLock::new();

impl FontBook {
    /// The fonts installed on this computer. They are found on first use,
    /// which takes a while: not something to wait for while drawing.
    pub fn system() -> &'static FontBook {
        SYSTEM.get_or_init(|| {
            let mut db = Database::new();
            db.load_system_fonts();
            FontBook::new(db, PREFERRED)
        })
    }

    /// The installed fonts, if they have been found already.
    pub fn loaded() -> Option<&'static FontBook> {
        SYSTEM.get()
    }

    pub fn new(db: Database, preferred: &[&str]) -> Self {
        let usable: Vec<&FaceInfo> = db.faces().filter(|f| usable(&db, f.id)).collect();
        let order = [400u16, 700].map(|weight| {
            let rank = |f: &FaceInfo| (f.style != Style::Normal, f.weight.0.abs_diff(weight));
            let mut ids = Vec::new();
            let mut taken = HashSet::new();
            for family in preferred {
                let mut faces: Vec<&FaceInfo> = usable
                    .iter()
                    .copied()
                    .filter(|f| f.families.iter().any(|(name, _)| name.eq_ignore_ascii_case(family)))
                    .collect();
                faces.sort_by_key(|f| rank(f));
                ids.extend(faces.into_iter().filter(|f| taken.insert(f.id)).map(|f| f.id));
            }
            let mut rest: Vec<&FaceInfo> = usable.iter().copied().filter(|f| !taken.contains(&f.id)).collect();
            rest.sort_by(|a, b| {
                rank(a)
                    .cmp(&rank(b))
                    .then_with(|| family_name(a).cmp(family_name(b)))
                    .then(a.index.cmp(&b.index))
            });
            ids.extend(rest.into_iter().map(|f| f.id));
            ids
        });
        Self {
            db: Mutex::new(db),
            order,
            choices: Mutex::new(HashMap::new()),
            lines: Mutex::default(),
        }
    }

    /// The preferred face that has a glyph for `c`.
    fn choose(&self, c: char, bold: bool) -> Option<ID> {
        let key = (c, bold);
        if let Some(choice) = self.choices.lock().ok().and_then(|m| m.get(&key).copied()) {
            return choice;
        }
        let choice = self.order[bold as usize].iter().copied().find(|&id| self.covers(id, c));
        if let Ok(mut choices) = self.choices.lock() {
            choices.insert(key, choice);
        }
        choice
    }

    fn covers(&self, id: ID, c: char) -> bool {
        let covers = |data: &[u8], index| ttf_parser::Face::parse(data, index).ok().and_then(|f| f.glyph_index(c)).is_some();
        // Faces in use are already mapped; others are only looked into here.
        self.db.lock().ok().and_then(|db| db.with_face_data(id, covers)).unwrap_or(false)
    }

    /// Read a face that text is set in. Its file stays mapped from then on,
    /// rather than being opened and mapped again for every line.
    fn with_face<T>(&self, id: ID, read: impl FnOnce(&[u8], u32) -> T) -> Option<T> {
        // SAFETY: the file is mapped read-only, exactly as fontdb's own
        // `with_face_data` maps it for each call; here the mapping is kept.
        // Either way, installed font files must not be rewritten in place
        // while they are in use.
        let (data, index) = unsafe { self.db.lock().ok()?.make_shared_face_data(id)? };
        Some(read((*data).as_ref(), index))
    }

    /// A face's PostScript and family names.
    fn names(&self, id: ID) -> Option<(String, String)> {
        let db = self.db.lock().ok()?;
        let info = db.face(id)?;
        Some((info.post_script_name.clone(), family_name(info).to_string()))
    }

    /// Lay out one line of text: fonts, visual order and shaping.
    pub fn shape(&self, text: &str, bold: bool) -> Arc<Line> {
        if let Some(line) = self.lines.lock().ok().and_then(|lines| lines[bold as usize].get(text).cloned()) {
            return line;
        }
        let line = Arc::new(self.lay_out(text, bold));
        if let Ok(mut lines) = self.lines.lock() {
            let lines = &mut lines[bold as usize];
            if lines.len() >= MAX_CACHED_LINES {
                lines.clear();
            }
            lines.insert(text.to_owned(), line.clone());
        }
        line
    }

    fn lay_out(&self, text: &str, bold: bool) -> Line {
        let text = text.replace('\t', " ");
        let mut line = Line::default();
        if text.is_empty() {
            return line;
        }
        let bidi = BidiInfo::new(&text, None);
        let mut pen = 0.;
        for paragraph in &bidi.paragraphs {
            let (levels, runs) = bidi.visual_runs(paragraph, paragraph.range.clone());
            for run in runs {
                let rtl = levels[run.start].is_rtl();
                let mut segments = self.segment(&text, run, bold, &mut line.missing);
                // Within a right-to-left run, the first segment is the rightmost.
                if rtl {
                    segments.reverse();
                }
                for (range, face) in segments {
                    pen = self.shape_segment(&text[range], face, rtl, pen, &mut line.glyphs);
                }
            }
        }
        line.width = pen;
        line
    }

    /// Split a run of text into pieces that each use one face.
    fn segment(&self, text: &str, range: Range<usize>, bold: bool, missing: &mut BTreeSet<char>) -> Vec<(Range<usize>, ID)> {
        let mut out: Vec<(Range<usize>, ID)> = Vec::new();
        for (i, c) in text[range.clone()].char_indices() {
            let (start, end) = (range.start + i, range.start + i + c.len_utf8());
            let previous = out.last().map(|(_, face)| *face);
            // Marks and joiners belong with the character before them; spaces stay in
            // the current font (keeping words of a script in one piece) if it has them.
            let attached = matches!(bidi_class(c), BidiClass::NSM)
                || matches!(c, '\u{200c}' | '\u{200d}' | '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}');
            let face = match previous {
                Some(face) if (attached || c.is_whitespace()) && self.covers(face, c) => Some(face),
                _ => self.choose(c, bold),
            };
            let Some(face) = face else {
                missing.insert(c);
                continue;
            };
            match out.last_mut() {
                Some((r, f)) if *f == face && r.end == start => r.end = end,
                _ => out.push((start..end, face)),
            }
        }
        out
    }

    fn shape_segment(&self, text: &str, face: ID, rtl: bool, pen: f32, out: &mut Vec<Glyph>) -> f32 {
        self.with_face(face, |data, index| {
            let Some(font) = rustybuzz::Face::from_slice(data, index) else { return pen };
            let upem = font.units_per_em() as f32;
            let mut buffer = rustybuzz::UnicodeBuffer::new();
            buffer.push_str(text);
            buffer.set_direction(if rtl {
                rustybuzz::Direction::RightToLeft
            } else {
                rustybuzz::Direction::LeftToRight
            });
            let shaped = rustybuzz::shape(&font, &[], buffer);
            // Cluster starts in logical order give each cluster's text.
            let mut starts: Vec<usize> = shaped.glyph_infos().iter().map(|g| g.cluster as usize).collect();
            starts.sort_unstable();
            starts.dedup();
            let cluster_text = |start: usize| {
                let next = starts.iter().find(|&&s| s > start).copied().unwrap_or(text.len());
                text.get(start..next).unwrap_or_default().to_string()
            };
            let mut seen = HashSet::new();
            let mut pen = pen;
            for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                let id = info.glyph_id as u16;
                let cluster = info.cluster as usize;
                out.push(Glyph {
                    face,
                    id,
                    x: pen + pos.x_offset as f32 / upem,
                    y: pos.y_offset as f32 / upem,
                    width: font.glyph_hor_advance(ttf_parser::GlyphId(id)).unwrap_or(0) as f32 / upem,
                    text: if seen.insert(cluster) { cluster_text(cluster) } else { String::new() },
                });
                pen += pos.x_advance as f32 / upem;
            }
            pen
        })
        .unwrap_or(pen)
    }
}

/// Whether a face can go into a PDF: it has outlines (not just bitmaps), and
/// its licence allows embedding a subset.
fn usable(db: &Database, id: ID) -> bool {
    db.with_face_data(id, |data, index| {
        let Ok(face) = ttf_parser::Face::parse(data, index) else { return false };
        let tables = face.tables();
        let outlines = tables.glyf.is_some() || tables.cff.is_some() || tables.cff2.is_some();
        // Colour bitmap fonts (Apple Color Emoji, Noto Color Emoji) only have
        // placeholder outlines, which would come out blank.
        let bitmaps = tables.sbix.is_some() || tables.cbdt.is_some();
        outlines
            && !bitmaps
            && face.is_outline_embedding_allowed()
            && face.is_subsetting_allowed()
            && face.permissions() != Some(ttf_parser::Permissions::Restricted)
    })
    .unwrap_or(false)
}

fn family_name(face: &FaceInfo) -> &str {
    face.families.first().map(|(name, _)| name.as_str()).unwrap_or_default()
}

/// A glyph placed on a line. Distances are in em (multiply by the font size).
#[derive(Clone, Debug)]
pub struct Glyph {
    face: ID,
    id: u16,
    /// Where the glyph is drawn, relative to the start of the line's baseline.
    x: f32,
    y: f32,
    /// The font's own advance for the glyph: how far a PDF viewer moves after it.
    width: f32,
    /// The text the glyph stands for ("" for the second and later glyphs of a cluster).
    text: String,
}

/// A shaped line of text.
#[derive(Clone, Debug, Default)]
pub struct Line {
    glyphs: Vec<Glyph>,
    /// Advance width of the whole line, in em.
    pub width: f32,
    /// Characters no installed font has (they are left out).
    pub missing: BTreeSet<char>,
}

/// Collects the glyphs a document uses and embeds them as font subsets.
pub struct Embedder<'a> {
    book: &'a FontBook,
    fonts: Vec<EmbeddedFont>,
}

struct EmbeddedFont {
    face: ID,
    /// The Type0 font object (written by `finish`).
    object: ObjectId,
    /// Resource name in content streams.
    name: String,
    /// Original glyph ids → the ids (CIDs) in the subset.
    glyphs: GlyphRemapper,
    /// Advance width by CID, in em.
    widths: Vec<f32>,
    /// Text by CID, for the ToUnicode map.
    text: BTreeMap<u16, String>,
}

impl<'a> Embedder<'a> {
    pub fn new(book: &'a FontBook) -> Self {
        Self { book, fonts: Vec::new() }
    }

    /// Append operators that draw `line` at `size` with the start of its
    /// baseline at (x, y) in text space. They belong between `BT` and `ET`;
    /// the fonts they use are added to `resources`.
    pub fn show(
        &mut self,
        doc: &mut Document,
        line: &Line,
        size: f32,
        (x, y): (f32, f32),
        ops: &mut String,
        resources: &mut Vec<(String, ObjectId)>,
    ) {
        let mut current: Option<usize> = None;
        let mut run = String::new();
        let mut next: Option<(f32, f32)> = None;
        let flush = |run: &mut String, ops: &mut String| {
            if !run.is_empty() {
                ops.push_str(&format!("<{run}> Tj\n"));
                run.clear();
            }
        };
        for glyph in &line.glyphs {
            let index = self.font_index(doc, glyph.face);
            let font = &mut self.fonts[index];
            let cid = font.glyphs.remap(glyph.id);
            if font.widths.len() <= cid as usize {
                font.widths.resize(cid as usize + 1, 0.);
            }
            font.widths[cid as usize] = glyph.width;
            if !glyph.text.is_empty() {
                font.text.entry(cid).or_insert_with(|| glyph.text.clone());
            }
            let (gx, gy) = (x + glyph.x * size, y + glyph.y * size);
            // A glyph continues the current string if it sits exactly where the
            // viewer's pen ends up after the previous one.
            let continues = current == Some(index) && next.is_some_and(|(nx, ny)| (nx - gx).abs() < 0.005 && (ny - gy).abs() < 0.005);
            if !continues {
                flush(&mut run, ops);
                if current != Some(index) {
                    ops.push_str(&format!("/{} {} Tf\n", font.name, num(size as f64)));
                    if !resources.iter().any(|(name, _)| *name == font.name) {
                        resources.push((font.name.clone(), font.object));
                    }
                    current = Some(index);
                }
                ops.push_str(&format!("1 0 0 1 {} {} Tm\n", num(gx as f64), num(gy as f64)));
            }
            run.push_str(&format!("{cid:04X}"));
            next = Some((gx + glyph.width * size, gy));
        }
        flush(&mut run, ops);
    }

    fn font_index(&mut self, doc: &mut Document, face: ID) -> usize {
        if let Some(i) = self.fonts.iter().position(|f| f.face == face) {
            return i;
        }
        self.fonts.push(EmbeddedFont {
            face,
            object: doc.new_object_id(),
            name: format!("PatraPDFF{}", self.fonts.len()),
            glyphs: GlyphRemapper::new(),
            widths: Vec::new(),
            text: BTreeMap::new(),
        });
        self.fonts.len() - 1
    }

    /// Write the fonts used so far into the document.
    pub fn finish(self, doc: &mut Document) -> Result<()> {
        for font in self.fonts {
            let (postscript, family) = self.book.names(font.face).ok_or_else(|| anyhow!("font disappeared"))?;
            let postscript = sanitize_name(&postscript);
            let built = self.book.with_face(font.face, |data, index| -> Result<_> {
                let face = ttf_parser::Face::parse(data, index).map_err(|e| anyhow!("could not read font {family}: {e}"))?;
                let subset = subsetter::subset_with_variations(data, index, &[], &font.glyphs)
                    .map_err(|e| anyhow!("could not embed font {family}: {e}"))?;
                Ok((Metrics::of(&face), subset))
            });
            let (metrics, subset) = built.ok_or_else(|| anyhow!("could not read font {family}"))??;
            write_font(doc, &font, &postscript, &metrics, subset)?;
        }
        Ok(())
    }
}

/// Font-wide numbers for the font descriptor, in 1/1000 em.
struct Metrics {
    bbox: [f32; 4],
    ascent: f32,
    descent: f32,
    cap_height: f32,
    italic_angle: f32,
    italic: bool,
    fixed_pitch: bool,
    weight: u16,
}

impl Metrics {
    fn of(face: &ttf_parser::Face) -> Self {
        let scale = 1000. / face.units_per_em() as f32;
        let b = face.global_bounding_box();
        Self {
            bbox: [b.x_min, b.y_min, b.x_max, b.y_max].map(|v| v as f32 * scale),
            ascent: face.ascender() as f32 * scale,
            descent: face.descender() as f32 * scale,
            cap_height: face.capital_height().unwrap_or(face.ascender()) as f32 * scale,
            italic_angle: face.italic_angle(),
            italic: face.is_italic(),
            fixed_pitch: face.is_monospaced(),
            weight: face.weight().to_number(),
        }
    }
}

fn write_font(doc: &mut Document, font: &EmbeddedFont, postscript: &str, m: &Metrics, subset: Vec<u8>) -> Result<()> {
    let cff = ttf_parser::RawFace::parse(&subset, 0)
        .ok()
        .and_then(|raw| raw.table(ttf_parser::Tag::from_bytes(b"CFF ")))
        .map(<[u8]>::to_vec);
    let (file_key, mut file) = match &cff {
        // A bare CFF program is the most widely supported way to embed CFF outlines.
        Some(cff) => ("FontFile3", Stream::new(dictionary! { "Subtype" => "CIDFontType0C" }, cff.clone())),
        None => ("FontFile2", Stream::new(dictionary! { "Length1" => subset.len() as i64 }, subset)),
    };
    let _ = file.compress();
    let file = doc.add_object(file);

    // Subset fonts are named with a tag unique to the glyphs they hold.
    let mut hasher = DefaultHasher::new();
    (postscript, font.glyphs.remapped_gids().collect::<Vec<_>>()).hash(&mut hasher);
    let mut hash = hasher.finish();
    let tag: String = (0..6)
        .map(|_| {
            let c = (b'A' + (hash % 26) as u8) as char;
            hash /= 26;
            c
        })
        .collect();
    let base_font = Object::Name(format!("{tag}+{postscript}").into_bytes());

    let flags = 4 | if m.fixed_pitch { 1 } else { 0 } | if m.italic { 64 } else { 0 };
    let descriptor = doc.add_object(dictionary! {
        "Type" => "FontDescriptor",
        "FontName" => base_font.clone(),
        "Flags" => flags,
        "FontBBox" => m.bbox.iter().map(|&v| Object::Real(v.round())).collect::<Vec<_>>(),
        "ItalicAngle" => m.italic_angle,
        "Ascent" => m.ascent.round(),
        "Descent" => m.descent.round(),
        "CapHeight" => m.cap_height.round(),
        "StemV" => (10. + 220. * (m.weight as f32 - 50.) / 900.).round().max(10.),
        file_key => file,
    });

    let widths: Vec<Object> = font.widths.iter().map(|&w| Object::Real((w * 1000.).round())).collect();
    let mut cid_font = dictionary! {
        "Type" => "Font",
        "Subtype" => if cff.is_some() { "CIDFontType0" } else { "CIDFontType2" },
        "BaseFont" => base_font.clone(),
        "CIDSystemInfo" => dictionary! {
            "Registry" => Object::string_literal("Adobe"),
            "Ordering" => Object::string_literal("Identity"),
            "Supplement" => 0,
        },
        "FontDescriptor" => descriptor,
        "W" => vec![Object::Integer(0), Object::Array(widths)],
    };
    if cff.is_none() {
        cid_font.set("CIDToGIDMap", "Identity");
    }
    let cid_font = doc.add_object(cid_font);

    let mut to_unicode = Stream::new(dictionary! {}, to_unicode_cmap(&font.text).into_bytes());
    let _ = to_unicode.compress();
    let to_unicode = doc.add_object(to_unicode);

    doc.objects.insert(
        font.object,
        Object::Dictionary(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => base_font,
            "Encoding" => "Identity-H",
            "DescendantFonts" => vec![cid_font.into()],
            "ToUnicode" => to_unicode,
        }),
    );
    Ok(())
}

/// A CMap from CIDs to the text they stand for (UTF-16BE).
fn to_unicode_cmap(text: &BTreeMap<u16, String>) -> String {
    let mut out = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let entries: Vec<(&u16, &String)> = text.iter().collect();
    for chunk in entries.chunks(100) {
        out.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (cid, text) in chunk {
            let utf16: String = text.encode_utf16().map(|u| format!("{u:04X}")).collect();
            out.push_str(&format!("<{cid:04X}> <{utf16}>\n"));
        }
        out.push_str("endbfchar\n");
    }
    out.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    out
}

/// A PostScript name usable as a PDF name.
fn sanitize_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).take(60).collect();
    if clean.is_empty() { "Font".into() } else { clean }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The system's fonts, or `None` (skipping the test) when none of them has
    /// all of `text` — CI machines don't have fonts for every script.
    fn book_with(text: &str) -> Option<&'static FontBook> {
        let book = FontBook::system();
        let line = book.shape(text, false);
        if line.missing.is_empty() {
            Some(book)
        } else {
            eprintln!("skipping: no installed font has {:?}", line.missing);
            None
        }
    }

    #[test]
    fn right_to_left_text_is_laid_out_in_visual_order() {
        let Some(book) = book_with("abc שלום") else { return };
        let line = book.shape("abc שלום", false);
        // Logical order: a b c space ש ל ו ם. Visually the Hebrew word reads right
        // to left, so its last letter (ם) is drawn first, right after the space.
        let texts: Vec<&str> = line.glyphs.iter().map(|g| g.text.as_str()).collect();
        assert_eq!(&texts[..3], ["a", "b", "c"]);
        assert_eq!(texts.last(), Some(&"ש"), "{texts:?}");
        assert!(line.glyphs.windows(2).all(|w| w[0].x <= w[1].x + 1e-4), "glyphs advance left to right");
        assert!(line.width > 0.);
    }

    #[test]
    fn arabic_letters_are_joined() {
        let Some(book) = book_with("سلام") else { return };
        let line = book.shape("سلام", false);
        // Shaping picks contextual forms: the ids differ from the isolated letters'.
        let isolated: Vec<u16> = "سلام".chars().flat_map(|c| book.shape(&c.to_string(), false).glyphs.clone()).map(|g| g.id).collect();
        let joined: Vec<u16> = line.glyphs.iter().map(|g| g.id).collect();
        assert_ne!(joined, isolated);
    }

    #[test]
    fn missing_characters_are_reported() {
        let book = FontBook::new(Database::new(), &[]);
        let line = book.shape("hi", false);
        assert!(line.glyphs.is_empty());
        assert_eq!(line.missing, BTreeSet::from(['h', 'i']));
    }

    #[test]
    fn to_unicode_maps_cids_to_utf16() {
        let map = BTreeMap::from([(1u16, "A".to_string()), (2, "😀".to_string()), (3, "ﬁ".to_string())]);
        let cmap = to_unicode_cmap(&map);
        assert!(cmap.contains("<0001> <0041>"));
        assert!(cmap.contains("<0002> <D83DDE00>"), "surrogate pairs");
        assert!(cmap.contains("3 beginbfchar"));
    }
}

//! Finding text in a document: each page's characters with where they are
//! drawn (read with hayro's interpreter, which the renderer uses too), and
//! the places a phrase occurs among them.

use hayro::hayro_interpret::font::{Glyph, GlyphRun};
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterCache, InterpreterSettings, SoftMask,
    TransformExt, interpret_page,
};
use hayro::hayro_syntax::Pdf;
use hayro::kurbo::{BezPath, Point, Rect};

/// A box on a page: `[x, y, width, height]` in view space (points, from the top left).
pub type Area = [f32; 4];

/// How far a character reaches above and below its baseline, in thousandths
/// of the font size. Fonts differ; this is the box a highlight is drawn with.
const ASCENT: f64 = 780.;
const DESCENT: f64 = -220.;
/// Characters further apart than this share of the font size are separate words.
const WORD_GAP: f64 = 0.2;
/// A box larger than this (in points) is a mistake, not a character.
const MAX_SIZE: f64 = 5000.;

/// The text of one page.
#[derive(Default)]
pub struct PageText {
    /// Its characters in the order they are drawn, in lower case, with one
    /// space wherever words or lines are apart.
    chars: Vec<char>,
    /// Where each is drawn (`None` for the spaces in between).
    areas: Vec<Option<Area>>,
}

/// A phrase as it is looked for: in lower case, with single spaces.
pub fn fold(phrase: &str) -> Vec<char> {
    phrase.split_whitespace().collect::<Vec<_>>().join(" ").chars().flat_map(char::to_lowercase).collect()
}

impl PageText {
    /// Every place `phrase` (see [`fold`]) occurs: the boxes that cover it, one per line it runs over.
    pub fn find(&self, phrase: &[char]) -> Vec<Vec<Area>> {
        if phrase.is_empty() {
            return Vec::new();
        }
        let mut found = Vec::new();
        let mut start = 0;
        while start + phrase.len() <= self.chars.len() {
            if self.chars[start..start + phrase.len()] == *phrase {
                let areas = lines(self.areas[start..start + phrase.len()].iter().flatten());
                if !areas.is_empty() {
                    found.push(areas);
                }
                // Occurrences don't overlap.
                start += phrase.len();
            } else {
                start += 1;
            }
        }
        found
    }

    fn push(&mut self, c: char, area: Option<Area>) {
        if c.is_whitespace() || c.is_control() {
            self.space();
            return;
        }
        for c in c.to_lowercase() {
            self.chars.push(c);
            self.areas.push(area);
        }
    }

    fn space(&mut self) {
        if self.chars.last().is_some_and(|c| *c != ' ') {
            self.chars.push(' ');
            self.areas.push(None);
        }
    }
}

/// Join the boxes of consecutive characters into one box per line.
fn lines<'a>(areas: impl Iterator<Item = &'a Area>) -> Vec<Area> {
    let mut lines: Vec<Area> = Vec::new();
    for &[x, y, w, h] in areas {
        match lines.last_mut() {
            // On the same line as the box before: they overlap in height by half or more.
            Some(line) if (line[1] + line[3]).min(y + h) - line[1].max(y) > 0.5 * line[3].min(h) => {
                let (right, bottom) = ((line[0] + line[2]).max(x + w), (line[1] + line[3]).max(y + h));
                (line[0], line[1]) = (line[0].min(x), line[1].min(y));
                (line[2], line[3]) = (right - line[0], bottom - line[1]);
            }
            _ => lines.push([x, y, w, h]),
        }
    }
    lines
}

/// Read the text of the given pages one after another, handing each to `each`
/// as it's done; `each` says whether to go on. The fonts one page loads are
/// kept for the rest.
pub fn read_pages(pdf: &Pdf, pages: impl IntoIterator<Item = usize>, mut each: impl FnMut(usize, PageText) -> bool) {
    let cache = InterpreterCache::new();
    for index in pages {
        if !each(index, read_page(pdf, &cache, index)) {
            break;
        }
    }
}

fn read_page<'a>(pdf: &'a Pdf, cache: &InterpreterCache<'a>, index: usize) -> PageText {
    let mut reader = Reader::default();
    if let Some(page) = pdf.pages().get(index) {
        let (width, height) = page.render_dimensions();
        // The same space the page is drawn in at 100%: view space.
        let mut context = Context::new(
            page.initial_transform(true).to_kurbo(),
            Rect::new(0., 0., width as f64, height as f64),
            cache,
            page.xref(),
            InterpreterSettings::default(),
        );
        interpret_page(page, &mut context, &mut reader);
    }
    reader.text
}

/// A "device" that draws nothing and notes the characters it is asked to draw.
#[derive(Default)]
struct Reader {
    text: PageText,
    /// Where the last character ended, and its font size (view space).
    last: Option<(Point, f64)>,
}

impl<'a> Device<'a> for Reader {
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        for glyph in run.glyphs() {
            let Some(unicode) = glyph.as_unicode() else { continue };
            let advance = match &**glyph {
                Glyph::Outline(glyph) => glyph.advance_width(),
                Glyph::Type3(glyph) => glyph.advance_width(),
            };
            let advance = advance.map_or(500., f64::from);
            // From the glyph's own space (thousandths of the font size) to the page.
            let to_view = props.transform * glyph.transform();
            let (start, end) = (to_view * Point::new(0., 0.), to_view * Point::new(advance, 0.));
            let size = start.distance(to_view * Point::new(0., 1000.));
            if self.last.is_some_and(|(last, last_size)| last.distance(start) > WORD_GAP * size.max(last_size)) {
                self.text.space();
            }
            self.last = Some((end, size));

            let cell = to_view.transform_rect_bbox(Rect::new(0., DESCENT, advance, ASCENT));
            let sane = cell.is_finite() && cell.width() < MAX_SIZE && cell.height() < MAX_SIZE;
            let area = sane.then(|| [cell.x0 as f32, cell.y0 as f32, cell.width() as f32, cell.height() as f32]);
            match unicode {
                BfString::Char(c) => self.text.push(c, area),
                // A ligature: its letters share the one box.
                BfString::String(s) => s.chars().for_each(|c| self.text.push(c, area)),
            }
        }
    }

    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Pdf {
        Pdf::new(crate::pdf::tests::sample_pdf()).unwrap_or_else(|_| panic!("the sample doesn't open"))
    }

    #[test]
    fn text_is_read_with_where_it_is() {
        let pdf = sample();
        let mut pages = Vec::new();
        read_pages(&pdf, 0..2, |index, text| {
            pages.push((index, text));
            true
        });
        assert_eq!(pages.len(), 2);
        let text: String = pages[1].1.chars.iter().collect();
        assert_eq!(text, "page 2 - name:", "lower case, in reading order");

        // The line is set in 24pt Times from x = 72, on the baseline 700pt up a 792pt page.
        let found = pages[0].1.find(&fold("NAME"));
        let [x, y, w, h] = match found.as_slice() {
            [lines] if lines.len() == 1 => lines[0],
            _ => panic!("one occurrence on one line, not {found:?}"),
        };
        let before = 24. * (0.556 + 0.444 + 0.5 + 0.444 + 0.25 + 0.5 + 0.25 + 0.333 + 0.25) as f32;
        assert!((x - (72. + before)).abs() < 0.5, "starts after \"Page 1 - \": {x}");
        assert!((w - 24. * (0.722 + 0.444 + 0.778 + 0.444)).abs() < 0.5, "as wide as the word: {w}");
        assert!(y < 792. - 700. - 12. && y + h > 792. - 700., "around the baseline: {y} {h}");
    }

    #[test]
    fn phrases_match_whatever_their_case_and_spacing() {
        let text = &read_page(&sample(), &InterpreterCache::new(), 0);
        assert_eq!(text.find(&fold("page   1")).len(), 1);
        assert_eq!(text.find(&fold("PAGE 1 - name:")).len(), 1);
        assert_eq!(text.find(&fold("e")).len(), 2, "every occurrence");
        assert!(text.find(&fold("page 2")).is_empty());
        assert!(text.find(&fold("  ")).is_empty(), "nothing to look for");
    }

    #[test]
    fn boxes_join_along_a_line_and_break_between_lines() {
        let boxes = [[10., 10., 5., 10.], [15., 10., 5., 10.], [22., 11., 5., 9.], [10., 30., 5., 10.]];
        assert_eq!(lines(boxes.iter()), vec![[10., 10., 17., 10.], [10., 30., 5., 10.]]);
    }
}

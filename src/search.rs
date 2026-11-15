//! The text of a document: each page's characters with where they are drawn
//! (read with hayro's interpreter, which the renderer uses too), the places a
//! phrase occurs among them, and the stretch of them between two points.

use std::ops::Range;

use hayro::hayro_interpret::font::{Glyph, GlyphRun};
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterCache, InterpreterSettings, SoftMask,
    TransformExt, interpret_page,
};
use hayro::hayro_syntax::Pdf;
use hayro::kurbo::{Affine, BezPath, Point, Rect};

/// A box on a page: `[x, y, width, height]` in view space (points, from the top left).
pub type Area = [f32; 4];

/// How far a character reaches above and below its baseline, in thousandths
/// of the font size. Fonts differ; this is the box a highlight is drawn with.
const ASCENT: f64 = 780.;
const DESCENT: f64 = -220.;
/// Characters further apart than this share of the font size are separate words.
const WORD_GAP: f64 = 0.2;
/// Characters further apart than this share of the font size, up or down the
/// page, are on separate lines.
const LINE_GAP: f64 = 0.5;
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
    /// Each as the page has it, to copy: in its own case, and with a line
    /// break where a new line starts. (A character that is two in lower case
    /// stands at the first, with [`NOTHING`] at the second.)
    source: Vec<char>,
}

const NOTHING: char = '\0';
/// Characters this much further (in points) up or down the page from a point
/// than the nearest one are still on the line nearest it.
const SAME_LINE: f32 = 1.;

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
                let areas = self.boxes(start..start + phrase.len());
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

    /// The boxes that cover a stretch of the characters, one per line it runs over.
    pub fn boxes(&self, range: Range<usize>) -> Vec<Area> {
        lines(self.areas[self.within(range)].iter().flatten())
    }

    /// A stretch of the characters as the page has them, to copy.
    pub fn text(&self, range: Range<usize>) -> String {
        self.source[self.within(range)].iter().filter(|c| **c != NOTHING).collect::<String>().trim().to_string()
    }

    /// The place between two characters nearest a point on the page (a count
    /// of the characters before it), if the page has any text.
    pub fn caret_at(&self, x: f32, y: f32) -> Option<usize> {
        // The nearest line first (how far up or down the page a character's
        // box is), then the nearest character along it.
        let away = |&[left, top, width, height]: &Area| ((top - y).max(y - top - height).max(0.), (left - x).max(x - left - width).max(0.));
        let drawn = || self.areas.iter().enumerate().filter_map(|(index, area)| Some((index, area.as_ref()?)));
        let line = drawn().map(|(_, area)| away(area).0).min_by(f32::total_cmp)?;
        let on_line = drawn().filter(|(_, area)| away(area).0 <= line + SAME_LINE);
        let (index, area) = on_line.min_by(|a, b| away(a.1).1.total_cmp(&away(b.1).1))?;
        // After it, from its middle on.
        Some(if x > area[0] + area[2] / 2. { index + 1 } else { index })
    }

    /// The word around a place between characters.
    pub fn word_at(&self, caret: usize) -> Range<usize> {
        let caret = caret.min(self.chars.len());
        let in_word = |index: &usize| self.areas[*index].is_some();
        let start = (0..caret).rev().take_while(in_word).last().unwrap_or(caret);
        let end = (caret..self.chars.len()).take_while(in_word).last().map_or(caret, |last| last + 1);
        start..end
    }

    fn within(&self, range: Range<usize>) -> Range<usize> {
        let end = range.end.min(self.chars.len());
        range.start.min(end)..end
    }

    fn push(&mut self, c: char, area: Option<Area>) {
        if c.is_whitespace() || c.is_control() {
            self.space(' ');
            return;
        }
        for (i, lower) in c.to_lowercase().enumerate() {
            self.chars.push(lower);
            self.areas.push(area);
            self.source.push(if i == 0 { c } else { NOTHING });
        }
    }

    /// Words (`' '`) or lines (`'\n'`) are apart here.
    fn space(&mut self, between: char) {
        match self.source.last_mut() {
            // A new line counts for more than a space.
            Some(last) if last.is_whitespace() => *last = if between == '\n' { between } else { *last },
            Some(_) => {
                self.chars.push(' ');
                self.areas.push(None);
                self.source.push(between);
            }
            None => {}
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
///
/// A page is given by its index and the quarter turns clockwise it is shown with.
pub fn read_pages(pdf: &Pdf, pages: impl IntoIterator<Item = (usize, u8)>, mut each: impl FnMut(usize, PageText) -> bool) {
    let cache = InterpreterCache::new();
    for (index, turns) in pages {
        if !each(index, read_page(pdf, &cache, index, turns)) {
            break;
        }
    }
}

fn read_page<'a>(pdf: &'a Pdf, cache: &InterpreterCache<'a>, index: usize, turns: u8) -> PageText {
    let mut reader = Reader::default();
    if let Some(page) = pdf.pages().get(index) {
        let (width, height) = page.render_dimensions();
        let turn = Affine::new(crate::model::turn(turns, width as f64, height as f64));
        let (width, height) = if turns % 2 == 1 { (height, width) } else { (width, height) };
        // The same space the page is drawn in at 100%: view space.
        let mut context = Context::new(
            turn * page.initial_transform(true).to_kurbo(),
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
            if let Some((last, last_size)) = self.last {
                // Further up or down the page than a line is tall: a new line.
                if (start.y - last.y).abs() > LINE_GAP * size.max(last_size) {
                    self.text.space('\n');
                } else if last.distance(start) > WORD_GAP * size.max(last_size) {
                    self.text.space(' ');
                }
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
    fn the_text_of_a_turned_page_is_where_it_is_shown() {
        let (pdf, cache) = (sample(), InterpreterCache::new());
        // All of what a phrase covers, as one box.
        let around = |text: &PageText, phrase: &str| {
            let found = text.find(&fold(phrase));
            assert_eq!(found.len(), 1, "{phrase} is on the page once");
            let (x0, y0, x1, y1) = found[0].iter().fold((f32::MAX, f32::MAX, 0f32, 0f32), |(x0, y0, x1, y1), &[x, y, w, h]| {
                (x0.min(x), y0.min(y), x1.max(x + w), y1.max(y + h))
            });
            [x0, y0, x1 - x0, y1 - y0]
        };
        // The first page is 612 × 792; turned to the right, what was `y` down it is `792 - y` across.
        let [x, y, w, h] = around(&read_page(&pdf, &cache, 0, 0), "NAME");
        for (turns, there) in [(1, [792. - y - h, x, h, w]), (2, [612. - x - w, 792. - y - h, w, h]), (3, [y, 612. - x - w, h, w])] {
            let turned = around(&read_page(&pdf, &cache, 0, turns), "NAME");
            turned.iter().zip(there).for_each(|(a, b)| assert!((a - b).abs() < 0.01, "turned {turns}: {turned:?} for {there:?}"));
        }
    }

    #[test]
    fn text_is_read_with_where_it_is() {
        let pdf = sample();
        let mut pages = Vec::new();
        read_pages(&pdf, [(0, 0), (1, 0)], |index, text| {
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
        let text = &read_page(&sample(), &InterpreterCache::new(), 0, 0);
        assert_eq!(text.find(&fold("page   1")).len(), 1);
        assert_eq!(text.find(&fold("PAGE 1 - name:")).len(), 1);
        assert_eq!(text.find(&fold("e")).len(), 2, "every occurrence");
        assert!(text.find(&fold("page 2")).is_empty());
        assert!(text.find(&fold("  ")).is_empty(), "nothing to look for");
    }

    #[test]
    fn text_is_picked_out_between_two_points() {
        // Two lines: "Name: Jane" and "Élan", each character 10 wide and 12 tall.
        let mut text = PageText::default();
        let put = |text: &mut PageText, line: &str, top: f32| {
            for (i, c) in line.chars().enumerate() {
                text.push(c, Some([100. + 10. * i as f32, top, 10., 12.]));
            }
        };
        put(&mut text, "Name: Jane", 50.);
        text.space('\n');
        put(&mut text, "Élan", 70.);

        // A point is taken to the nearest gap between characters, on the nearest line.
        assert_eq!(text.caret_at(101., 55.), Some(0));
        assert_eq!(text.caret_at(108., 55.), Some(1), "past the middle of a character is after it");
        assert_eq!(text.caret_at(20., 40.), Some(0), "off to the left, above");
        assert_eq!(text.caret_at(500., 58.), Some(10), "off to the right: the end of that line");
        assert_eq!(text.caret_at(500., 75.), Some(15), "and of the one below");
        assert_eq!(PageText::default().caret_at(0., 0.), None);

        // What is copied is as the page has it, with its line breaks.
        assert_eq!(text.text(0..4), "Name");
        assert_eq!(text.text(6..15), "Jane\nÉlan");
        assert_eq!(text.text(4..7), ": J");
        assert_eq!(text.text(12..99), "lan", "no further than the text goes");
        assert_eq!(text.boxes(6..15), vec![[160., 50., 40., 12.], [100., 70., 40., 12.]]);
        assert_eq!(text.find(&fold("jane élan")).len(), 1, "a line break is a space to look for");

        // A word reaches from one gap to the next.
        assert_eq!(text.word_at(2), 0..5);
        assert_eq!(text.word_at(8), 6..10);
        assert_eq!(text.word_at(10), 6..10, "at its end");
        assert_eq!(text.word_at(15), 11..15);
    }

    #[test]
    fn lines_of_a_page_are_read_as_lines() {
        let mut doc = lopdf::Document::load_mem(&crate::pdf::tests::sample_pdf()).unwrap();
        let page = doc.get_pages()[&1];
        let content = "BT /F1 24 Tf 72 700 Td (First Line) Tj 0 -30 Td (Second) Tj ET";
        doc.change_page_content(page, content.as_bytes().to_vec()).unwrap();
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let pdf = Pdf::new(bytes).unwrap_or_else(|_| panic!("the sample doesn't open"));
        let text = read_page(&pdf, &InterpreterCache::new(), 0, 0);
        assert_eq!(text.text(0..usize::MAX), "First Line\nSecond");
        assert_eq!(text.find(&fold("line second")).len(), 1);
    }

    #[test]
    fn boxes_join_along_a_line_and_break_between_lines() {
        let boxes = [[10., 10., 5., 10.], [15., 10., 5., 10.], [22., 11., 5., 9.], [10., 30., 5., 10.]];
        assert_eq!(lines(boxes.iter()), vec![[10., 10., 17., 10.], [10., 30., 5., 10.]]);
    }
}

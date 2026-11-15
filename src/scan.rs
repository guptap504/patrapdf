//! What a phone's document scanner does to a page: cut it out by its four
//! corners (which straightens it), and clean up its colours.
//!
//! A page this is done to is shown, and saved, as a picture of itself.

use serde::{Deserialize, Serialize};

/// A point: `(x, y)`.
pub type Spot = (f32, f32);

/// The colours a page is shown in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Look {
    /// As the file has it.
    #[default]
    Original,
    /// The paper made white (shadows and tints taken off it) and the ink dark.
    Enhanced,
    /// The same, without colour.
    Gray,
    /// Ink or paper, and nothing in between.
    Mono,
}

impl Look {
    pub const ALL: [Look; 4] = [Look::Original, Look::Enhanced, Look::Gray, Look::Mono];

    pub fn name(self) -> &'static str {
        match self {
            Look::Original => "Original",
            Look::Enhanced => "Enhance",
            Look::Gray => "Grayscale",
            Look::Mono => "Black & white",
        }
    }
}

/// The part of a page that is kept.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    /// Its corners on the page as the file has it (view space, in points):
    /// top left, top right, bottom right, bottom left. They need not make a
    /// rectangle: what is between them is pulled straight into one.
    pub quad: [Spot; 4],
    /// The size of the page it makes, in points.
    pub width: f32,
    pub height: f32,
}

/// What has been done to a page: nothing, by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    #[serde(default)]
    pub look: Look,
}

impl Fix {
    /// Whether the page is as the file has it.
    pub fn is_none(&self) -> bool {
        self.crop.is_none() && self.look == Look::Original
    }
}

/// The corners of a `width` × `height` rectangle, in the order of [`Crop::quad`].
pub fn corners(width: f32, height: f32) -> [Spot; 4] {
    [(0., 0.), (width, 0.), (width, height), (0., height)]
}

/// The size of the rectangle four corners are pulled into: as wide as its
/// top and bottom are long (between the two), as high as its sides.
pub fn size_of(quad: &[Spot; 4]) -> (f32, f32) {
    let length = |a: Spot, b: Spot| (b.0 - a.0).hypot(b.1 - a.1);
    ((length(quad[0], quad[1]) + length(quad[3], quad[2])) / 2., (length(quad[0], quad[3]) + length(quad[1], quad[2])) / 2.)
}

/// Whether four corners (in the order of [`Crop::quad`]) go round a shape
/// with no dent in it, each corner turning by more than a sliver, and no
/// side shorter than `least`.
pub fn is_sound(quad: &[Spot; 4], least: f32) -> bool {
    (0..4).all(|i| {
        let (a, b, c) = (quad[i], quad[(i + 1) % 4], quad[(i + 2) % 4]);
        let (side, next) = ((b.0 - a.0, b.1 - a.1), (c.0 - b.0, c.1 - b.1));
        let (length, next_length) = (side.0.hypot(side.1), next.0.hypot(next.1));
        // (y runs down the page, so going round clockwise turns this way.)
        length >= least && side.0 * next.1 - side.1 * next.0 > 0.2 * length * next_length
    })
}

/// How one four-cornered shape is pulled into another, taking everything in
/// it along (a projective map): what straightens a page seen at an angle.
#[derive(Clone, Copy, Debug)]
pub struct Warp([f64; 9]);

impl Warp {
    /// The map that takes each corner of `from` to the same corner of `to`,
    /// if there is one (not for corners in a line).
    pub fn between(from: [Spot; 4], to: [Spot; 4]) -> Option<Warp> {
        // Eight equations for its eight numbers, two from each corner.
        let mut rows = [[0f64; 9]; 8];
        for corner in 0..4 {
            let (x, y) = (from[corner].0 as f64, from[corner].1 as f64);
            let (u, v) = (to[corner].0 as f64, to[corner].1 as f64);
            rows[2 * corner] = [x, y, 1., 0., 0., 0., -u * x, -u * y, u];
            rows[2 * corner + 1] = [0., 0., 0., x, y, 1., -v * x, -v * y, v];
        }
        for column in 0..8 {
            let pivot = (column..8).max_by(|&a, &b| rows[a][column].abs().total_cmp(&rows[b][column].abs()))?;
            if rows[pivot][column].abs() < 1e-9 {
                return None;
            }
            rows.swap(column, pivot);
            let solved = rows[column];
            for (row, values) in rows.iter_mut().enumerate() {
                let factor = values[column] / solved[column];
                if row != column && factor != 0. {
                    for (value, with) in values.iter_mut().zip(solved).skip(column) {
                        *value -= factor * with;
                    }
                }
            }
        }
        let mut map = [1f64; 9];
        for (i, row) in rows.iter().enumerate() {
            map[i] = row[8] / row[i];
        }
        map.iter().all(|n| n.is_finite()).then_some(Warp(map))
    }

    /// Where a point goes.
    pub fn at(&self, (x, y): Spot) -> Spot {
        let [a, b, c, d, e, f, g, h, i] = self.0;
        let (x, y) = (x as f64, y as f64);
        let w = g * x + h * y + i;
        (((a * x + b * y + c) / w) as f32, ((d * x + e * y + f) / w) as f32)
    }
}

/// The part of a picture (BGRA, `width` × `height`) between the corners
/// `quad` (in its pixels), pulled straight into one of `out_width` × `out_height`.
pub fn straighten(bgra: &[u8], width: usize, height: usize, quad: [Spot; 4], out_width: usize, out_height: usize) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || out_width == 0 || out_height == 0 || bgra.len() != width * height * 4 {
        return None;
    }
    // From each pixel of the result back to where it is in the picture.
    let [a, b, c, d, e, f, g, h, i] = Warp::between(corners(out_width as f32, out_height as f32), quad)?.0;
    let (last_x, last_y) = ((width - 1) as f64, (height - 1) as f64);
    let mut out = vec![0u8; out_width * out_height * 4];
    for (y, row) in out.chunks_exact_mut(out_width * 4).enumerate() {
        let down = y as f64 + 0.5;
        // Along a row these grow by `a`, `d` and `g` a pixel.
        let (mut u, mut v, mut w) = (a * 0.5 + b * down + c, d * 0.5 + e * down + f, g * 0.5 + h * down + i);
        for pixel in row.chunks_exact_mut(4) {
            // Between the four pixels around that place, by how near each is.
            let (x, y) = ((u / w - 0.5).clamp(0., last_x), (v / w - 0.5).clamp(0., last_y));
            let (left, top) = (x as usize, y as usize);
            let (right, bottom) = ((left + 1).min(width - 1), (top + 1).min(height - 1));
            let (along, downward) = (((x - left as f64) * 256.) as u32, ((y - top as f64) * 256.) as u32);
            let (upper, lower) = (top * width * 4, bottom * width * 4);
            for (channel, value) in pixel.iter_mut().enumerate() {
                let of = |row: usize, column: usize| bgra[row + column * 4 + channel] as u32;
                let above = of(upper, left) * (256 - along) + of(upper, right) * along;
                let below = of(lower, left) * (256 - along) + of(lower, right) * along;
                *value = ((above * (256 - downward) + below * downward + (1 << 15)) >> 16) as u8;
            }
            u += a;
            v += d;
            w += g;
        }
    }
    Some(out)
}

/// How bright a pixel is (0 to 255): green counts most, blue least.
fn brightness(blue: u8, green: u8, red: u8) -> u8 {
    ((blue as u32 * 29 + green as u32 * 150 + red as u32 * 77) >> 8) as u8
}

/// The paper is as bright, in each small square of the page, as the pixel
/// this share of that square's pixels are no brighter than: text never
/// covers so much of a square that this would be ink.
const PAPER_SHARE: f32 = 0.85;
/// Paper comes out a little short of white (it is never even), so what is
/// this bright or brighter once the paper is evened out is made white.
const WHITE_FROM: f32 = 235.;
/// The ink is as dark as the darkest pixels are, this share of them being
/// darker still; it is made black, unless it is lighter than `BLACK_UP_TO`.
const INK_SHARE: f32 = 0.02;
const BLACK_UP_TO: usize = 110;
/// In black and white, what is darker than this (of 255, the paper being
/// evened out) is ink. The step between the two is this wide either side,
/// so that edges stay smooth on screen.
const INK_BELOW: f32 = 190.;
const INK_EDGE: f32 = 24.;
/// A page whose paper is darker than this isn't paper: light on dark, or a
/// photograph. Its paper is left alone.
const DARKEST_PAPER: f32 = 100.;

/// How bright the paper is across a page, in squares `cell` pixels wide:
/// what the page would look like with nothing written on it.
struct Paper {
    cell: usize,
    columns: usize,
    rows: usize,
    /// For each square (row after row): blue, green and red.
    levels: Vec<[f32; 3]>,
}

impl Paper {
    fn of(bgra: &[u8], width: usize, height: usize) -> Paper {
        let cell = (width.max(height) / 40).max(8);
        let (columns, rows) = (width.div_ceil(cell), height.div_ceil(cell));
        // Every pixel of a small square; of a large one, enough of them to tell.
        let step = (cell / 12).max(1);
        let mut levels = vec![[255f32; 3]; columns * rows];
        let mut counts = vec![[[0u32; 256]; 3]; columns];
        for row in 0..rows {
            counts.iter_mut().for_each(|count| *count = [[0; 256]; 3]);
            for y in (row * cell..((row + 1) * cell).min(height)).step_by(step) {
                for x in (0..width).step_by(step) {
                    let pixel = &bgra[(y * width + x) * 4..][..3];
                    let count = &mut counts[x / cell];
                    for channel in 0..3 {
                        count[channel][pixel[channel] as usize] += 1;
                    }
                }
            }
            for (column, count) in counts.iter().enumerate() {
                for channel in 0..3 {
                    let total: u32 = count[channel].iter().sum();
                    let mut below = 0;
                    let level = count[channel].iter().position(|&n| {
                        below += n;
                        below as f32 >= total as f32 * PAPER_SHARE
                    });
                    levels[row * columns + column][channel] = level.unwrap_or(255).max(1) as f32;
                }
            }
        }
        let mut paper = Paper { cell, columns, rows, levels };
        paper.settle();
        paper
    }

    /// Even the squares out: one with no paper in it (a picture, a heading
    /// in a dark band) takes its paper from those around it, and none is far
    /// darker than the page's paper is on the whole.
    fn settle(&mut self) {
        let mut usual = [255f32; 3];
        for (channel, usual) in usual.iter_mut().enumerate() {
            let mut all: Vec<f32> = self.levels.iter().map(|level| level[channel]).collect();
            all.sort_by(f32::total_cmp);
            *usual = all[all.len() / 2];
        }
        if brightness(usual[0] as u8, usual[1] as u8, usual[2] as u8) as f32 <= DARKEST_PAPER {
            self.levels.fill([255.; 3]);
            return;
        }
        let around = |levels: &[[f32; 3]], row: usize, column: usize, of: &dyn Fn(f32, f32) -> f32, from: f32| {
            let mut found = [from; 3];
            let mut squares = 0.;
            for r in row.saturating_sub(1)..(row + 2).min(self.rows) {
                for c in column.saturating_sub(1)..(column + 2).min(self.columns) {
                    squares += 1.;
                    for channel in 0..3 {
                        found[channel] = of(found[channel], levels[r * self.columns + c][channel]);
                    }
                }
            }
            (found, squares)
        };
        // The brightest around each, then twice the mean around each.
        let passes: [(&dyn Fn(f32, f32) -> f32, f32, bool); 3] = [(&f32::max, 0., false), (&|a, b| a + b, 0., true), (&|a, b| a + b, 0., true)];
        for (of, from, mean) in passes {
            let before = self.levels.clone();
            for row in 0..self.rows {
                for column in 0..self.columns {
                    let (found, squares) = around(&before, row, column, of, from);
                    self.levels[row * self.columns + column] = if mean { found.map(|sum| sum / squares) } else { found };
                }
            }
        }
        for level in &mut self.levels {
            for channel in 0..3 {
                level[channel] = level[channel].max(usual[channel] * 0.55).max(32.);
            }
        }
    }

    /// For each pixel across (or down) the page: the two squares it is
    /// between the middles of, and how far it is from the first to the second.
    fn between(&self, pixels: usize, squares: usize) -> Vec<(usize, usize, f32)> {
        (0..pixels)
            .map(|pixel| {
                let at = ((pixel as f32 + 0.5) / self.cell as f32 - 0.5).clamp(0., (squares - 1) as f32);
                let first = at as usize;
                (first, (first + 1).min(squares - 1), at - first as f32)
            })
            .collect()
    }
}

/// Give a picture of a page (BGRA, `width` × `height`) a [`Look`].
pub fn recolour(bgra: &mut [u8], width: usize, height: usize, look: Look) {
    if look == Look::Original || width == 0 || height == 0 || bgra.len() != width * height * 4 {
        return;
    }
    // Every pixel as it would be on even, white paper; and how many there are of each brightness.
    let paper = Paper::of(bgra, width, height);
    let (across, down) = (paper.between(width, paper.columns), paper.between(height, paper.rows));
    let mut line = vec![[255f32; 3]; paper.columns];
    let mut how_many = [0u32; 256];
    for (y, row) in bgra.chunks_exact_mut(width * 4).enumerate() {
        let (above, below, share) = down[y];
        for (column, level) in line.iter_mut().enumerate() {
            let (a, b) = (paper.levels[above * paper.columns + column], paper.levels[below * paper.columns + column]);
            *level = [0, 1, 2].map(|channel| 255. / (a[channel] + (b[channel] - a[channel]) * share));
        }
        for (pixel, &(left, right, share)) in row.chunks_exact_mut(4).zip(&across) {
            let (a, b) = (line[left], line[right]);
            for channel in 0..3 {
                let gain = a[channel] + (b[channel] - a[channel]) * share;
                pixel[channel] = (pixel[channel] as f32 * gain).min(255.) as u8;
            }
            how_many[brightness(pixel[0], pixel[1], pixel[2]) as usize] += 1;
        }
    }
    // Then from the ink (black) to just short of the paper (white).
    let mut darker = 0;
    let ink = how_many.iter().position(|&n| {
        darker += n;
        darker as f32 >= (width * height) as f32 * INK_SHARE
    });
    let black = ink.unwrap_or(0).min(BLACK_UP_TO) as f32;
    let shade: [u8; 256] = std::array::from_fn(|value| {
        let value = value as f32;
        let shade = match look {
            Look::Mono => (value - (INK_BELOW - INK_EDGE)) / (2. * INK_EDGE),
            _ => (value - black) / (WHITE_FROM - black),
        };
        (shade.clamp(0., 1.) * 255.).round() as u8
    });
    for pixel in bgra.chunks_exact_mut(4) {
        if look == Look::Enhanced {
            for value in &mut pixel[..3] {
                *value = shade[*value as usize];
            }
        } else {
            let gray = shade[brightness(pixel[0], pixel[1], pixel[2]) as usize];
            pixel[..3].fill(gray);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_pulled_to_corners_and_what_is_between_them_follows() {
        let quad = [(10., 20.), (110., 30.), (120., 180.), (5., 170.)];
        let warp = Warp::between(quad, corners(100., 150.)).unwrap();
        for (corner, to) in quad.into_iter().zip(corners(100., 150.)) {
            let at = warp.at(corner);
            assert!((at.0 - to.0).abs() < 1e-3 && (at.1 - to.1).abs() < 1e-3, "{corner:?} goes to {at:?}, not {to:?}");
        }
        // A straight line stays one: the middle of a side is somewhere along that side of the rectangle.
        let middle = warp.at((60., 25.));
        assert!(middle.1.abs() < 1e-3 && middle.0 > 30. && middle.0 < 70., "{middle:?}");
        // And back again.
        let back = Warp::between(corners(100., 150.), quad).unwrap();
        let there = back.at(warp.at((50., 90.)));
        assert!((there.0 - 50.).abs() < 1e-3 && (there.1 - 90.).abs() < 1e-3, "{there:?}");
        // Corners in a line make no shape to pull.
        assert!(Warp::between([(0., 0.), (1., 1.), (2., 2.), (3., 3.)], corners(10., 10.)).is_none());

        assert_eq!(size_of(&corners(80., 50.)), (80., 50.));
        assert!(is_sound(&quad, 20.));
        assert!(!is_sound(&quad, 120.), "a side is shorter than that");
        assert!(!is_sound(&[(0., 0.), (100., 0.), (40., 40.), (0., 100.)], 20.), "dented");
        assert!(!is_sound(&[(0., 0.), (0., 100.), (100., 100.), (100., 0.)], 20.), "the wrong way round");
    }

    /// A picture `width` × `height`, each pixel `of(x, y)` (blue, green, red).
    fn picture(width: usize, height: usize, of: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| {
                let [b, g, r] = of(i % width, i / width);
                [b, g, r, 255]
            })
            .collect()
    }

    #[test]
    fn a_part_of_a_picture_is_cut_out_straight() {
        // Blue grows across, green down.
        let (w, h) = (64, 48);
        let source = picture(w, h, |x, y| [(x * 4) as u8, (y * 5) as u8, 7]);
        // The whole of it, the same size: as it was.
        let same = straighten(&source, w, h, corners(w as f32, h as f32), w, h).unwrap();
        assert_eq!(same, source);
        // The right half, at twice the size across.
        let half = straighten(&source, w, h, [(32., 0.), (64., 0.), (64., 48.), (32., 48.)], 64, 48).unwrap();
        let at = |x: usize, y: usize| &half[(y * 64 + x) * 4..][..4];
        assert!(at(0, 10)[0].abs_diff(128) <= 3 && at(63, 10)[0].abs_diff(252) <= 3, "{:?} {:?}", at(0, 10), at(63, 10));
        assert_eq!((at(20, 10)[1], at(20, 10)[2], at(20, 10)[3]), (50, 7, 255));
        // On its side: the top left of the result is the bottom left of the picture.
        let turned = straighten(&source, w, h, [(0., 48.), (0., 0.), (64., 0.), (64., 48.)], 48, 64).unwrap();
        assert!(turned[0] <= 4 && turned[1] >= 231, "{:?}", &turned[..4]);
        assert!(straighten(&source, w, h + 1, corners(1., 1.), 4, 4).is_none());
    }

    /// A page photographed in warm light: its paper in a shadow that deepens
    /// to the right and down, with dark lines of writing across it.
    fn photographed(width: usize, height: usize) -> Vec<u8> {
        picture(width, height, |x, y| {
            let light = 1. - 0.45 * (x as f32 / width as f32) - 0.15 * (y as f32 / height as f32);
            let ink = y % 24 < 3 && x % 7 < 5 && x > 20 && x < width - 20;
            let [b, g, r] = if ink { [48., 58., 64.] } else { [170., 205., 225.] };
            [(b * light) as u8, (g * light) as u8, (r * light) as u8]
        })
    }

    #[test]
    fn the_paper_of_a_photographed_page_is_made_white_and_its_ink_dark() {
        let (w, h) = (400, 520);
        let ink_at = |x: usize, y: usize| y % 24 < 3 && x % 7 < 5 && x > 20 && x < w - 20;
        let mut page = photographed(w, h);
        recolour(&mut page, w, h, Look::Enhanced);
        let at = |page: &[u8], x: usize, y: usize| [page[(y * w + x) * 4], page[(y * w + x) * 4 + 1], page[(y * w + x) * 4 + 2]];
        // In the light and deep in the shadow alike.
        for (x, y) in [(40, 40), (200, 260), (370, 500), (380, 30), (30, 490)] {
            let (paper, ink) = (at(&page, x, y - y % 24 + 12), at(&page, x - x % 7 + 1, y - y % 24 + 1));
            assert!(paper.iter().all(|&v| v >= 245), "paper at {x},{y} is {paper:?}");
            assert!(ink.iter().all(|&v| v <= 40), "ink at {x},{y} is {ink:?}");
        }
        assert!(page.chunks_exact(4).all(|pixel| pixel[3] == 255));

        // Without colour: every pixel a gray.
        let mut gray = photographed(w, h);
        recolour(&mut gray, w, h, Look::Gray);
        assert!(gray.chunks_exact(4).all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2]));
        assert!(at(&gray, 370, 492)[0] >= 245 && at(&gray, 365, 481)[0] <= 40);

        // In black and white: ink where the ink is, paper everywhere else.
        let mut mono = photographed(w, h);
        recolour(&mut mono, w, h, Look::Mono);
        for y in 0..h {
            for x in 0..w {
                let value = at(&mono, x, y)[0];
                assert_eq!(value, if ink_at(x, y) { 0 } else { 255 }, "at {x},{y}");
            }
        }

        // The original look changes nothing.
        let mut same = photographed(w, h);
        recolour(&mut same, w, h, Look::Original);
        assert_eq!(same, photographed(w, h));
    }

    #[test]
    fn a_clean_page_stays_as_it_is_and_a_dark_one_is_not_bleached() {
        // Black on white, with a patch of pale yellow and a red mark.
        let (w, h) = (320, 400);
        let clean = picture(w, h, |x, y| match (x, y) {
            (60..90, 60..80) => [0, 0, 220],
            (200..260, 200..230) => [170, 250, 255],
            _ if y % 20 < 2 && x % 5 < 3 => [0, 0, 0],
            _ => [255, 255, 255],
        });
        let mut page = clean.clone();
        recolour(&mut page, w, h, Look::Enhanced);
        let at = |page: &[u8], x: usize, y: usize| [page[(y * w + x) * 4], page[(y * w + x) * 4 + 1], page[(y * w + x) * 4 + 2]];
        assert_eq!(at(&page, 150, 150), [255, 255, 255]);
        assert_eq!(at(&page, 151, 140), [0, 0, 0]);
        let red = at(&page, 75, 70);
        assert!(red[2] >= 220 && red[0] <= 10 && red[1] <= 10, "the red mark is {red:?}");
        let yellow = at(&page, 230, 215);
        assert!(yellow[0] < 215 && yellow[2] == 255, "the yellow patch is {yellow:?}");

        // White on nearly black: evening out its "paper" would wipe it.
        let dark = picture(w, h, |x, y| if y % 20 < 2 && x % 5 < 3 { [240, 240, 240] } else { [25, 25, 30] });
        let mut page = dark.clone();
        recolour(&mut page, w, h, Look::Enhanced);
        assert!(at(&page, 151, 140).iter().all(|&v| v >= 240) && at(&page, 150, 150).iter().all(|&v| v <= 30), "{:?}", at(&page, 150, 150));
    }
}

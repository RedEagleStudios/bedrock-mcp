//! The game's pixel fonts as glyph bitmaps on their own pixel grid (1 font pixel = 1 cell).

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use ttf_parser::{Face, OutlineBuilder};

use crate::Error;

/// Font files under the game directory; only the pixel fonts are kept (`Minecraft*`,
/// `Mojangles*`), the Noto fonts are vector fonts without a pixel grid.
const FONT_DIRS: [&str; 2] = ["data/fonts", "data/gui/dist/hbui/fonts"];

/// A pixel font has at most this many grid cells per em; finer "grids" are vector shapes.
const MAX_CELLS_PER_EM: u32 = 32;

/// One glyph: ink cells row-major, rows counted downward from the baseline
/// (row 0 is the first row below it, row -1 the last row above it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Glyph {
    pub ch: char,
    pub width: usize,
    pub height: usize,
    /// Baseline-relative row of the first ink row.
    pub top: i32,
    /// Columns from the pen position to the first ink column.
    pub lead: i32,
    /// Columns from this pen position to the next.
    pub advance: i32,
    pub bits: Vec<bool>,
    /// The ink cells as `(row, col)`, row-major.
    pub ink: Vec<(u16, u16)>,
}

impl Glyph {
    pub(crate) fn new(
        ch: char,
        width: usize,
        top: i32,
        lead: i32,
        advance: i32,
        bits: Vec<bool>,
    ) -> Self {
        let ink = (0..bits.len())
            .filter(|&i| bits[i])
            .map(|i| ((i / width) as u16, (i % width) as u16))
            .collect();
        Self {
            ch,
            width,
            height: bits.len() / width,
            top,
            lead,
            advance,
            bits,
            ink,
        }
    }

    #[cfg(test)]
    pub fn bit(&self, row: usize, col: usize) -> bool {
        self.bits[row * self.width + col]
    }
}

/// One font's glyphs.
#[derive(Clone, Debug)]
pub(crate) struct Font {
    pub name: String,
    pub glyphs: Vec<Glyph>,
    /// Advance of a space in cells.
    pub space: i32,
}

/// The pixel fonts text is matched against.
#[derive(Clone, Debug, Default)]
pub struct Fonts {
    pub(crate) fonts: Vec<Font>,
}

impl Fonts {
    /// The pixel fonts of a game installation (`<game>/data/fonts`, the OreUI fonts in
    /// `<game>/data/gui/dist/hbui/fonts`).
    pub fn load(game_dir: &Path) -> Result<Self, Error> {
        let mut paths: Vec<PathBuf> = Vec::new();
        for dir in FONT_DIRS {
            let Ok(entries) = fs::read_dir(game_dir.join(dir)) else {
                continue;
            };
            let mut found: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    (name.starts_with("Minecraft") || name.starts_with("Mojangles"))
                        && matches!(ext.to_ascii_lowercase().as_str(), "ttf" | "otf")
                })
                .collect();
            found.sort();
            paths.extend(found);
        }
        let fonts = Self::from_files(&paths)?;
        if fonts.fonts.is_empty() {
            return Err(Error(format!(
                "no Minecraft pixel fonts under {}/{{{}}}",
                game_dir.display(),
                FONT_DIRS.join(",")
            )));
        }
        Ok(fonts)
    }

    /// Pixel fonts from these files, in this order of preference; vector fonts are skipped.
    pub fn from_files(paths: &[PathBuf]) -> Result<Self, Error> {
        let mut fonts = Self::default();
        for path in paths {
            let data = fs::read(path)
                .map_err(|error| Error(format!("cannot read {}: {error}", path.display())))?;
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("font");
            if let Some(font) = Font::parse(&font_name(stem), &data) {
                fonts.push(font);
            }
        }
        Ok(fonts)
    }

    /// Add a font unless an earlier one has the same glyphs.
    pub(crate) fn push(&mut self, font: Font) {
        if !self.fonts.iter().any(|known| known.glyphs == font.glyphs) {
            self.fonts.push(font);
        }
    }

    /// Names of the loaded fonts.
    pub fn names(&self) -> Vec<&str> {
        self.fonts.iter().map(|font| font.name.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }
}

/// `Minecraft-Seven-66398119c2c20ee73019` → `Minecraft-Seven` (drops the bundler's hash).
fn font_name(stem: &str) -> String {
    match stem.rsplit_once('-') {
        Some((name, hash)) if hash.len() >= 16 && hash.chars().all(|c| c.is_ascii_hexdigit()) => {
            name.to_owned()
        }
        _ => stem.to_owned(),
    }
}

/// Characters to build templates for, most likely first: an identical later bitmap is dropped.
fn charset() -> impl Iterator<Item = char> {
    ('A'..='Z')
        .chain('a'..='z')
        .chain('0'..='9')
        .chain(
            (33u8..=126)
                .map(char::from)
                .filter(|c| !c.is_ascii_alphanumeric()),
        )
        .chain(('\u{a1}'..='\u{ff}').filter(|&c| c != '\u{ad}'))
}

#[derive(Default)]
struct Outline {
    contours: Vec<Vec<(f32, f32)>>,
    curved: bool,
}

impl OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.contours.push(vec![(x, y)]);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        if let Some(contour) = self.contours.last_mut() {
            contour.push((x, y));
        }
    }
    fn quad_to(&mut self, _: f32, _: f32, x: f32, y: f32) {
        self.curved = true;
        self.line_to(x, y);
    }
    fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, x: f32, y: f32) {
        self.curved = true;
        self.line_to(x, y);
    }
    fn close(&mut self) {}
}

impl Outline {
    /// Nonzero winding number test.
    fn contains(&self, x: f32, y: f32) -> bool {
        let mut winding = 0i32;
        for contour in &self.contours {
            for (i, &(x0, y0)) in contour.iter().enumerate() {
                let (x1, y1) = contour[(i + 1) % contour.len()];
                if (y0 <= y) != (y1 <= y) {
                    let cross = x0 + (y - y0) / (y1 - y0) * (x1 - x0);
                    if cross > x {
                        winding += if y1 > y0 { 1 } else { -1 };
                    }
                }
            }
        }
        winding != 0
    }

    fn points(&self) -> impl Iterator<Item = (f32, f32)> + '_ {
        self.contours.iter().flatten().copied()
    }
}

/// The grid offset of `values` for cell size `unit`, if nearly all lie on it. 90% rather
/// than all: `Minecraft-Ten` puts a few punctuation points on half cells.
fn grid_offset(values: &[i32], unit: i32, tolerance: i32) -> Option<i32> {
    let mut counts = std::collections::HashMap::<i32, usize>::new();
    for value in values {
        *counts.entry(value.rem_euclid(unit)).or_default() += 1;
    }
    let mut residues: Vec<(i32, usize)> = counts.iter().map(|(&r, &n)| (r, n)).collect();
    residues.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    residues.iter().take(2).find_map(|&(offset, _)| {
        let hits: usize = counts
            .iter()
            .filter(|&(&r, _)| {
                let d = (r - offset).rem_euclid(unit);
                d.min(unit - d) <= tolerance
            })
            .map(|(_, &n)| n)
            .sum();
        (hits * 100 >= values.len() * 90).then_some(offset)
    })
}

impl Font {
    /// Rasterize a pixel font at one cell per font pixel; `None` for vector fonts.
    pub(crate) fn parse(name: &str, data: &[u8]) -> Option<Self> {
        let face = Face::parse(data, 0).ok()?;
        let upm = i32::from(face.units_per_em());
        let mut outlines = Vec::new();
        let (mut xs, mut ys) = (Vec::new(), Vec::new());
        for ch in charset() {
            let Some(id) = face.glyph_index(ch).filter(|id| id.0 != 0) else {
                continue;
            };
            let mut outline = Outline::default();
            face.outline_glyph(id, &mut outline);
            if outline.curved {
                return None;
            }
            if ch.is_ascii() {
                for (x, y) in outline.points() {
                    xs.push(x.round() as i32);
                    ys.push(y.round() as i32);
                }
            }
            let advance = i32::from(face.glyph_hor_advance(id).unwrap_or(0));
            outlines.push((ch, outline, advance));
        }
        if xs.is_empty() {
            return None;
        }
        let tolerance = (upm / 1000).max(1);
        let min_unit = (upm / MAX_CELLS_PER_EM as i32).max(2);
        let (unit, ox, oy) = (min_unit..=upm / 4).rev().find_map(|unit| {
            Some((
                unit,
                grid_offset(&xs, unit, tolerance)?,
                grid_offset(&ys, unit, tolerance)?,
            ))
        })?;
        let (unit, ox, oy) = (unit as f32, ox as f32, oy as f32);
        let mut glyphs: Vec<Glyph> = Vec::new();
        let mut seen = HashSet::new();
        for (ch, outline, advance) in outlines {
            let Some(glyph) = rasterize(ch, &outline, advance, unit, ox, oy) else {
                continue;
            };
            if seen.insert((glyph.top, glyph.width, glyph.bits.clone())) {
                glyphs.push(glyph);
            }
        }
        let space = face
            .glyph_index(' ')
            .and_then(|id| face.glyph_hor_advance(id))
            .map_or(4, |adv| (f32::from(adv) / unit).round() as i32)
            .max(1);
        Some(Self {
            name: name.to_owned(),
            glyphs,
            space,
        })
    }
}

/// Sample the outline at cell centres: column `i` spans font x `[ox + i·u, ox + (i+1)·u)`,
/// row `j` spans font y `[oy - (j+1)·u, oy - j·u)`.
fn rasterize(ch: char, outline: &Outline, advance: i32, u: f32, ox: f32, oy: f32) -> Option<Glyph> {
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for (x, y) in outline.points() {
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    if x0 > x1 {
        return None;
    }
    let cols = ((x0 - ox) / u).floor() as i32 - 1..((x1 - ox) / u).ceil() as i32 + 1;
    let rows = ((oy - y1) / u).floor() as i32 - 1..((oy - y0) / u).ceil() as i32 + 1;
    let mut ink = Vec::new();
    for j in rows.clone() {
        for i in cols.clone() {
            let cx = ox + (i as f32 + 0.5) * u;
            let cy = oy - (j as f32 + 0.5) * u;
            if outline.contains(cx, cy) {
                ink.push((j, i));
            }
        }
    }
    let top = ink.iter().map(|&(j, _)| j).min()?;
    let bottom = ink.iter().map(|&(j, _)| j).max()?;
    let left = ink.iter().map(|&(_, i)| i).min()?;
    let right = ink.iter().map(|&(_, i)| i).max()?;
    let (width, height) = ((right - left + 1) as usize, (bottom - top + 1) as usize);
    let mut bits = vec![false; width * height];
    for &(j, i) in &ink {
        bits[(j - top) as usize * width + (i - left) as usize] = true;
    }
    Some(Glyph::new(
        ch,
        width,
        top,
        left,
        (advance as f32 / u).round() as i32,
        bits,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A glyph from rows of `#`/`.`, its last row `bottom` rows below the baseline row -1.
    pub fn glyph(ch: char, rows: &[&str], bottom: i32) -> Glyph {
        let width = rows[0].len();
        let bits: Vec<bool> = rows
            .iter()
            .flat_map(|r| r.bytes().map(|b| b == b'#'))
            .collect();
        Glyph::new(
            ch,
            width,
            bottom - rows.len() as i32,
            0,
            width as i32 + 1,
            bits,
        )
    }

    #[test]
    fn names_drop_the_bundler_hash() {
        assert_eq!(
            font_name("Minecraft-Seven-66398119c2c20ee73019"),
            "Minecraft-Seven"
        );
        assert_eq!(font_name("Mojangles"), "Mojangles");
        assert_eq!(font_name("Minecraft-Five-Bold"), "Minecraft-Five-Bold");
    }

    #[test]
    fn grid_offsets_need_nearly_every_coordinate_on_the_grid() {
        assert_eq!(grid_offset(&[50, 150, 250, 650], 100, 1), Some(50));
        assert_eq!(grid_offset(&[0, 100, 140, 300], 100, 1), None);
    }

    #[test]
    fn rasterizes_a_square_outline_into_cells() {
        // A 2×3-cell bar standing on the baseline, cells of 100 units offset by 50.
        let outline = Outline {
            contours: vec![vec![
                (50.0, 0.0),
                (250.0, 0.0),
                (250.0, 300.0),
                (50.0, 300.0),
            ]],
            curved: false,
        };
        let glyph = rasterize('I', &outline, 300, 100.0, 50.0, 0.0).unwrap();
        assert_eq!(
            (glyph.width, glyph.height, glyph.top, glyph.lead),
            (2, 3, -3, 0)
        );
        assert_eq!(glyph.advance, 3);
        assert!(glyph.bits.iter().all(|&b| b));
    }
}

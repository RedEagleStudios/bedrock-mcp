//! Text recognition for Minecraft Bedrock screenshots.
//!
//! Minecraft draws text with pixel fonts: every font pixel is a square block of screen
//! pixels. Instead of generic OCR, this finds text-like pixels, measures each line's
//! font-pixel size and grid phase, resamples the line to one value per font pixel and
//! matches the result against glyph bitmaps rasterized from the game's own fonts
//! (`Mojangles` for the classic UI, `Minecraft-Seven`/`-Ten`/`-Five` for OreUI screens).
//! Lines no font explains can fall back to the `tesseract` CLI when it is installed.
//!
//! The fonts are read from the game installation at runtime ([`Fonts::load`]); no glyph
//! data ships with this crate.

mod detect;
mod font;
mod grid;
mod image;
mod matcher;
mod tesseract;

use serde::Serialize;

use crate::{
    detect::{Box2, Plane},
    matcher::Reading,
};
pub use crate::{
    font::Fonts,
    image::{Image, Rect},
};

/// An error loading fonts or images.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Which recognizer produced a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// Matched against the game's pixel-font glyphs.
    Glyphs,
    /// Read by the tesseract CLI fallback.
    Tesseract,
}

/// One recognized line of text; the box is in image pixels.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Line {
    pub text: String,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Image pixels per font pixel (0 for tesseract lines).
    pub scale: f32,
    /// The matched font (empty for tesseract lines).
    pub font: String,
    /// 0–1: share of the line's ink the glyphs explain (tesseract's own confidence there).
    pub confidence: f32,
    pub engine: Engine,
}

impl Line {
    fn bounds(&self) -> Box2 {
        Box2 {
            x0: self.x as usize,
            y0: self.y as usize,
            x1: (self.x + self.width) as usize,
            y1: (self.y + self.height) as usize,
        }
    }
}

/// What to read and how.
#[derive(Clone, Debug)]
pub struct Options {
    /// Only read this part of the image (image pixels); the whole image when `None`.
    pub region: Option<Rect>,
    /// Ask the `tesseract` CLI (if installed) about text-like rows no font explains. Off by
    /// default: each row costs a process start (~150 ms) and on game captures it mostly
    /// reads icons and textures as noise.
    pub tesseract: bool,
    /// Glyph-matched lines below this confidence are dropped.
    pub min_confidence: f32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            region: None,
            tesseract: false,
            min_confidence: 0.8,
        }
    }
}

/// At most this many unexplained rows are passed to tesseract per call.
const MAX_TESSERACT_LINES: usize = 8;

/// Recognize the text lines of `image`, top to bottom, then left to right.
pub fn recognize(image: &Image, fonts: &Fonts, options: &Options) -> Vec<Line> {
    let full = Rect::new(0, 0, image.width(), image.height());
    let region = options
        .region
        .unwrap_or(full)
        .clamp(image.width(), image.height());
    if region.width == 0 || region.height == 0 {
        return Vec::new();
    }
    let plane = Plane {
        width: region.width as usize,
        height: region.height as usize,
        luma: image.luma(region),
    };
    let detection = detect::detect(&plane, image.height() as usize);
    let mut lines: Vec<Line> = Vec::new();
    let mut unread: Vec<Box2> = Vec::new();
    for candidate in &detection.candidates {
        let readings: Vec<(Reading, f32)> = grid::sample(&plane, &detection, candidate)
            .map(|sampled| {
                matcher::read(&sampled, fonts)
                    .into_iter()
                    .map(|reading| (reading, sampled.scale))
                    .collect()
            })
            .unwrap_or_default();
        let mut any = false;
        for (reading, scale) in readings {
            if !plausible(&reading, options.min_confidence) {
                continue;
            }
            any = true;
            let b = candidate.bounds;
            let x0 = reading.x0.max(0.0).round() as u32;
            let x1 = (reading.x1.round() as u32).max(x0 + 1);
            lines.push(Line {
                text: reading.text,
                x: region.x + x0,
                y: region.y + b.y0 as u32,
                width: x1 - x0,
                height: b.height() as u32,
                scale,
                font: reading.font,
                confidence: reading.confidence,
                engine: Engine::Glyphs,
            });
        }
        if !any {
            unread.push(candidate.bounds);
        }
    }
    let mut lines = dedupe(lines);
    if options.tesseract {
        lines.extend(fallback(image, region, &detection, &unread, &lines));
    }
    lines.sort_by_key(|line| (line.y + line.height / 2, line.x));
    lines
}

/// Whether a reading is text rather than a texture or icon that happens to match glyphs.
fn plausible(reading: &Reading, min_confidence: f32) -> bool {
    let glyphs: Vec<char> = reading
        .text
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let known = glyphs.iter().filter(|&&c| c != '\u{fffd}').count();
    let alphanumeric = glyphs.iter().filter(|c| c.is_alphanumeric()).count();
    reading.confidence >= min_confidence
        && known * 4 >= glyphs.len() * 3
        && (alphanumeric >= 2 || (glyphs.len() == 1 && reading.confidence >= 0.97))
}

/// Drop lines mostly covered by a more confident line (a shadow read as dark text, or the
/// same text found under both polarities).
fn dedupe(mut lines: Vec<Line>) -> Vec<Line> {
    lines.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    let mut kept: Vec<Line> = Vec::new();
    for line in lines {
        let b = line.bounds();
        let covered = kept
            .iter()
            .any(|k| k.bounds().intersection_area(&b) * 2 > b.area());
        if !covered {
            kept.push(line);
        }
    }
    kept
}

/// Tesseract readings of text-like rows the fonts did not explain.
fn fallback(
    image: &Image,
    region: Rect,
    detection: &detect::Detection,
    unread: &[Box2],
    read: &[Line],
) -> Vec<Line> {
    let mut rows: Vec<Box2> = unread
        .iter()
        .copied()
        .filter(|b| {
            let parts = detection
                .candidates
                .iter()
                .find(|c| c.bounds == *b)
                .map_or(0, |c| c.labels.len());
            let h = b.height();
            parts >= 3 && (8..=150).contains(&h) && b.width() * 2 >= h * 3
        })
        .collect();
    rows.sort_by_key(|b| std::cmp::Reverse(b.area()));
    let mut lines = Vec::new();
    for b in rows {
        if lines.len() >= MAX_TESSERACT_LINES {
            break;
        }
        let pad = (b.height() / 4) as u32;
        let rect = Rect::new(
            (region.x + b.x0 as u32).saturating_sub(pad),
            (region.y + b.y0 as u32).saturating_sub(pad),
            b.width() as u32 + 2 * pad,
            b.height() as u32 + 2 * pad,
        )
        .clamp(image.width(), image.height());
        let image_box = Box2 {
            x0: region.x as usize + b.x0,
            y0: region.y as usize + b.y0,
            x1: region.x as usize + b.x1,
            y1: region.y as usize + b.y1,
        };
        if read
            .iter()
            .any(|line| line.bounds().intersection_area(&image_box) * 2 > image_box.area())
        {
            continue;
        }
        if let Some((text, confidence)) = tesseract::read_line(image, rect) {
            lines.push(Line {
                text,
                x: image_box.x0 as u32,
                y: image_box.y0 as u32,
                width: b.width() as u32,
                height: b.height() as u32,
                scale: 0.0,
                font: String::new(),
                confidence,
                engine: Engine::Tesseract,
            });
        }
    }
    lines
}

#[cfg(test)]
mod tests;

//! Fallback for text the pixel fonts cannot explain (vector fonts, unusual glyphs): the
//! `tesseract` CLI on one cropped line, if it is installed.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use crate::image::{Image, Rect};

/// Words below this tesseract confidence (0–100) are dropped.
const MIN_WORD_CONFIDENCE: f32 = 50.0;
/// Tesseract reads best with text at least this tall.
const MIN_HEIGHT: u32 = 48;

/// Nearest-neighbour upscale by an integer factor.
fn upscale(image: &Image, factor: u32) -> Image {
    if factor <= 1 {
        return image.clone();
    }
    let (w, h) = (image.width() * factor, image.height() * factor);
    let src = image.rgba();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let at = (((y / factor) * image.width() + x / factor) * 4) as usize;
            rgba.extend_from_slice(&src[at..at + 4]);
        }
    }
    Image::new(w, h, rgba).expect("sized above")
}

/// Words and mean confidence (0–1) from `tesseract … tsv` output.
pub(crate) fn parse_tsv(tsv: &str) -> Option<(String, f32)> {
    let mut words = Vec::new();
    let mut total = 0f32;
    for line in tsv.lines().skip(1) {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 12 || fields[0] != "5" {
            continue;
        }
        let Ok(confidence) = fields[10].parse::<f32>() else {
            continue;
        };
        let text = fields[11].trim();
        if confidence >= MIN_WORD_CONFIDENCE && !text.is_empty() {
            words.push(text.to_owned());
            total += confidence;
        }
    }
    if words.is_empty() {
        return None;
    }
    let confidence = total / words.len() as f32 / 100.0;
    Some((words.join(" "), confidence))
}

/// Read one line of text in `rect` with tesseract; `None` when it is not installed, fails,
/// or finds nothing confident.
pub(crate) fn read_line(image: &Image, rect: Rect) -> Option<(String, f32)> {
    let crop = image.crop(rect);
    let factor = MIN_HEIGHT.div_ceil(rect.height.max(1)).clamp(1, 4);
    let png = upscale(&crop, factor).to_png().ok()?;
    let mut child = Command::new("tesseract")
        .args(["stdin", "stdout", "--psm", "7", "tsv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(&png).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_tsv(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsv_keeps_confident_words() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   1\t1\t0\t0\t0\t0\t0\t0\t100\t20\t-1\t\n\
                   5\t1\t1\t1\t1\t1\t0\t0\t40\t20\t90.5\tHello\n\
                   5\t1\t1\t1\t1\t2\t50\t0\t40\t20\t20\t%$#\n\
                   5\t1\t1\t1\t1\t3\t100\t0\t40\t20\t70.5\tworld\n";
        let (text, confidence) = parse_tsv(tsv).unwrap();
        assert_eq!(text, "Hello world");
        assert!((confidence - 0.805).abs() < 1e-4);
        assert_eq!(parse_tsv("level\n"), None);
    }

    #[test]
    fn upscaling_repeats_pixels() {
        let image = Image::new(1, 1, vec![1, 2, 3, 4]).unwrap();
        let big = upscale(&image, 2);
        assert_eq!((big.width(), big.height()), (2, 2));
        assert_eq!(big.rgba(), [1, 2, 3, 4].repeat(4));
    }
}

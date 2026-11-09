//! End-to-end recognition of synthetic pixel text.

use super::*;
use crate::font::{Font, Glyph, tests::glyph};

fn font() -> Font {
    Font {
        name: "synthetic".into(),
        glyphs: vec![
            glyph(
                'H',
                &[
                    "#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
                ],
                0,
            ),
            glyph('e', &[".###.", "#...#", "#####", "#....", ".####"], 0),
            glyph('l', &["#.", "#.", "#.", "#.", "#.", "#.", ".#"], 0),
            glyph('o', &[".###.", "#...#", "#...#", "#...#", ".###."], 0),
            glyph(
                'W',
                &[
                    "#...#", "#...#", "#...#", "#.#.#", "#.#.#", "#.#.#", ".#.#.",
                ],
                0,
            ),
            glyph('r', &["#.##", "##..", "#...", "#...", "#..."], 0),
            glyph(
                'd',
                &[
                    "....#", "....#", ".####", "#...#", "#...#", "#...#", ".####",
                ],
                0,
            ),
            glyph('!', &["#", "#", "#", "#", "#", ".", "#"], 0),
        ],
        space: 4,
    }
}

/// Draw `text` with its pen at (`x`, baseline `y`), `scale` pixels per font pixel, in
/// `color`, with a quarter-brightness shadow one font pixel down and right.
fn draw(
    rgba: &mut [u8],
    width: usize,
    font: &Font,
    text: &str,
    (x, y): (usize, usize),
    scale: usize,
    color: u8,
) {
    let mut paint = |glyph: &Glyph, pen: usize, shade: u8, shift: usize| {
        for r in 0..glyph.height {
            for c in 0..glyph.width {
                if !glyph.bit(r, c) {
                    continue;
                }
                let gx = pen + (glyph.lead as usize + c + shift) * scale;
                let gy = (y as i32 + (glyph.top + r as i32 + shift as i32) * scale as i32) as usize;
                for py in gy..gy + scale {
                    for px in gx..gx + scale {
                        let at = (py * width + px) * 4;
                        rgba[at..at + 3].fill(shade);
                    }
                }
            }
        }
    };
    for (shade, shift) in [(color / 4, 1), (color, 0)] {
        let mut pen = x;
        for ch in text.chars() {
            if ch == ' ' {
                pen += font.space as usize * scale;
                continue;
            }
            let glyph = font.glyphs.iter().find(|g| g.ch == ch).unwrap();
            paint(glyph, pen, shade, shift);
            pen += glyph.advance as usize * scale;
        }
    }
}

fn scene() -> Image {
    let (w, h) = (400usize, 300usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            // A dim, uneven background.
            let v = 40 + ((x / 7 + y / 5) % 3) as u8 * 6;
            rgba.extend_from_slice(&[v, v + 4, v + 10, 255]);
        }
    }
    let font = font();
    draw(&mut rgba, w, &font, "Hello World!", (20, 50), 3, 255);
    draw(&mut rgba, w, &font, "Hello", (40, 130), 5, 220);
    Image::new(w as u32, h as u32, rgba).unwrap()
}

fn fonts() -> Fonts {
    Fonts {
        fonts: vec![font()],
    }
}

#[test]
fn reads_shadowed_pixel_text_at_two_scales() {
    let lines = recognize(
        &scene(),
        &fonts(),
        &Options {
            tesseract: false,
            ..Options::default()
        },
    );
    let found: Vec<(&str, f32)> = lines.iter().map(|l| (l.text.as_str(), l.scale)).collect();
    assert_eq!(found.len(), 2, "{lines:#?}");
    assert_eq!(found[0].0, "Hello World!");
    assert!((found[0].1 - 3.0).abs() < 0.05, "{lines:#?}");
    assert_eq!(found[1].0, "Hello");
    assert!((found[1].1 - 5.0).abs() < 0.05, "{lines:#?}");
    assert!(
        lines
            .iter()
            .all(|l| l.engine == Engine::Glyphs && l.confidence > 0.95)
    );
    // The first line's box covers its glyphs: pen 20, 12 glyph cells high at most.
    assert_eq!(lines[0].x, 20);
    assert!(lines[0].y <= 50 - 21 && lines[0].y + lines[0].height >= 50);
}

#[test]
fn a_region_limits_reading_and_keeps_image_coordinates() {
    let region = Rect::new(30, 90, 200, 60);
    let options = Options {
        region: Some(region),
        tesseract: false,
        ..Options::default()
    };
    let lines = recognize(&scene(), &fonts(), &options);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert_eq!(lines[0].text, "Hello");
    assert_eq!(lines[0].x, 40);
}

#[test]
fn an_empty_region_reads_nothing() {
    let options = Options {
        region: Some(Rect::new(1000, 1000, 10, 10)),
        ..Options::default()
    };
    assert!(recognize(&scene(), &fonts(), &options).is_empty());
}

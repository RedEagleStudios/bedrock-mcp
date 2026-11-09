//! Accuracy of the recognizer on real captures:
//! `cargo run --release -p ocr --example fixtures [-- --lines] [--tesseract] [--region x,y,w,h]`.
//!
//! Fixtures come from `$OCR_FIXTURES` (default `~/.cache/bedrock-mcp/ocr-fixtures`):
//! PNG captures plus `expected.json` mapping each file name to strings on screen.

#[path = "../tests/common/mod.rs"]
mod common;

use ocr::{Options, Rect};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let show_lines = args.iter().any(|a| a == "--lines");
    let options = Options {
        tesseract: args.iter().any(|a| a == "--tesseract"),
        region: args
            .iter()
            .position(|a| a == "--region")
            .and_then(|at| args.get(at + 1))
            .and_then(|spec| {
                let v: Vec<u32> = spec.split(',').filter_map(|n| n.parse().ok()).collect();
                (v.len() == 4).then(|| Rect::new(v[0], v[1], v[2], v[3]))
            }),
        ..Options::default()
    };
    let (fonts, fixtures) = match common::load() {
        Ok(loaded) => loaded,
        Err(why) => {
            eprintln!("{why}");
            std::process::exit(2);
        }
    };
    println!("fonts: {}", fonts.names().join(", "));
    println!(
        "{:<12} {:>7} {:>8} {:>8}  missing",
        "fixture", "found", "CER", "ms"
    );
    let (mut found, mut expected, mut errors, mut chars) = (0, 0, 0, 0);
    for fixture in &fixtures {
        let score = common::score(fixture, &fonts, &options);
        let missing: Vec<&str> = fixture
            .expected
            .iter()
            .zip(&score.found)
            .filter(|(_, f)| !**f)
            .map(|(w, _)| w.as_str())
            .collect();
        println!(
            "{:<12} {:>7} {:>7.1}% {:>8.0}  {:?}",
            fixture.name,
            format!("{}/{}", score.found_count(), fixture.expected.len()),
            score.cer() * 100.0,
            score.elapsed.as_secs_f64() * 1000.0,
            missing
        );
        if show_lines {
            for line in &score.lines {
                println!(
                    "    {:>5},{:<5} {:>4}x{:<3} s={:<5.2} {:<5.2} {:<10} {:<22} {:?}",
                    line.x,
                    line.y,
                    line.width,
                    line.height,
                    line.scale,
                    line.confidence,
                    format!("{:?}", line.engine),
                    line.font,
                    line.text
                );
            }
        }
        found += score.found_count();
        expected += fixture.expected.len();
        errors += score.errors;
        chars += score.chars;
    }
    println!(
        "{:<12} {:>7} {:>7.1}%",
        "total",
        format!("{found}/{expected}"),
        errors as f64 * 100.0 / chars.max(1) as f64
    );
}

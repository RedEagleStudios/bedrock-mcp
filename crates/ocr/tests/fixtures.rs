//! Recognition of real game captures. The captures and the game fonts are not in the
//! repository; without them the tests print which one is missing and pass.

mod common;

use ocr::{Options, Rect};

#[test]
fn reads_the_visible_strings_of_every_capture() {
    let (fonts, fixtures) = match common::load() {
        Ok(loaded) => loaded,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let occluded = common::occluded();
    for fixture in &fixtures {
        let score = common::score(fixture, &fonts, &Options::default());
        for (want, found) in fixture.expected.iter().zip(&score.found) {
            assert!(
                *found || occluded.contains(want),
                "{}: {want:?} not read; lines: {:#?}",
                fixture.name,
                score.lines
            );
        }
    }
}

#[test]
fn a_region_reads_only_its_lines() {
    let (fonts, fixtures) = match common::load() {
        Ok(loaded) => loaded,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let Some(chat) = fixtures.iter().find(|f| f.name == "chat") else {
        eprintln!(
            "skipped: missing fixture chat.png in {}",
            common::fixture_dir().display()
        );
        return;
    };
    // The bottom-left chat area of the 2522x1360 capture.
    let region = Rect::new(0, 1100, 1300, 260);
    let options = Options {
        region: Some(region),
        ..Options::default()
    };
    let lines = ocr::recognize(&chat.image, &fonts, &options);
    let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    assert!(
        texts
            .iter()
            .any(|text| text.ends_with("> hello OCR test 123")),
        "{texts:?}"
    );
    assert!(
        lines
            .iter()
            .all(|l| l.y >= region.y && l.x + l.width <= region.x + region.width),
        "{lines:#?}"
    );
}

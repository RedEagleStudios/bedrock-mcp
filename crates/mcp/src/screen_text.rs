//! On-screen text read from captures with the `ocr` crate, matched against the game's own
//! pixel fonts. Coordinates in and out are logical window pixels, like pointer input.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use ocr::{Engine, Fonts, Image, Line, Options, Rect};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result, bail};

/// A rectangle in logical window pixels.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Region {
    pub fn parse(value: &Value) -> Result<Self> {
        let region: Self = serde_json::from_value(value.clone()).map_err(|error| {
            Error::msg(format!(
                "region must be {{x, y, width, height}} in logical window pixels: {error}"
            ))
        })?;
        let Self {
            x,
            y,
            width,
            height,
        } = region;
        if !(x >= 0.0 && y >= 0.0 && width > 0.0 && height > 0.0) {
            bail!("region needs x, y >= 0 and a positive width and height.");
        }
        Ok(region)
    }

    /// The region in image pixels, `factor` image pixels per logical pixel.
    fn to_image(self, factor: f64) -> Rect {
        let x0 = (self.x * factor).floor();
        let y0 = (self.y * factor).floor();
        let x1 = ((self.x + self.width) * factor).ceil();
        let y1 = ((self.y + self.height) * factor).ceil();
        Rect::new(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32)
    }
}

/// The pixel fonts of the last game directory read, kept for the life of the server:
/// rasterizing them takes longer than reading a frame.
static FONTS: Mutex<Option<(PathBuf, Arc<Fonts>)>> = Mutex::new(None);

/// The game's pixel fonts for the installation in `game_dir`.
pub fn fonts(game_dir: &Path) -> Result<Arc<Fonts>> {
    let mut cache = FONTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((dir, fonts)) = cache.as_ref()
        && dir == game_dir
    {
        return Ok(Arc::clone(fonts));
    }
    let fonts = Arc::new(Fonts::load(game_dir).map_err(|error| {
        Error::msg(format!(
            "OCR is unavailable: it reads text with the game's own fonts and found none ({error})."
        ))
    })?);
    *cache = Some((game_dir.to_owned(), Arc::clone(&fonts)));
    Ok(fonts)
}

/// One recognized line in logical window pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLine {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub confidence: f32,
    pub engine: Engine,
}

impl TextLine {
    fn from_line(line: Line, factor: f64) -> Self {
        let logical = |v: u32| (f64::from(v) / factor * 10.0).round() / 10.0;
        Self {
            text: line.text,
            x: logical(line.x),
            y: logical(line.y),
            width: logical(line.width),
            height: logical(line.height),
            confidence: line.confidence,
            engine: line.engine,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "text": self.text,
            "x": self.x,
            "y": self.y,
            "width": self.width,
            "height": self.height,
            "confidence": (f64::from(self.confidence) * 100.0).round() / 100.0,
            "engine": self.engine,
        })
    }
}

/// Text lines of a capture of a window `logical_width` logical pixels wide, top to
/// bottom, optionally only inside `region`.
pub fn read(
    image: &Image,
    logical_width: f64,
    region: Option<Region>,
    fonts: &Fonts,
) -> Result<Vec<TextLine>> {
    if image.width() == 0 || logical_width <= 0.0 {
        bail!("The capture is empty.");
    }
    let factor = f64::from(image.width()) / logical_width;
    let region = region.map(|region| region.to_image(factor));
    if region.is_some_and(|rect| rect.x >= image.width() || rect.y >= image.height()) {
        bail!(
            "region lies outside the {}x{} logical window.",
            logical_width.round(),
            (f64::from(image.height()) / factor).round()
        );
    }
    let options = Options {
        region,
        ..Options::default()
    };
    Ok(ocr::recognize(image, fonts, &options)
        .into_iter()
        .map(|line| TextLine::from_line(line, factor))
        .collect())
}

/// [`read`] a PNG capture into `{lines, ms}`.
pub fn read_png(
    path: &Path,
    logical_width: f64,
    region: Option<Region>,
    fonts: &Fonts,
) -> Result<Value> {
    let started = Instant::now();
    let image = Image::open(path).map_err(|error| Error::msg(error.to_string()))?;
    let lines = read(&image, logical_width, region, fonts)?;
    Ok(json!({
        "lines": lines.iter().map(TextLine::to_json).collect::<Vec<_>>(),
        "ms": started.elapsed().as_millis(),
    }))
}

/// The first line containing `needle`, ignoring case.
pub fn find<'a>(lines: &'a [TextLine], needle: &str) -> Option<&'a TextLine> {
    let needle = needle.to_lowercase();
    lines
        .iter()
        .find(|line| line.text.to_lowercase().contains(&needle))
}

/// How much of a label OCR has to read for a fuzzy match.
const FUZZY_SCORE: f64 = 0.7;

/// How a label was found on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// A line that is the label.
    Exact,
    /// A line that contains the label.
    Contains,
    /// One line, or several on the same row, that OCR read with letters missing or split
    /// (large OreUI fonts), such as "ESP WN" for "RESPAWN".
    Fuzzy,
}

/// The line that shows `label`, ignoring case: one that is the label, else (unless `exact`)
/// one that contains it, else one whose letters mostly match it.
pub fn locate(lines: &[TextLine], label: &str, exact: bool) -> Option<(TextLine, Match)> {
    let label = label.trim();
    if let Some(line) = lines
        .iter()
        .find(|line| line.text.trim().eq_ignore_ascii_case(label))
    {
        return Some((line.clone(), Match::Exact));
    }
    if exact {
        return None;
    }
    let lower = label.to_lowercase();
    if let Some(line) = lines
        .iter()
        .find(|line| line.text.to_lowercase().contains(&lower))
    {
        return Some((line.clone(), Match::Contains));
    }
    let wanted = letters(label);
    rows(lines)
        .into_iter()
        .map(|line| (letters_score(&letters(&line.text), &wanted), line))
        .filter(|(score, _)| *score >= FUZZY_SCORE)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, line)| (line, Match::Fuzzy))
}

fn letters(text: &str) -> Vec<char> {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The longest common subsequence of `read` and `wanted`, over the longer of the two.
fn letters_score(read: &[char], wanted: &[char]) -> f64 {
    if read.len() < 3 || wanted.is_empty() {
        return 0.0;
    }
    let mut previous = vec![0usize; wanted.len() + 1];
    for a in read {
        let mut current = vec![0usize; wanted.len() + 1];
        for (j, b) in wanted.iter().enumerate() {
            current[j + 1] = if a == b {
                previous[j] + 1
            } else {
                previous[j + 1].max(current[j])
            };
        }
        previous = current;
    }
    previous[wanted.len()] as f64 / read.len().max(wanted.len()) as f64
}

/// Every line, plus each run of neighbouring lines on one row joined into one, since OCR can
/// split a word where it misses a letter.
fn rows(lines: &[TextLine]) -> Vec<TextLine> {
    let mut candidates = lines.to_vec();
    let mut sorted: Vec<&TextLine> = lines.iter().collect();
    sorted.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
    for (start, first) in sorted.iter().enumerate() {
        let mut joined = (*first).clone();
        for next in &sorted[start + 1..] {
            let same_row = (next.y + next.height / 2.0 - joined.y - joined.height / 2.0).abs()
                < joined.height / 2.0;
            let gap = next.x - (joined.x + joined.width);
            if !same_row || gap < 0.0 || gap > joined.height * 2.0 {
                break;
            }
            let right = next.x + next.width;
            let bottom = (joined.y + joined.height).max(next.y + next.height);
            joined.text = format!("{} {}", joined.text, next.text);
            joined.y = joined.y.min(next.y);
            joined.width = right - joined.x;
            joined.height = bottom - joined.y;
            joined.confidence = joined.confidence.min(next.confidence);
            candidates.push(joined.clone());
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(text: &str, x: f64, y: f64, width: f64) -> TextLine {
        TextLine {
            text: text.into(),
            x,
            y,
            width,
            height: 20.0,
            confidence: 1.0,
            engine: Engine::Glyphs,
        }
    }

    #[test]
    fn labels_prefer_exact_lines_then_contained_text_then_split_rows() {
        let menu = [
            placed("Zombie Villager", 700.0, 100.0, 180.0),
            placed("Villager", 700.0, 140.0, 90.0),
            placed("ESP", 740.0, 584.0, 50.0),
            placed("WN", 815.0, 584.0, 40.0),
            placed("Game menu", 740.0, 664.0, 100.0),
        ];
        let (hit, how) = locate(&menu, "villager", false).unwrap();
        assert_eq!((hit.y, how), (140.0, Match::Exact));
        assert_eq!(locate(&menu, "Zombie", false).unwrap().1, Match::Contains);
        assert!(locate(&menu, "Zombie", true).is_none());
        let (hit, how) = locate(&menu, "Respawn", false).unwrap();
        assert_eq!(how, Match::Fuzzy);
        assert_eq!(
            (hit.text.as_str(), hit.x, hit.width),
            ("ESP WN", 740.0, 115.0)
        );
        assert!(locate(&menu, "Respawn", true).is_none());
        assert!(
            locate(&menu, "Settings", false).is_none(),
            "unrelated words must not match"
        );
    }

    #[test]
    fn regions_need_a_positive_size_and_scale_outward() {
        let region = Region::parse(&json!({"x": 10.5, "y": 0, "width": 20, "height": 5})).unwrap();
        assert_eq!(region.to_image(1.6), Rect::new(16, 0, 33, 8));
        assert!(Region::parse(&json!({"x": 0, "y": 0, "width": 0, "height": 5})).is_err());
        assert!(Region::parse(&json!({"x": -1, "y": 0, "width": 5, "height": 5})).is_err());
        assert!(Region::parse(&json!({"x": 0, "y": 0, "width": 5})).is_err());
        assert!(Region::parse(&json!({"x": 0, "y": 0, "width": 5, "height": 5, "z": 1})).is_err());
    }

    fn line(text: &str) -> TextLine {
        TextLine {
            text: text.into(),
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            confidence: 1.0,
            engine: Engine::Glyphs,
        }
    }

    #[test]
    fn find_matches_a_substring_ignoring_case() {
        let lines = [line("Settings"), line("Resume Game")];
        assert_eq!(
            find(&lines, "resume g").map(|l| l.text.as_str()),
            Some("Resume Game")
        );
        assert!(find(&lines, "Quit").is_none());
    }

    #[test]
    fn lines_report_logical_pixels() {
        let line = TextLine::from_line(
            Line {
                text: "Hi".into(),
                x: 16,
                y: 32,
                width: 160,
                height: 48,
                scale: 4.0,
                font: "Mojangles".into(),
                confidence: 0.987,
                engine: Engine::Glyphs,
            },
            1.6,
        );
        assert_eq!(
            line.to_json(),
            json!({"text": "Hi", "x": 10.0, "y": 20.0, "width": 100.0, "height": 30.0, "confidence": 0.99, "engine": "glyphs"})
        );
    }

    #[test]
    fn missing_fonts_are_reported_as_ocr_unavailable() {
        let dir = std::env::temp_dir().join(format!("bedrock-mcp-no-fonts-{}", std::process::id()));
        let error = fonts(&dir).expect_err("no fonts there").to_string();
        assert!(error.contains("OCR is unavailable"), "{error}");
    }

    #[test]
    fn a_region_outside_the_window_is_refused() {
        let image = Image::new(16, 16, vec![0; 16 * 16 * 4]).unwrap();
        let fonts = Fonts::default();
        let outside = Region {
            x: 20.0,
            y: 0.0,
            width: 5.0,
            height: 5.0,
        };
        assert!(read(&image, 8.0, Some(outside), &fonts).is_err());
        assert!(read(&image, 8.0, None, &fonts).unwrap().is_empty());
    }

    /// A real pause-menu capture (2522x1360 physical, Hyprland scale 1.6) with the game's
    /// fonts; skipped when either is missing.
    #[test]
    fn reads_a_capture_into_logical_pixels() {
        let root = crate::install::Install::from_env().profile;
        let game = root.parent().unwrap_or(&root).join("game");
        let capture = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".cache/bedrock-mcp/ocr-fixtures/pause.png");
        if !capture.is_file() {
            eprintln!("skipped: missing fixture {}", capture.display());
            return;
        }
        let fonts = match fonts(&game) {
            Ok(fonts) => fonts,
            Err(error) => {
                eprintln!("skipped: missing game fonts: {error}");
                return;
            }
        };
        let logical_width = 2522.0 / 1.6;
        let region = Region {
            x: 300.0,
            y: 250.0,
            width: 300.0,
            height: 80.0,
        };
        let reply = read_png(&capture, logical_width, Some(region), &fonts).unwrap();
        let lines = reply["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1, "{reply}");
        assert_eq!(lines[0]["text"], "Resume Game");
        assert_eq!(lines[0]["engine"], "glyphs");
        // Physical box 581,444 315x35 at scale 1.6.
        let x = lines[0]["x"].as_f64().unwrap();
        let y = lines[0]["y"].as_f64().unwrap();
        assert!(
            (x - 363.1).abs() < 2.0 && (y - 277.5).abs() < 2.0,
            "{reply}"
        );
    }
}

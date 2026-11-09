//! Real-capture fixtures (not in the repository) and their scoring, shared by the fixture
//! test and the `fixtures` example.

#![allow(dead_code)]

use std::{
    env,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use ocr::{Fonts, Image, Line, Options};

/// `$OCR_FIXTURES`, else `~/.cache/bedrock-mcp/ocr-fixtures`.
pub fn fixture_dir() -> PathBuf {
    env::var_os("OCR_FIXTURES")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache/bedrock-mcp/ocr-fixtures"))
}

fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

/// The game directory of the mcbe-gdk-linux installation: `$MCBE_GDK_ROOT` (else
/// `${XDG_DATA_HOME:-~/.local/share}/mcbe-gdk-linux`), then its `game-dir` pointer or `game`.
pub fn game_dir() -> PathBuf {
    let var = |name: &str| env::var_os(name).filter(|v| !v.is_empty());
    let root = var("MCBE_GDK_ROOT").map(PathBuf::from).unwrap_or_else(|| {
        var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("mcbe-gdk-linux")
    });
    std::fs::read_to_string(root.join("game-dir"))
        .ok()
        .map(|text| PathBuf::from(text.trim()))
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| root.join("game"))
}

pub struct Fixture {
    pub name: String,
    pub image: Image,
    pub expected: Vec<String>,
}

/// Strings listed under `_occluded` in `expected.json`: shown only partly in the captures,
/// overdrawn by the debug overlay or a desktop notification. Empty without the manifest.
pub fn occluded() -> Vec<String> {
    std::fs::read_to_string(fixture_dir().join("expected.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json.get("_occluded").cloned())
        .and_then(|list| serde_json::from_value(list).ok())
        .unwrap_or_default()
}

/// The game fonts and every fixture listed in `expected.json`, or why they are missing.
pub fn load() -> Result<(Fonts, Vec<Fixture>), String> {
    let game = game_dir();
    let fonts = Fonts::load(&game).map_err(|e| format!("missing game fonts: {e}"))?;
    let dir = fixture_dir();
    let manifest = dir.join("expected.json");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("missing fixture {}: {e}", manifest.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad {}: {e}", manifest.display()))?;
    let mut fixtures = Vec::new();
    for (file, lines) in json.as_object().into_iter().flatten() {
        if file.starts_with('_') {
            continue;
        }
        let Some(lines) = lines.as_array() else {
            continue;
        };
        let image = Image::open(&dir.join(file))
            .map_err(|e| format!("missing fixture {}: {e}", dir.join(file).display()))?;
        fixtures.push(Fixture {
            name: Path::new(file)
                .file_stem()
                .map_or_else(|| file.clone(), |s| s.to_string_lossy().into_owned()),
            image,
            expected: lines
                .iter()
                .filter_map(|l| l.as_str().map(str::to_owned))
                .collect(),
        });
    }
    Ok((fonts, fixtures))
}

/// Edit distance from `needle` to its best-matching substring of `hay`.
pub fn substring_distance(needle: &str, hay: &str) -> usize {
    let needle: Vec<char> = needle.chars().collect();
    let hay: Vec<char> = hay.chars().collect();
    // Row j: distance of needle[..j] ending at each hay position; free start in hay.
    let mut prev = vec![0usize; hay.len() + 1];
    for j in 1..=needle.len() {
        let mut row = vec![j; hay.len() + 1];
        for i in 1..=hay.len() {
            let substitute = prev[i - 1] + usize::from(needle[j - 1] != hay[i - 1]);
            row[i] = substitute.min(prev[i] + 1).min(row[i - 1] + 1);
        }
        prev = row;
    }
    prev.into_iter().min().unwrap_or(needle.len())
}

pub struct Score {
    pub found: Vec<bool>,
    pub errors: usize,
    pub chars: usize,
    pub elapsed: Duration,
    pub lines: Vec<Line>,
}

impl Score {
    pub fn found_count(&self) -> usize {
        self.found.iter().filter(|&&f| f).count()
    }
    pub fn cer(&self) -> f64 {
        self.errors as f64 / self.chars.max(1) as f64
    }
}

/// Recognize a fixture and score it: which expected strings appear exactly in one line,
/// and the character error rate of each expected string against its closest line.
pub fn score(fixture: &Fixture, fonts: &Fonts, options: &Options) -> Score {
    let started = Instant::now();
    let lines = ocr::recognize(&fixture.image, fonts, options);
    let elapsed = started.elapsed();
    let found = fixture
        .expected
        .iter()
        .map(|want| lines.iter().any(|l| l.text.contains(want.as_str())))
        .collect();
    let errors = fixture
        .expected
        .iter()
        .map(|want| {
            lines
                .iter()
                .map(|l| substring_distance(want, &l.text))
                .min()
                .unwrap_or(want.chars().count())
        })
        .sum();
    let chars = fixture.expected.iter().map(|w| w.chars().count()).sum();
    Score {
        found,
        errors,
        chars,
        elapsed,
        lines,
    }
}

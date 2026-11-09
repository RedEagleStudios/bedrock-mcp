//! Glyph matching: explain each sampled segment with font glyphs on a common baseline,
//! then join them into text with spaces from the font's advances.

use std::collections::HashMap;

use crate::{
    font::{Font, Fonts, Glyph},
    grid::{Sampled, Segment},
};

/// Extra cost per placed glyph, so one wide glyph beats two narrow ones that fit equally.
const PLACE_COST: f32 = 0.5;
/// Extra cost per column left unexplained.
const SKIP_COST: f32 = 0.3;
/// A single glyph explaining a segment this well (cost / union ink) needs no split search.
const GOOD_FIT: f32 = 0.1;
/// Below this confidence a placement is reported as U+FFFD.
const MIN_GLYPH_CONFIDENCE: f32 = 0.5;
/// Segments wider than this many cells are not split into glyphs (noise, not text).
const MAX_SPLIT_COLS: usize = 48;
/// A word at either end of a line whose glyphs miss more than this share of their ink is
/// dropped as an icon or texture next to the text.
const MAX_EDGE_WORD_MISFIT: f32 = 0.3;

/// One glyph placed in a segment.
#[derive(Clone, Copy, Debug)]
struct Placement {
    /// Index into `Fonts::fonts` (set by the caller of the per-font searches).
    font: usize,
    glyph: usize,
    /// Left edge of its first ink column in plane pixels.
    x: f32,
    cost: f32,
    union: f32,
}

/// Recognized text of one run of glyphs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Reading {
    pub text: String,
    pub font: String,
    pub confidence: f32,
    /// Ink extent in plane pixels.
    pub x0: f32,
    pub x1: f32,
}

/// Mismatch of glyph `g` placed at column `col` of `seg` with baseline row `base`:
/// `(sum |template − coverage|, sum max(template, coverage))` over the glyph's columns.
/// Starting from an empty template (cost and union are the columns' coverage), each ink
/// cell on coverage `v` changes the cost by `1 − 2v` and the union by `1 − v`; ink outside
/// the segment meets no coverage.
fn place_cost(seg: &Segment, rows: usize, g: &Glyph, col: usize, base: i32) -> (f32, f32) {
    let empty: f32 = seg.columns[col.min(seg.cols)..(col + g.width).min(seg.cols)]
        .iter()
        .sum();
    let (mut cost, mut union) = (empty, empty);
    let first = base + g.top;
    for &(gr, gc) in &g.ink {
        let r = first + i32::from(gr);
        let c = col + usize::from(gc);
        let v = if (0..rows as i32).contains(&r) && c < seg.cols {
            seg.cell(r as usize, c)
        } else {
            0.0
        };
        cost += 1.0 - 2.0 * v;
        union += 1.0 - v;
    }
    (cost, union)
}

/// The best single glyph for the whole segment (allowing one column of misfit).
fn best_single(
    seg: &Segment,
    rows: usize,
    font: &Font,
    base: i32,
    s: f32,
) -> Option<Vec<Placement>> {
    let mut best: Option<(f32, Placement)> = None;
    for (index, g) in font.glyphs.iter().enumerate() {
        if g.width.abs_diff(seg.cols) > 1 {
            continue;
        }
        let offsets: &[usize] = if g.width + 1 == seg.cols {
            &[0, 1]
        } else {
            &[0]
        };
        for &col in offsets {
            let (mut cost, mut union) = place_cost(seg, rows, g, col, base);
            for c in (0..col).chain(col + g.width..seg.cols) {
                let ink = seg.columns[c];
                cost += ink;
                union += ink;
            }
            let score = cost / union.max(1.0);
            if best.as_ref().is_none_or(|(s, _)| score < *s) {
                best = Some((
                    score,
                    Placement {
                        font: 0,
                        glyph: index,
                        x: seg.x + col as f32 * s,
                        cost,
                        union,
                    },
                ));
            }
        }
    }
    best.map(|(_, p)| vec![p])
}

/// Explain the segment with a sequence of glyphs and skipped columns (dynamic programming
/// over columns), for touching glyphs and glyphs whose width was mismeasured.
fn best_split(
    seg: &Segment,
    rows: usize,
    font: &Font,
    base: i32,
    s: f32,
) -> Option<Vec<Placement>> {
    let n = seg.cols;
    let mut cost = vec![f32::INFINITY; n + 1];
    let mut union = vec![0f32; n + 1];
    let mut from: Vec<Option<(usize, Option<Placement>)>> = vec![None; n + 1];
    cost[0] = 0.0;
    for c in 0..n {
        if !cost[c].is_finite() {
            continue;
        }
        let ink = seg.columns[c];
        if cost[c] + ink + SKIP_COST < cost[c + 1] {
            cost[c + 1] = cost[c] + ink + SKIP_COST;
            union[c + 1] = union[c] + ink;
            from[c + 1] = Some((c, None));
        }
        for (index, g) in font.glyphs.iter().enumerate() {
            let end = c + g.width;
            if end > n {
                continue;
            }
            let (pc, pu) = place_cost(seg, rows, g, c, base);
            if cost[c] + pc + PLACE_COST < cost[end] {
                cost[end] = cost[c] + pc + PLACE_COST;
                union[end] = union[c] + pu;
                from[end] = Some((
                    c,
                    Some(Placement {
                        font: 0,
                        glyph: index,
                        x: seg.x + c as f32 * s,
                        cost: pc,
                        union: pu,
                    }),
                ));
            }
        }
    }
    let mut placed = Vec::new();
    let mut at = n;
    while at > 0 {
        let (prev, placement) = from[at]?;
        match placement {
            Some(p) => placed.push(p),
            None => {
                // Skipped ink counts against the neighbouring glyph's confidence.
                let ink = seg.columns[prev];
                if let Some(last) = placed.last_mut() {
                    last.cost += ink;
                    last.union += ink;
                }
            }
        }
        at = prev;
    }
    placed.reverse();
    Some(placed)
}

fn score(placements: &[Placement]) -> (f32, f32) {
    placements
        .iter()
        .fold((0.0, 0.0), |(c, u), p| (c + p.cost, u + p.union))
}

/// Candidate baselines: the most common rows just below segment ink.
fn baselines(sampled: &Sampled) -> Vec<i32> {
    let mut counts: HashMap<i32, usize> = HashMap::new();
    for seg in &sampled.segments {
        if let Some((_, last)) = seg.rows_inked(sampled.rows) {
            *counts.entry(last as i32 + 1).or_default() += seg.cols;
        }
    }
    let mut ranked: Vec<(i32, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    ranked.into_iter().take(3).map(|(b, _)| b).collect()
}

/// The best explanation of one segment: the best single glyph of any font (the line's
/// font wins near-ties), else a split into several glyphs of the line's font.
fn explain(
    seg: &Segment,
    rows: usize,
    fonts: &Fonts,
    line_font: usize,
    base: i32,
    s: f32,
) -> Option<Vec<Placement>> {
    let mut single: Option<(f32, Vec<Placement>)> = None;
    for (fi, font) in fonts.fonts.iter().enumerate() {
        let Some(mut p) = best_single(seg, rows, font, base, s) else {
            continue;
        };
        p.iter_mut().for_each(|p| p.font = fi);
        let (c, u) = score(&p);
        let ratio = c / u.max(1.0) + if fi == line_font { 0.0 } else { 0.02 };
        if single.as_ref().is_none_or(|(best, _)| ratio < *best) {
            single = Some((ratio, p));
        }
    }
    if single.as_ref().is_some_and(|(ratio, _)| *ratio <= GOOD_FIT) || seg.cols > MAX_SPLIT_COLS {
        return single.map(|(_, p)| p);
    }
    let split = best_split(seg, rows, &fonts.fonts[line_font], base, s).map(|mut p| {
        p.iter_mut().for_each(|p| p.font = line_font);
        p
    });
    match (single, split) {
        (Some((ratio, a)), Some(b)) => {
            let (cb, ub) = score(&b);
            let penalty = PLACE_COST * (b.len() as f32 - 1.0);
            Some(if (cb + penalty) / ub.max(1.0) < ratio {
                b
            } else {
                a
            })
        }
        (a, b) => a.map(|(_, p)| p).or(b),
    }
}

/// Match a sampled line and split it into readings at wide gaps.
pub(crate) fn read(sampled: &Sampled, fonts: &Fonts) -> Vec<Reading> {
    let rows = sampled.rows;
    let s = sampled.scale;
    // Pick the font and baseline under which single glyphs explain the segments best.
    let mut choice: Option<(f32, usize, i32)> = None;
    for base in baselines(sampled) {
        for (fi, font) in fonts.fonts.iter().enumerate() {
            let (mut cost, mut union) = (0f32, 0f32);
            for seg in &sampled.segments {
                match best_single(seg, rows, font, base, s) {
                    Some(p) => {
                        let (c, u) = score(&p);
                        cost += c;
                        union += u;
                    }
                    None => {
                        let ink: f32 = seg.cells.iter().sum();
                        cost += ink;
                        union += ink;
                    }
                }
            }
            let ratio = cost / union.max(1.0);
            if choice.is_none_or(|(best, _, _)| ratio < best) {
                choice = Some((ratio, fi, base));
            }
        }
    }
    let Some((_, fi, base)) = choice else {
        return Vec::new();
    };
    let placements: Vec<Placement> = sampled
        .segments
        .iter()
        .filter_map(|seg| explain(seg, rows, fonts, fi, base, s))
        .flatten()
        .collect();
    assemble(&placements, fonts, fi, s)
}

/// Join placements into readings: spaces from pen-position gaps, a new reading at a gap
/// of two spaces or more (separate labels, or an icon next to its label).
fn assemble(placements: &[Placement], fonts: &Fonts, line_font: usize, s: f32) -> Vec<Reading> {
    let glyph = |p: &Placement| &fonts.fonts[p.font].glyphs[p.glyph];
    let mut readings = Vec::new();
    let mut current: Vec<(char, Placement)> = Vec::new();
    let flush = |current: &mut Vec<(char, Placement)>, readings: &mut Vec<Reading>| {
        // Unknown marks at either end are noise next to the text, not part of it, and so is
        // a poorly matched edge word (an icon read as glyphs) when other words remain.
        let noise = |c: char| c == '\u{fffd}' || c == ' ';
        let unknown = |word: &[(char, Placement)]| {
            let (cost, union) = word
                .iter()
                .fold((0f32, 0f32), |(c, u), (_, p)| (c + p.cost, u + p.union));
            word.iter().any(|(c, _)| *c == '\u{fffd}') || cost > MAX_EDGE_WORD_MISFIT * union
        };
        loop {
            while current.first().is_some_and(|(c, _)| noise(*c)) {
                current.remove(0);
            }
            while current.last().is_some_and(|(c, _)| noise(*c)) {
                current.pop();
            }
            let first_end = current.iter().position(|(c, _)| *c == ' ');
            let last_start = current.iter().rposition(|(c, _)| *c == ' ');
            match (first_end, last_start) {
                (Some(end), _) if unknown(&current[..end]) => {
                    current.drain(..end);
                }
                (_, Some(start)) if unknown(&current[start + 1..]) => current.truncate(start),
                _ => break,
            }
        }
        let glyphs: Vec<&Placement> = current
            .iter()
            .filter(|(c, _)| *c != ' ')
            .map(|(_, p)| p)
            .collect();
        if let (Some(first), Some(last)) = (glyphs.first(), glyphs.last()) {
            let (cost, union) = glyphs
                .iter()
                .fold((0f32, 0f32), |(c, u), p| (c + p.cost, u + p.union));
            readings.push(Reading {
                text: current.iter().map(|(c, _)| *c).collect(),
                font: fonts.fonts[line_font].name.clone(),
                confidence: (1.0 - cost / union.max(1.0)).clamp(0.0, 1.0),
                x0: first.x,
                x1: last.x + glyph(last).width as f32 * s,
            });
        }
        current.clear();
    };
    let space = fonts.fonts[line_font].space as f32;
    let mut previous: Option<&Placement> = None;
    for p in placements {
        let g = glyph(p);
        if let Some(prev) = previous {
            let pg = glyph(prev);
            let expected = prev.x - pg.lead as f32 * s + pg.advance as f32 * s;
            let pen = p.x - g.lead as f32 * s;
            let extra = (pen - expected) / s;
            if extra >= 1.6 * space {
                flush(&mut current, &mut readings);
            } else if extra >= (0.45 * space).max(1.5) {
                current.push((' ', *p));
            }
        }
        let confidence = 1.0 - p.cost / p.union.max(1.0);
        let ch = if confidence >= MIN_GLYPH_CONFIDENCE {
            g.ch
        } else {
            '\u{fffd}'
        };
        current.push((ch, *p));
        previous = Some(p);
    }
    flush(&mut current, &mut readings);
    readings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::tests::glyph;

    fn font() -> Font {
        Font {
            name: "test".into(),
            glyphs: vec![
                glyph('H', &["#..#", "#..#", "####", "#..#", "#..#"], 0),
                glyph('i', &["#", ".", "#", "#", "#"], 0),
                glyph('.', &["#"], 0),
            ],
            space: 3,
        }
    }

    fn segment(glyph: &Glyph, rows: usize, base: i32, x: f32) -> Segment {
        let mut cells = vec![0.0; rows * glyph.width];
        for r in 0..glyph.height {
            for c in 0..glyph.width {
                if glyph.bit(r, c) {
                    cells[(base + glyph.top) as usize * glyph.width + r * glyph.width + c] = 1.0;
                }
            }
        }
        Segment::new(x, glyph.width, cells)
    }

    #[test]
    fn reads_glyphs_and_spaces_from_pen_positions() {
        let font = font();
        let s = 2.0;
        let (h, i, dot) = (&font.glyphs[0], &font.glyphs[1], &font.glyphs[2]);
        // "Hi" then a space (3 cells) then "." — pens advance width + 1.
        let segments = vec![
            segment(h, 7, 6, 0.0),
            segment(i, 7, 6, 5.0 * s),
            segment(dot, 7, 6, (7.0 + 3.0) * s),
        ];
        let sampled = Sampled {
            scale: s,
            rows: 7,
            segments,
        };
        let fonts = Fonts {
            fonts: vec![font.clone()],
        };
        let readings = read(&sampled, &fonts);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].text, "Hi .");
        assert!(readings[0].confidence > 0.99);
    }
}

//! A text line's font-pixel grid: its cell size (measured from ink run lengths, which are
//! whole multiples of it), its phase, and the line resampled to one value per cell.

use std::f32::consts::TAU;

use crate::detect::{Box2, Candidate, Detection, Plane, Polarity};

const MIN_SCALE: f32 = 1.5;

/// One glyph-sized run of inked columns, resampled to cells.
#[derive(Clone, Debug)]
pub(crate) struct Segment {
    /// Left edge of cell column 0 in plane pixels.
    pub x: f32,
    pub cols: usize,
    /// Ink coverage per cell (0..1), `rows × cols` row-major.
    pub cells: Vec<f32>,
    /// Total coverage of each column.
    pub columns: Vec<f32>,
}

impl Segment {
    pub fn new(x: f32, cols: usize, cells: Vec<f32>) -> Self {
        let mut columns = vec![0f32; cols];
        for row in cells.chunks_exact(cols) {
            for (sum, v) in columns.iter_mut().zip(row) {
                *sum += v;
            }
        }
        Self {
            x,
            cols,
            cells,
            columns,
        }
    }

    pub fn cell(&self, row: usize, col: usize) -> f32 {
        self.cells[row * self.cols + col]
    }

    /// Ink rows (first, last) of the segment, if any cell is inked.
    pub fn rows_inked(&self, rows: usize) -> Option<(usize, usize)> {
        let inked = |r: usize| (0..self.cols).any(|c| self.cell(r, c) >= 0.5);
        let first = (0..rows).find(|&r| inked(r))?;
        let last = (0..rows).rev().find(|&r| inked(r))?;
        Some((first, last))
    }
}

/// A line on its grid.
#[derive(Clone, Debug)]
pub(crate) struct Sampled {
    /// Plane pixels per font pixel.
    pub scale: f32,
    pub rows: usize,
    pub segments: Vec<Segment>,
}

/// Ink coverage of a padded line box.
struct Coverage {
    bounds: Box2,
    values: Vec<f32>,
}

impl Coverage {
    fn width(&self) -> usize {
        self.bounds.width()
    }
    fn height(&self) -> usize {
        self.bounds.height()
    }
    fn at(&self, x: usize, y: usize) -> f32 {
        self.values[y * self.width() + x]
    }
}

fn percentile(values: &mut [u8], fraction: f32) -> Option<u8> {
    if values.is_empty() {
        return None;
    }
    let at = ((values.len() - 1) as f32 * fraction) as usize;
    Some(*values.select_nth_unstable(at).1)
}

/// Coverage in `0..1` from the text colour to the background, limited to the
/// candidate's components and the pixel ring around them (their anti-aliased edges).
fn coverage(plane: &Plane, detection: &Detection, candidate: &Candidate) -> Option<Coverage> {
    let b = candidate.bounds;
    let pad = (b.height() / 6).max(2);
    let bounds = Box2 {
        x0: b.x0.saturating_sub(pad),
        y0: b.y0.saturating_sub(pad),
        x1: (b.x1 + pad).min(plane.width),
        y1: (b.y1 + pad).min(plane.height),
    };
    let ours = |label: u32| label != 0 && candidate.labels.contains(&label);
    let (w, h) = (bounds.width(), bounds.height());
    let mut mine = vec![false; w * h];
    let (mut ink, mut background) = (Vec::new(), Vec::new());
    for y in 0..h {
        for x in 0..w {
            let at = (bounds.y0 + y) * plane.width + bounds.x0 + x;
            let luma = plane.luma[at];
            if ours(detection.labels[at]) {
                mine[y * w + x] = true;
                ink.push(luma);
            } else if detection.labels[at] == 0 {
                background.push(luma);
            }
        }
    }
    let (ink_level, bg_level) = match candidate.polarity {
        Polarity::Light => (
            percentile(&mut ink, 0.75)?,
            percentile(&mut background, 0.5)?,
        ),
        Polarity::Dark => (
            percentile(&mut ink, 0.25)?,
            percentile(&mut background, 0.5)?,
        ),
    };
    let (ink_level, bg_level) = (f32::from(ink_level), f32::from(bg_level));
    if (ink_level - bg_level).abs() < 24.0 {
        return None;
    }
    let mut values = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let near = (y.saturating_sub(1)..(y + 2).min(h))
                .any(|ny| (x.saturating_sub(1)..(x + 2).min(w)).any(|nx| mine[ny * w + nx]));
            if near {
                let luma = f32::from(plane.luma[(bounds.y0 + y) * plane.width + bounds.x0 + x]);
                values[y * w + x] = ((luma - bg_level) / (ink_level - bg_level)).clamp(0.0, 1.0);
            }
        }
    }
    Some(Coverage { bounds, values })
}

/// Ink runs along one axis: sub-pixel lengths and edge positions.
#[derive(Default)]
struct Runs {
    lengths: Vec<f32>,
    edges: Vec<f32>,
}

/// Runs of `get(i) >= 0.5` over `0..n`, measured with partial coverage at both ends;
/// runs touching the ends are dropped (they may be cut).
fn runs_1d(n: usize, get: impl Fn(usize) -> f32, runs: &mut Runs, offset: f32) {
    let mut i = 0;
    while i < n {
        if get(i) < 0.5 {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && get(i) >= 0.5 {
            i += 1;
        }
        if start == 0 || i == n {
            continue;
        }
        let before = get(start - 1);
        let after = get(i);
        let first = get(start);
        let last = get(i - 1);
        let lead = start as f32 + (1.0 - first) - before;
        let trail = i as f32 + after - (1.0 - last);
        runs.lengths.push(trail - lead);
        runs.edges.push(lead + offset);
        runs.edges.push(trail + offset);
    }
}

/// `values` rounded to 1/32 pixel as `(value, count)`: far fewer terms for the period
/// searches, which evaluate them at thousands of candidate periods.
fn histogram(values: &[f32]) -> Vec<(f32, f32)> {
    let mut steps: Vec<i32> = values.iter().map(|v| (v * 32.0).round() as i32).collect();
    steps.sort_unstable();
    let mut out: Vec<(f32, f32)> = Vec::new();
    for step in steps {
        let value = step as f32 / 32.0;
        match out.last_mut() {
            Some((last, count)) if *last == value => *count += 1.0,
            _ => out.push((value, 1.0)),
        }
    }
    out
}

fn total(values: &[(f32, f32)]) -> f32 {
    values.iter().map(|&(_, n)| n).sum::<f32>().max(1.0)
}

/// Coherence of `values` with period `s`: `(|mean of e^{2πi·v/s}|, phase in 0..s)`.
fn coherence(values: &[(f32, f32)], s: f32) -> (f32, f32) {
    let (mut c, mut sn) = (0f32, 0f32);
    for &(v, n) in values {
        let (sin, cos) = (TAU * v / s).sin_cos();
        c += n * cos;
        sn += n * sin;
    }
    let phase = (sn.atan2(c) / TAU * s).rem_euclid(s);
    ((c * c + sn * sn).sqrt() / total(values), phase)
}

/// How well `values` are whole multiples of `s`: the mean of `cos(2π·v/s)` (1 when all are).
fn alignment(values: &[(f32, f32)], s: f32) -> f32 {
    let sum: f32 = values.iter().map(|&(v, n)| n * (TAU * v / s).cos()).sum();
    sum / total(values)
}

/// The cell size of a line's run lengths: the largest period of which (nearly) every
/// length is a whole multiple, else (text drawn by the OreUI renderer, whose glyph edges
/// are smoothed and slightly emboldened) a period every length fits up to one common
/// extra width.
fn cell_size(lengths: &[f32], max: f32) -> Option<f32> {
    if lengths.len() < 3 || max < MIN_SCALE {
        return None;
    }
    let lengths = histogram(lengths);
    period(&lengths, max, alignment).or_else(|| period(&lengths, max, emboldened))
}

/// How well `values` are whole multiples of `s` plus one shared extra width of at most a
/// quarter cell, spanning at least two different multiples (one repeated length fits
/// every period).
fn emboldened(values: &[(f32, f32)], s: f32) -> f32 {
    let (r, phase) = coherence(values, s);
    let extra = if phase > s / 2.0 { phase - s } else { phase };
    if extra.abs() > 0.25 * s {
        return 0.0;
    }
    let multiple = |v: f32| ((v - extra) / s).round();
    let first = multiple(values[0].0);
    if values.iter().all(|&(v, _)| multiple(v) == first) {
        return 0.0;
    }
    r
}

/// The largest strong peak of `fit` over cell sizes up to `max`.
fn period(lengths: &[(f32, f32)], max: f32, fit: fn(&[(f32, f32)], f32) -> f32) -> Option<f32> {
    let steps = ((max - MIN_SCALE) / 0.01) as usize + 1;
    let scores: Vec<(f32, f32)> = (0..steps)
        .map(|k| {
            let s = MIN_SCALE + k as f32 * 0.01;
            (s, fit(lengths, s))
        })
        .collect();
    let best = scores.iter().map(|&(_, r)| r).fold(0f32, f32::max);
    if best < 0.6 {
        return None;
    }
    // Every divisor of the true size fits as well; take the largest strong peak.
    let peak = (0..scores.len()).rev().find(|&k| {
        let r = scores[k].1;
        r >= 0.88 * best
            && (k == 0 || scores[k - 1].1 <= r)
            && (k + 1 == scores.len() || scores[k + 1].1 <= r)
    })?;
    let coarse = scores[peak].0;
    (0..=40)
        .map(|k| coarse - 0.01 + k as f32 * 0.0005)
        .max_by(|&a, &b| fit(lengths, a).total_cmp(&fit(lengths, b)))
}

/// Mean coverage of the pixels whose centres lie in the middle of a cell.
fn sample_cell(cov: &Coverage, cx: f32, cy: f32, s: f32) -> f32 {
    let half = (0.3 * s).max(0.5);
    let range = |c: f32, n: usize| {
        let lo = (c - half).round().max(0.0) as usize;
        let hi = ((c + half).round() as usize).min(n);
        if lo < hi {
            lo..hi
        } else {
            let at = (c.floor().max(0.0) as usize).min(n.saturating_sub(1));
            at..at + 1
        }
    };
    let (xs, ys) = (range(cx, cov.width()), range(cy, cov.height()));
    let mut sum = 0f32;
    let mut n = 0;
    for y in ys {
        for x in xs.clone() {
            sum += cov.at(x, y);
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { sum / n as f32 }
}

/// Measure the candidate's grid and resample it into glyph segments.
pub(crate) fn sample(
    plane: &Plane,
    detection: &Detection,
    candidate: &Candidate,
) -> Option<Sampled> {
    let cov = coverage(plane, detection, candidate)?;
    let (w, h) = (cov.width(), cov.height());
    let mut horizontal = Runs::default();
    let mut vertical = Runs::default();
    for y in 0..h {
        runs_1d(w, |x| cov.at(x, y), &mut horizontal, 0.0);
    }
    for x in 0..w {
        runs_1d(h, |y| cov.at(x, y), &mut vertical, 0.0);
    }
    let lengths: Vec<f32> = horizontal
        .lengths
        .iter()
        .chain(&vertical.lengths)
        .copied()
        .collect();
    let s = cell_size(&lengths, candidate.bounds.height() as f32 / 3.0)?;
    let oy = coherence(&histogram(&vertical.edges), s).1;
    // Row 0 starts at the last grid line at or above the box top.
    let y_start = oy - (oy / s).ceil() * s;
    let rows = ((h as f32 - y_start) / s).ceil() as usize;

    // Inked columns, split at gaps of at least half a cell.
    let inked: Vec<bool> = (0..w)
        .map(|x| (0..h).any(|y| cov.at(x, y) >= 0.5))
        .collect();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut x = 0;
    while x < w {
        if !inked[x] {
            x += 1;
            continue;
        }
        let start = x;
        while x < w && inked[x] {
            x += 1;
        }
        match spans.last_mut() {
            Some(last) if ((start - last.1) as f32) < 0.5 * s => last.1 = x,
            _ => spans.push((start, x)),
        }
    }

    let mut segments = Vec::new();
    for (x0, x1) in spans {
        // The window keeps one uninked column on each side for the partial edge pixels.
        let lo = x0.saturating_sub(1);
        let hi = (x1 + 1).min(w);
        let mut runs = Runs::default();
        for y in 0..h {
            runs_1d(
                hi - lo + 2,
                |i| {
                    let x = (lo + i).wrapping_sub(1);
                    if (lo..hi).contains(&x) {
                        cov.at(x, y)
                    } else {
                        0.0
                    }
                },
                &mut runs,
                lo as f32 - 1.0,
            );
        }
        let (Some(left), Some(right)) = (
            runs.edges.iter().copied().reduce(f32::min),
            runs.edges.iter().copied().reduce(f32::max),
        ) else {
            continue;
        };
        let ox = coherence(&histogram(&runs.edges), s).1;
        let x_start = ox + ((left - ox) / s).round() * s;
        let cols = ((right - x_start) / s).round().max(1.0) as usize;
        let mut cells = Vec::with_capacity(rows * cols);
        for r in 0..rows {
            for c in 0..cols {
                let cx = x_start + (c as f32 + 0.5) * s;
                let cy = y_start + (r as f32 + 0.5) * s;
                cells.push(sample_cell(&cov, cx, cy, s));
            }
        }
        segments.push(Segment::new(x_start + cov.bounds.x0 as f32, cols, cells));
    }
    if segments.is_empty() {
        return None;
    }
    Some(Sampled {
        scale: s,
        rows,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_size_is_the_largest_common_period_of_run_lengths() {
        let lengths = [5.0, 10.0, 5.0, 15.0, 5.0, 20.0, 10.0];
        let s = cell_size(&lengths, 12.0).unwrap();
        assert!((s - 5.0).abs() < 0.02, "{s}");
        let fractional = [2.8, 5.6, 8.4, 2.8, 2.8, 8.4];
        let s = cell_size(&fractional, 10.0).unwrap();
        assert!((s - 2.8).abs() < 0.02, "{s}");
    }

    #[test]
    fn runs_measure_partial_edges() {
        let values = [0.0, 0.25, 1.0, 1.0, 1.0, 0.5, 0.0];
        let mut runs = Runs::default();
        runs_1d(values.len(), |i| values[i], &mut runs, 0.0);
        assert_eq!(runs.lengths.len(), 1);
        assert!((runs.lengths[0] - 3.75).abs() < 1e-5, "{:?}", runs.lengths);
        assert!((runs.edges[0] - 1.75).abs() < 1e-5, "{:?}", runs.edges);
    }
}

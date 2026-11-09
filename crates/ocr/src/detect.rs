//! Candidate text lines: pixels clearly brighter (or darker) than their surroundings,
//! grouped into connected components and then into rows of similar height.

/// Luma difference from the local mean that marks a text pixel.
const CONTRAST: i32 = 40;

/// A luma plane.
pub(crate) struct Plane {
    pub width: usize,
    pub height: usize,
    pub luma: Vec<u8>,
}

/// Bright text on a darker background, or dark text on a lighter one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Polarity {
    Light,
    Dark,
}

/// Inclusive-exclusive pixel box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Box2 {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Box2 {
    pub fn width(&self) -> usize {
        self.x1 - self.x0
    }
    pub fn height(&self) -> usize {
        self.y1 - self.y0
    }
    pub fn union(&self, other: &Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
    pub fn intersection_area(&self, other: &Self) -> usize {
        let w = self.x1.min(other.x1).saturating_sub(self.x0.max(other.x0));
        let h = self.y1.min(other.y1).saturating_sub(self.y0.max(other.y0));
        w * h
    }
    pub fn area(&self) -> usize {
        self.width() * self.height()
    }
}

/// A connected component of text-like pixels.
#[derive(Clone, Debug)]
pub(crate) struct Component {
    pub bounds: Box2,
    pub pixels: usize,
    pub polarity: Polarity,
}

/// A row of components that may be one line of text.
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub bounds: Box2,
    pub polarity: Polarity,
    /// Component labels (the values of `Detection::labels` on its pixels).
    pub labels: Vec<u32>,
}

pub(crate) struct Detection {
    /// Per pixel: 0 for none, else 1 + the component index.
    pub labels: Vec<u32>,
    pub candidates: Vec<Candidate>,
}

/// Sum of luma over `[0,x)×[0,y)` at `(y·(w+1) + x)`.
fn integral(width: usize, height: usize, value: impl Fn(usize) -> u32) -> Vec<u32> {
    let w = width + 1;
    let mut sums = vec![0u32; w * (height + 1)];
    for y in 0..height {
        let mut row = 0u32;
        for x in 0..width {
            row += value(y * width + x);
            sums[(y + 1) * w + x + 1] = sums[y * w + x + 1] + row;
        }
    }
    sums
}

/// Per pixel 1 (light text), 2 (dark text) or 0, against the mean of a square window.
/// A first pass marks pixels far from the plain window mean; the second compares against
/// the mean of the unmarked pixels only, so the background next to bright (dark) text is
/// not itself marked dark (bright) by contrast with the text.
fn classify(plane: &Plane, radius: usize) -> Vec<u8> {
    let (width, height) = (plane.width, plane.height);
    let w = width + 1;
    let window = |sums: &[u32], x: usize, y: usize| {
        let (y0, y1) = (y.saturating_sub(radius), (y + radius + 1).min(height));
        let (x0, x1) = (x.saturating_sub(radius), (x + radius + 1).min(width));
        i64::from(sums[y1 * w + x1] + sums[y0 * w + x0] - sums[y0 * w + x1] - sums[y1 * w + x0])
    };
    let class = |luma: i64, sum: i64, count: i64| {
        let diff = luma * count - sum;
        if diff >= i64::from(CONTRAST) * count {
            1
        } else if diff <= -i64::from(CONTRAST) * count {
            2
        } else {
            0
        }
    };
    let sums = integral(width, height, |i| u32::from(plane.luma[i]));
    let ones = integral(width, height, |_| 1);
    let mut first = vec![0u8; width * height];
    for y in 0..height {
        for x in 0..width {
            let luma = i64::from(plane.luma[y * width + x]);
            first[y * width + x] = class(luma, window(&sums, x, y), window(&ones, x, y));
        }
    }
    let background = |i: usize| u32::from(first[i] == 0);
    let bg_sums = integral(width, height, |i| u32::from(plane.luma[i]) * background(i));
    let bg_count = integral(width, height, background);
    let mut out = vec![0u8; width * height];
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let count = window(&bg_count, x, y);
            out[i] = if count == 0 {
                first[i]
            } else {
                class(i64::from(plane.luma[i]), window(&bg_sums, x, y), count)
            };
        }
    }
    out
}

/// 8-connected components of equal non-zero classes.
fn components(plane: &Plane, classes: &[u8]) -> (Vec<u32>, Vec<Component>) {
    let (w, h) = (plane.width, plane.height);
    let mut labels = vec![0u32; w * h];
    let mut found = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        let class = classes[start];
        if class == 0 || labels[start] != 0 {
            continue;
        }
        let label = found.len() as u32 + 1;
        labels[start] = label;
        stack.push(start);
        let mut bounds = Box2 {
            x0: start % w,
            y0: start / w,
            x1: start % w + 1,
            y1: start / w + 1,
        };
        let mut pixels = 0;
        while let Some(at) = stack.pop() {
            pixels += 1;
            let (x, y) = (at % w, at / w);
            bounds.x0 = bounds.x0.min(x);
            bounds.x1 = bounds.x1.max(x + 1);
            bounds.y0 = bounds.y0.min(y);
            bounds.y1 = bounds.y1.max(y + 1);
            for ny in y.saturating_sub(1)..(y + 2).min(h) {
                for nx in x.saturating_sub(1)..(x + 2).min(w) {
                    let next = ny * w + nx;
                    if classes[next] == class && labels[next] == 0 {
                        labels[next] = label;
                        stack.push(next);
                    }
                }
            }
        }
        found.push(Component {
            bounds,
            pixels,
            polarity: if class == 1 {
                Polarity::Light
            } else {
                Polarity::Dark
            },
        });
    }
    (labels, found)
}

/// Whether a component can be (part of) a glyph.
fn glyph_like(component: &Component, max_height: usize) -> bool {
    let b = component.bounds;
    let (w, h) = (b.width(), b.height());
    let fill = component.pixels as f32 / b.area() as f32;
    component.pixels >= 3
        && h <= max_height
        && w <= max_height * 3
        && !(w >= 12 * h.max(1) && w > 40)
        && !(h >= 12 * w.max(1) && h > 40)
        && !(fill > 0.92 && w.min(h) > 16)
        // Button frames and outlines: large and nearly empty.
        && !(fill < 0.15 && w.min(h) > 30)
}

/// Whether two components sit on one text row, close enough to be one line.
fn same_line(a: &Box2, b: &Box2) -> bool {
    let (ha, hb) = (a.height() as f32, b.height() as f32);
    let (big, small) = (ha.max(hb), ha.min(hb));
    let overlap = a.y1.min(b.y1) as f32 - a.y0.max(b.y0) as f32;
    if overlap < 0.5 * small {
        return false;
    }
    if small >= 0.6 * big && overlap < 0.6 * small {
        return false;
    }
    let gap = b.x0 as f32 - a.x1 as f32;
    gap <= 1.3 * big && gap >= -0.5 * big
}

/// Find text-line candidates in `plane`. `reference_height` (the full screenshot height)
/// sizes the background window so a cropped region classifies like the whole screen.
pub(crate) fn detect(plane: &Plane, reference_height: usize) -> Detection {
    let radius = (reference_height / 60).clamp(16, 40);
    let classes = classify(plane, radius);
    let (labels, components) = components(plane, &classes);
    let max_height = (reference_height / 6).max(24);
    let mut order: Vec<usize> = (0..components.len())
        .filter(|&i| glyph_like(&components[i], max_height))
        .collect();
    order.sort_by_key(|&i| components[i].bounds.x0);
    let mut parent: Vec<usize> = (0..components.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for (n, &i) in order.iter().enumerate() {
        let a = components[i].bounds;
        let reach = a.x1 + (1.3 * a.height().max(1) as f32) as usize + max_height;
        for &j in &order[n + 1..] {
            let b = components[j].bounds;
            if b.x0 > reach {
                break;
            }
            if components[j].polarity == components[i].polarity
                && (same_line(&a, &b) || same_line(&b, &a))
            {
                let (ri, rj) = (root(&mut parent, i), root(&mut parent, j));
                parent[ri] = rj;
            }
        }
    }
    let mut groups: std::collections::HashMap<usize, Vec<usize>> = Default::default();
    for &i in &order {
        let r = root(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    let mut candidates: Vec<Candidate> = groups
        .into_values()
        .flat_map(|members| split_rows(members, &components))
        .filter_map(|members| {
            let bounds = members
                .iter()
                .map(|&i| components[i].bounds)
                .reduce(|a, b| a.union(&b))?;
            Some(Candidate {
                bounds,
                polarity: components[members[0]].polarity,
                labels: members.iter().map(|&i| i as u32 + 1).collect(),
            })
        })
        .collect();
    merge_satellites(&mut candidates);
    candidates.retain(|c| {
        let pixels: usize = c
            .labels
            .iter()
            .map(|&l| components[l as usize - 1].pixels)
            .sum();
        c.bounds.height() >= 5 && pixels >= 12
    });
    candidates.sort_by_key(|c| (c.bounds.y0, c.bounds.x0));
    Detection { labels, candidates }
}

/// Split a group whose components stack in several rows (one tall or misplaced component
/// can chain neighbouring lines together) at gaps between component centres.
fn split_rows(mut members: Vec<usize>, components: &[Component]) -> Vec<Vec<usize>> {
    let mut heights: Vec<usize> = members
        .iter()
        .map(|&i| components[i].bounds.height())
        .collect();
    let median = *heights.select_nth_unstable(members.len() / 2).1 as f32;
    let (top, bottom) = members.iter().fold((usize::MAX, 0), |(t, b), &i| {
        (
            t.min(components[i].bounds.y0),
            b.max(components[i].bounds.y1),
        )
    });
    if ((bottom - top) as f32) <= 1.8 * median {
        return vec![members];
    }
    let centre = |i: usize| (components[i].bounds.y0 + components[i].bounds.y1) as f32 / 2.0;
    members.sort_by(|&a, &b| centre(a).total_cmp(&centre(b)));
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut last = f32::NEG_INFINITY;
    for i in members {
        let c = centre(i);
        if c - last > 0.5 * median || rows.is_empty() {
            rows.push(Vec::new());
        }
        rows.last_mut().expect("pushed above").push(i);
        last = c;
    }
    rows
}

/// Whether a small group `mark` belongs to the line `line`: a dot just above it (i, j over
/// lowercase words), or a glyph part too short to overlap its neighbours enough (the stem
/// of a "!" after lowercase letters).
fn satellite_of(mark: &Box2, line: &Box2) -> bool {
    let lh = line.height() as f32;
    let gap = (mark.x0 as f32 - line.x1 as f32).max(line.x0 as f32 - mark.x1 as f32);
    if mark.y1 <= line.y0 {
        return mark.height() as f32 <= 0.4 * lh
            && gap <= 0.0
            && (line.y0 - mark.y1) as f32 <= 0.35 * lh;
    }
    mark.height() <= line.height()
        && mark.y0 as f32 >= line.y0 as f32 - 0.4 * lh
        && mark.y1 as f32 <= line.y1 as f32 + 0.15 * lh
        && gap <= 0.6 * lh
}

/// Fold groups of one or two components into the line they belong to (see [`satellite_of`]).
fn merge_satellites(candidates: &mut Vec<Candidate>) {
    candidates.sort_by_key(|c| std::cmp::Reverse(c.bounds.height()));
    let mut i = candidates.len();
    while i > 0 {
        i -= 1;
        if candidates[i].labels.len() > 2 {
            continue;
        }
        let mark = candidates[i].bounds;
        let host = (0..candidates.len())
            .filter(|&j| {
                j != i
                    && candidates[j].polarity == candidates[i].polarity
                    && candidates[j].labels.len() > candidates[i].labels.len()
                    && satellite_of(&mark, &candidates[j].bounds)
            })
            .min_by_key(|&j| {
                let line = candidates[j].bounds;
                line.x0.saturating_sub(mark.x1) + mark.x0.saturating_sub(line.x1)
            });
        if let Some(j) = host {
            let mark = candidates.swap_remove(i);
            // swap_remove moved the last candidate to i; the host index stays valid
            // unless the host was that last one.
            let j = if j == candidates.len() { i } else { j };
            candidates[j].bounds = candidates[j].bounds.union(&mark.bounds);
            candidates[j].labels.extend(mark.labels);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(width: usize, height: usize, paint: &[(usize, usize, usize, usize)]) -> Plane {
        let mut luma = vec![30u8; width * height];
        for &(x0, y0, x1, y1) in paint {
            for y in y0..y1 {
                for x in x0..x1 {
                    luma[y * width + x] = 230;
                }
            }
        }
        Plane {
            width,
            height,
            luma,
        }
    }

    #[test]
    fn neighbouring_blobs_of_one_height_form_one_line_and_far_rows_another() {
        let plane = plane(
            200,
            120,
            &[
                (10, 10, 16, 31),
                (22, 10, 28, 31),
                (34, 17, 40, 31),
                (10, 80, 16, 101),
            ],
        );
        let detection = detect(&plane, 120);
        let boxes: Vec<Box2> = detection.candidates.iter().map(|c| c.bounds).collect();
        assert_eq!(
            boxes,
            [
                Box2 {
                    x0: 10,
                    y0: 10,
                    x1: 40,
                    y1: 31
                },
                Box2 {
                    x0: 10,
                    y0: 80,
                    x1: 16,
                    y1: 101
                }
            ]
        );
        assert!(
            detection
                .candidates
                .iter()
                .all(|c| c.polarity == Polarity::Light)
        );
    }
}

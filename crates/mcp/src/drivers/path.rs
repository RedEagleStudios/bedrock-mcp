//! Grid pathfinding for walking: A* over the block cells a player can stand in. A cell is a
//! standing spot when its feet and head blocks are open and the block below carries the
//! player. From a spot the player walks to the eight neighbours on the same level (diagonals
//! only past two open sides, so it never cuts a corner), jumps up a one-block ledge, or drops
//! down at most [`MAX_DROP`] blocks. Lava never counts as open; water does only when the
//! caller allows it, at a higher cost, so a dry way wins when there is one.
//!
//! The search is bounded three ways: a square of [`Limits::radius`] blocks around the start,
//! [`Limits::max_nodes`] expanded spots and a deadline. Blocks come from a [`Blocks`] source
//! in aligned boxes and are cached in a [`Grid`], so a replan reads only what changed.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
    time::Instant,
};

use crate::error::Result;

/// The deepest drop a path takes, in blocks: a fall of 3 does no damage.
pub const MAX_DROP: i64 = 3;
/// Extra cost of a spot whose feet are in water, so paths stay dry when they can.
const WATER_COST: f64 = 6.0;
/// Extra cost of jumping up a ledge: a jump is slower than a step.
const JUMP_COST: f64 = 1.0;
/// Extra cost per block dropped.
const DROP_COST: f64 = 0.5;
/// How far apart the checks along a straight walk are, in blocks.
const LINE_STEP: f64 = 0.25;
/// Half a player's width, in blocks: a straight walk needs room on both sides of its line.
const HALF_WIDTH: f64 = 0.3;

/// What a block cell is to a walking player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    /// Air or a block without collision (grass, flowers, torches).
    Open,
    /// A full obstacle that also carries a player standing on it.
    Solid,
    /// A fence or wall: an obstacle 1.5 blocks high, so nobody jumps onto it.
    Tall,
    Water,
    Lava,
    /// Not loaded, or the source could not say: never entered.
    Unknown,
}

impl Cell {
    /// The cell's letter in a block grid: `.` open, `#` solid, `t` tall, `w` water, `l` lava,
    /// `?` unknown.
    pub fn from_letter(letter: char) -> Self {
        match letter {
            '.' => Self::Open,
            '#' => Self::Solid,
            't' => Self::Tall,
            'w' => Self::Water,
            'l' => Self::Lava,
            _ => Self::Unknown,
        }
    }

    /// Whether the player's body fits in the cell.
    fn holds_body(self, water: bool) -> bool {
        self == Self::Open || (water && self == Self::Water)
    }
}

/// Where cells come from: the Bridge pack's block grid or test commands.
pub trait Blocks {
    /// The cells of the box at `origin` spanning `size`, x fastest, then z, then y.
    fn load(&mut self, origin: [i64; 3], size: [i64; 3]) -> Result<Vec<Cell>>;
    /// The size of the aligned boxes it loads at once.
    fn chunk(&self) -> [i64; 3];
}

/// Cells read so far, loaded box by box on first use.
pub struct Grid<B> {
    blocks: B,
    cells: HashMap<[i64; 3], Cell>,
    /// How many boxes were loaded, for the reply.
    pub loads: usize,
}

impl<B: Blocks> Grid<B> {
    pub fn new(blocks: B) -> Self {
        Self {
            blocks,
            cells: HashMap::new(),
            loads: 0,
        }
    }

    pub fn cell(&mut self, at: [i64; 3]) -> Result<Cell> {
        if let Some(cell) = self.cells.get(&at) {
            return Ok(*cell);
        }
        let size = self.blocks.chunk();
        let origin = [0, 1, 2].map(|axis| at[axis].div_euclid(size[axis]) * size[axis]);
        let cells = self.blocks.load(origin, size)?;
        self.loads += 1;
        let mut index = 0;
        for y in 0..size[1] {
            for z in 0..size[2] {
                for x in 0..size[0] {
                    let cell = cells.get(index).copied().unwrap_or(Cell::Unknown);
                    self.cells
                        .insert([origin[0] + x, origin[1] + y, origin[2] + z], cell);
                    index += 1;
                }
            }
        }
        Ok(self.cells.get(&at).copied().unwrap_or(Cell::Unknown))
    }

    /// Forget the cells within `radius` blocks of `at`, so the next plan reads them again:
    /// something there differs from what the last plan assumed.
    pub fn forget_near(&mut self, at: [i64; 3], radius: i64) {
        self.cells
            .retain(|cell, _| (0..3).any(|axis| (cell[axis] - at[axis]).abs() > radius));
    }
}

/// How far a search may go.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Half the side of the square around the start the path stays in.
    pub radius: i64,
    /// The most spots expanded.
    pub max_nodes: usize,
    pub deadline: Instant,
    /// Whether water counts as open (with a higher cost).
    pub water: bool,
}

/// Which bound ended a search before it found the goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Radius,
    Nodes,
    Time,
}

impl Bound {
    pub fn name(self) -> &'static str {
        match self {
            Self::Radius => "radius",
            Self::Nodes => "nodes",
            Self::Time => "time",
        }
    }
}

/// A search's result: the spots from the start to the goal, or to the spot nearest the goal
/// when it found no way (`reached` false).
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub cells: Vec<[i64; 3]>,
    pub reached: bool,
    /// The bound that stopped the search, if one did; `None` with `reached` false means every
    /// spot within the bounds was tried: the goal is walled off.
    pub bound: Option<Bound>,
    pub expanded: usize,
}

impl Plan {
    /// The path's length in blocks along its spots.
    pub fn length(&self) -> f64 {
        self.cells
            .windows(2)
            .map(|pair| {
                let [a, b] = [pair[0], pair[1]];
                let d = [0, 1, 2].map(|axis| (b[axis] - a[axis]) as f64);
                (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
            })
            .sum()
    }
}

/// The goal: a point the path must end within `radius` blocks of, horizontally, and within
/// one block vertically.
#[derive(Clone, Copy, Debug)]
pub struct Goal {
    pub at: [f64; 3],
    pub radius: f64,
}

impl Goal {
    fn flat_gap(&self, cell: [i64; 3]) -> f64 {
        let dx = cell[0] as f64 + 0.5 - self.at[0];
        let dz = cell[2] as f64 + 0.5 - self.at[2];
        dx.hypot(dz)
    }

    fn reached(&self, cell: [i64; 3]) -> bool {
        self.flat_gap(cell) <= self.radius && (cell[1] - self.at[1].floor() as i64).abs() <= 1
    }

    /// A lower bound of the cost left: the octile distance to the arrival circle.
    fn estimate(&self, cell: [i64; 3]) -> f64 {
        let dx = (cell[0] as f64 + 0.5 - self.at[0]).abs();
        let dz = (cell[2] as f64 + 0.5 - self.at[2]).abs();
        let (long, short) = if dx > dz { (dx, dz) } else { (dz, dx) };
        (long + (std::f64::consts::SQRT_2 - 1.0) * short - self.radius).max(0.0)
    }
}

#[derive(PartialEq)]
struct Open {
    score: f64,
    cost: f64,
    at: [i64; 3],
}

impl Eq for Open {}

impl Ord for Open {
    /// The lowest score first in a max-heap; ties go to the deeper spot.
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then(self.cost.total_cmp(&other.cost))
    }
}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

const SIDES: [[i64; 2]; 4] = [[1, 0], [-1, 0], [0, 1], [0, -1]];
const CORNERS: [[i64; 2]; 4] = [[1, 1], [1, -1], [-1, 1], [-1, -1]];

fn offset(at: [i64; 3], dx: i64, dy: i64, dz: i64) -> [i64; 3] {
    [at[0] + dx, at[1] + dy, at[2] + dz]
}

/// The extra cost of standing at `at`, or `None` when the player cannot stand there.
pub fn stand<B: Blocks>(grid: &mut Grid<B>, at: [i64; 3], water: bool) -> Result<Option<f64>> {
    let feet = grid.cell(at)?;
    if !feet.holds_body(water) || !grid.cell(offset(at, 0, 1, 0))?.holds_body(water) {
        return Ok(None);
    }
    let below = grid.cell(offset(at, 0, -1, 0))?;
    let carried = below == Cell::Solid || (water && below == Cell::Water);
    if !carried {
        return Ok(None);
    }
    let wet = feet == Cell::Water || below == Cell::Water;
    Ok(Some(if wet { WATER_COST } else { 0.0 }))
}

/// Whether the player's body passes through the column at `at` (feet and head).
fn clear<B: Blocks>(grid: &mut Grid<B>, at: [i64; 3], water: bool) -> Result<bool> {
    Ok(grid.cell(at)?.holds_body(water) && grid.cell(offset(at, 0, 1, 0))?.holds_body(water))
}

/// The spots reachable in one move from `at`, with each move's cost.
fn moves<B: Blocks>(grid: &mut Grid<B>, at: [i64; 3], water: bool) -> Result<Vec<([i64; 3], f64)>> {
    let mut found = Vec::new();
    for [dx, dz] in SIDES {
        let level = offset(at, dx, 0, dz);
        if let Some(extra) = stand(grid, level, water)? {
            found.push((level, 1.0 + extra));
            continue;
        }
        // A ledge: the head needs room to jump from here.
        let up = offset(at, dx, 1, dz);
        if grid.cell(offset(at, 0, 2, 0))?.holds_body(water)
            && let Some(extra) = stand(grid, up, water)?
        {
            found.push((up, 1.0 + JUMP_COST + extra));
            continue;
        }
        // A drop: walk off over the open column and land on the first floor below.
        if !clear(grid, level, water)? {
            continue;
        }
        for depth in 1..=MAX_DROP {
            let down = offset(at, dx, -depth, dz);
            if let Some(extra) = stand(grid, down, water)? {
                found.push((down, 1.0 + DROP_COST * depth as f64 + extra));
                break;
            }
            if !grid.cell(down)?.holds_body(water) {
                break;
            }
        }
    }
    for [dx, dz] in CORNERS {
        let corner = offset(at, dx, 0, dz);
        if clear(grid, offset(at, dx, 0, 0), water)?
            && clear(grid, offset(at, 0, 0, dz), water)?
            && let Some(extra) = stand(grid, corner, water)?
        {
            found.push((corner, std::f64::consts::SQRT_2 + extra));
        }
    }
    Ok(found)
}

/// Find a way from the standing spot `start` to `goal` within `limits`.
pub fn find<B: Blocks>(
    grid: &mut Grid<B>,
    start: [i64; 3],
    goal: Goal,
    limits: Limits,
) -> Result<Plan> {
    let mut open = BinaryHeap::new();
    let mut came: HashMap<[i64; 3], ([i64; 3], f64)> = HashMap::new();
    came.insert(start, (start, 0.0));
    open.push(Open {
        score: goal.estimate(start),
        cost: 0.0,
        at: start,
    });
    let mut nearest = (goal.estimate(start), goal.flat_gap(start), start);
    let mut bound = None;
    let mut expanded = 0;
    let finish = |at: [i64; 3],
                  reached: bool,
                  bound,
                  expanded,
                  came: &HashMap<[i64; 3], ([i64; 3], f64)>| {
        let mut cells = vec![at];
        let mut current = at;
        while current != start {
            current = came[&current].0;
            cells.push(current);
        }
        cells.reverse();
        Plan {
            cells,
            reached,
            bound,
            expanded,
        }
    };
    while let Some(Open { cost, at, .. }) = open.pop() {
        if cost > came[&at].1 {
            continue;
        }
        if goal.reached(at) {
            return Ok(finish(at, true, None, expanded, &came));
        }
        if expanded >= limits.max_nodes {
            bound = Some(Bound::Nodes);
            break;
        }
        if Instant::now() >= limits.deadline {
            bound = Some(Bound::Time);
            break;
        }
        expanded += 1;
        let here = (goal.estimate(at), goal.flat_gap(at), at);
        if (here.0, here.1) < (nearest.0, nearest.1) {
            nearest = here;
        }
        for (next, step) in moves(grid, at, limits.water)? {
            if (next[0] - start[0]).abs() > limits.radius
                || (next[2] - start[2]).abs() > limits.radius
            {
                bound.get_or_insert(Bound::Radius);
                continue;
            }
            let cost = cost + step;
            if came.get(&next).is_some_and(|(_, known)| *known <= cost) {
                continue;
            }
            came.insert(next, (at, cost));
            open.push(Open {
                score: cost + goal.estimate(next),
                cost,
                at: next,
            });
        }
    }
    Ok(finish(nearest.2, false, bound, expanded, &came))
}

/// The points a walk heads for, start left out. From each point it takes the farthest later
/// spot on the same level that a straight walk reaches over standing spots only, so the walk
/// cuts the grid's zigzags as a player would; each change of level is a point of its own.
pub fn waypoints<B: Blocks>(
    grid: &mut Grid<B>,
    cells: &[[i64; 3]],
    water: bool,
) -> Result<Vec<[i64; 3]>> {
    let mut points = Vec::new();
    let mut anchor = 0;
    while anchor + 1 < cells.len() {
        let mut best = anchor + 1;
        for next in anchor + 2..cells.len() {
            if cells[next][1] != cells[anchor][1]
                || !straight(grid, cells[anchor], cells[next], water)?
            {
                break;
            }
            best = next;
        }
        points.push(cells[best]);
        anchor = best;
    }
    Ok(points)
}

/// Whether a player walks straight from the middle of `from` to the middle of `to`, on the
/// same level, with every block under its body's width a standing spot.
fn straight<B: Blocks>(
    grid: &mut Grid<B>,
    from: [i64; 3],
    to: [i64; 3],
    water: bool,
) -> Result<bool> {
    let (dx, dz) = ((to[0] - from[0]) as f64, (to[2] - from[2]) as f64);
    let steps = (dx.hypot(dz) / LINE_STEP).ceil().max(1.0) as usize;
    for step in 0..=steps {
        let along = step as f64 / steps as f64;
        let (x, z) = (
            from[0] as f64 + 0.5 + dx * along,
            from[2] as f64 + 0.5 + dz * along,
        );
        for (side_x, side_z) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            let cell = [
                (x + side_x * HALF_WIDTH).floor() as i64,
                from[1],
                (z + side_z * HALF_WIDTH).floor() as i64,
            ];
            if stand(grid, cell, water)?.is_none() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A map of layers from y 0 up, each a list of rows along z of letters along x; cells
    /// outside it are open above the layers and solid below.
    struct Fake {
        layers: Vec<Vec<&'static str>>,
        loads: usize,
    }

    impl Fake {
        fn at(&self, [x, y, z]: [i64; 3]) -> Cell {
            if y < 0 {
                return Cell::Solid;
            }
            let Some(layer) = self.layers.get(y as usize) else {
                return Cell::Open;
            };
            let row = usize::try_from(z).ok().and_then(|z| layer.get(z));
            let letter =
                row.and_then(|row| usize::try_from(x).ok().and_then(|x| row.chars().nth(x)));
            letter.map_or(Cell::Unknown, Cell::from_letter)
        }
    }

    impl Blocks for Fake {
        fn load(&mut self, origin: [i64; 3], size: [i64; 3]) -> Result<Vec<Cell>> {
            self.loads += 1;
            let mut cells = Vec::new();
            for y in 0..size[1] {
                for z in 0..size[2] {
                    for x in 0..size[0] {
                        cells.push(self.at([origin[0] + x, origin[1] + y, origin[2] + z]));
                    }
                }
            }
            Ok(cells)
        }

        fn chunk(&self) -> [i64; 3] {
            [4, 4, 4]
        }
    }

    /// A flat 12 x 12 floor at y 0, with `walls` (letters) on the layers above it.
    fn world(above: Vec<Vec<&'static str>>) -> Grid<Fake> {
        let floor = vec!["############"; 12];
        let mut layers = vec![floor];
        layers.extend(above);
        Grid::new(Fake { layers, loads: 0 })
    }

    fn limits() -> Limits {
        Limits {
            radius: 32,
            max_nodes: 5000,
            deadline: Instant::now() + Duration::from_secs(5),
            water: false,
        }
    }

    fn goal(x: f64, y: f64, z: f64) -> Goal {
        Goal {
            at: [x, y, z],
            radius: 0.5,
        }
    }

    const OPEN: &str = "............";

    #[test]
    fn a_straight_way_is_one_waypoint() {
        let mut grid = world(vec![vec![OPEN; 12], vec![OPEN; 12]]);
        let plan = find(&mut grid, [1, 1, 1], goal(1.5, 1.0, 9.5), limits()).unwrap();
        assert!(plan.reached);
        let route = waypoints(&mut grid, &plan.cells, false).unwrap();
        assert_eq!(route, vec![[1, 1, 9]]);
        assert_eq!(plan.length(), 8.0);
    }

    #[test]
    fn a_wall_is_walked_around_without_cutting_its_corner() {
        // A wall along x from 0 to 8 at z 5, two blocks high.
        let wall = "#########...";
        let mut rows = vec![OPEN; 12];
        rows[5] = wall;
        let mut grid = world(vec![rows.clone(), rows]);
        let plan = find(&mut grid, [2, 1, 2], goal(2.5, 1.0, 8.5), limits()).unwrap();
        assert!(plan.reached, "{plan:?}");
        assert!(plan.cells.iter().all(|cell| cell[2] != 5 || cell[0] >= 9));
        // Never diagonal past the wall's end block.
        for pair in plan.cells.windows(2) {
            let [a, b] = [pair[0], pair[1]];
            assert!(!(a[0] == 9 && a[2] == 4 && b[0] == 8 && b[2] == 5));
        }
        // The route cuts the grid's zigzags: a few straight legs, each clear of the wall.
        let route = waypoints(&mut grid, &plan.cells, false).unwrap();
        assert!((2..=3).contains(&route.len()), "{route:?}");
        let mut from = [2, 1, 2];
        for point in &route {
            assert!(
                straight(&mut grid, from, *point, false).unwrap(),
                "{route:?}"
            );
            from = *point;
        }
    }

    #[test]
    fn a_one_block_ledge_is_climbed_and_two_are_not() {
        // A step at z >= 6 one block up; with a second block at z >= 6 it is a wall.
        let mut step = vec![OPEN; 12];
        for row in step.iter_mut().skip(6) {
            *row = "############";
        }
        let mut grid = world(vec![step.clone(), vec![OPEN; 12], vec![OPEN; 12]]);
        let plan = find(&mut grid, [3, 1, 2], goal(3.5, 2.0, 9.5), limits()).unwrap();
        assert!(plan.reached, "{plan:?}");
        assert_eq!(plan.cells.last(), Some(&[3, 2, 9]));
        let route = waypoints(&mut grid, &plan.cells, false).unwrap();
        assert!(route.contains(&[3, 2, 6]), "{route:?}");

        let mut grid = world(vec![step.clone(), step, vec![OPEN; 12]]);
        let plan = find(&mut grid, [3, 1, 2], goal(3.5, 3.0, 9.5), limits()).unwrap();
        assert!(!plan.reached);
        assert_eq!(plan.bound, None);
    }

    #[test]
    fn a_walled_off_goal_ends_at_the_nearest_spot() {
        // The goal at (6, 6) inside a closed box of walls three high.
        let mut rows = vec![OPEN; 12];
        rows[4] = "....###.....";
        rows[5] = "....#.#.....";
        rows[6] = "....#.#.....";
        rows[7] = "....###.....";
        let mut grid = world(vec![rows.clone(), rows.clone(), rows]);
        let plan = find(&mut grid, [1, 1, 1], goal(5.5, 1.0, 5.5), limits()).unwrap();
        assert!(!plan.reached);
        assert_eq!(plan.bound, None);
        let end = *plan.cells.last().unwrap();
        assert!(goal(5.5, 1.0, 5.5).flat_gap(end) <= 2.0, "{end:?}");
    }

    #[test]
    fn deep_drops_and_lava_are_avoided() {
        // A pit 4 deep across x 0..8 at z 5 and 6; the way around at x 9..11 has lava.
        let mut layers: Vec<Vec<&'static str>> = Vec::new();
        let mut floor = vec!["############"; 12];
        floor[5] = "............";
        floor[6] = "............";
        // Raise the walking level to y 4, so the pit is 4 deep.
        for _ in 0..4 {
            layers.push(floor.clone());
        }
        let mut top = vec![OPEN; 12];
        top[5] = ".........lll";
        top[6] = ".........lll";
        layers.push(top);
        layers.push(vec![OPEN; 12]);
        let mut grid = Grid::new(Fake { layers, loads: 0 });
        let plan = find(&mut grid, [2, 4, 2], goal(2.5, 4.0, 9.5), limits()).unwrap();
        assert!(!plan.reached, "{plan:?}");
        assert!(plan.cells.iter().all(|cell| cell[1] >= 4));

        // Three deep is taken: a floor at y 0 under a pit through y 1 to 3.
        let mut shallow: Vec<Vec<&'static str>> = vec![vec!["############"; 12]];
        let mut floor = vec!["############"; 12];
        floor[5] = "............";
        for _ in 0..3 {
            shallow.push(floor.clone());
        }
        shallow.push(vec![OPEN; 12]);
        let mut grid = Grid::new(Fake {
            layers: shallow,
            loads: 0,
        });
        let plan = find(&mut grid, [2, 4, 7], goal(2.5, 1.0, 5.5), limits()).unwrap();
        assert!(plan.reached, "{plan:?}");
        assert_eq!(plan.cells.last(), Some(&[2, 1, 5]));
    }

    #[test]
    fn water_is_crossed_only_when_allowed_and_no_dry_way_exists() {
        // A water channel across the whole map at z 5.
        let mut floor = vec!["############"; 12];
        floor[5] = "wwwwwwwwwwww";
        let mut feet = vec![OPEN; 12];
        feet[5] = "wwwwwwwwwwww";
        let layers = vec![floor, feet, vec![OPEN; 12]];
        let mut grid = Grid::new(Fake {
            layers: layers.clone(),
            loads: 0,
        });
        let plan = find(&mut grid, [2, 1, 2], goal(2.5, 1.0, 8.5), limits()).unwrap();
        assert!(!plan.reached);
        let mut grid = Grid::new(Fake { layers, loads: 0 });
        let wet = Limits {
            water: true,
            ..limits()
        };
        let plan = find(&mut grid, [2, 1, 2], goal(2.5, 1.0, 8.5), wet).unwrap();
        assert!(plan.reached, "{plan:?}");
    }

    #[test]
    fn the_bounds_stop_the_search() {
        let mut grid = world(vec![vec![OPEN; 12], vec![OPEN; 12]]);
        let tight = Limits {
            radius: 3,
            ..limits()
        };
        let plan = find(&mut grid, [1, 1, 1], goal(1.5, 1.0, 10.5), tight).unwrap();
        assert!(!plan.reached);
        assert_eq!(plan.bound, Some(Bound::Radius));
        assert_eq!(plan.cells.last().unwrap()[2], 4);

        let few = Limits {
            max_nodes: 3,
            ..limits()
        };
        let plan = find(&mut grid, [1, 1, 1], goal(1.5, 1.0, 10.5), few).unwrap();
        assert_eq!(plan.bound, Some(Bound::Nodes));
        assert_eq!(plan.expanded, 3);

        let late = Limits {
            deadline: Instant::now(),
            ..limits()
        };
        let plan = find(&mut grid, [1, 1, 1], goal(1.5, 1.0, 10.5), late).unwrap();
        assert_eq!(plan.bound, Some(Bound::Time));
    }

    #[test]
    fn cells_load_in_aligned_boxes_once() {
        let mut grid = world(vec![vec![OPEN; 12], vec![OPEN; 12]]);
        grid.cell([1, 1, 1]).unwrap();
        grid.cell([3, 3, 3]).unwrap();
        assert_eq!(grid.loads, 1);
        grid.cell([4, 1, 1]).unwrap();
        assert_eq!(grid.loads, 2);
        grid.forget_near([1, 1, 1], 1);
        grid.cell([3, 3, 3]).unwrap();
        assert_eq!(grid.loads, 2);
        grid.cell([1, 1, 1]).unwrap();
        assert_eq!(grid.loads, 3);
    }
}

//! How many servers this machine can run at once, how a run's tests split across shards, and
//! where a shard places each test's structure.

use std::{fs, path::Path};

use serde_json::{Value, json};

use super::discover::Registered;
use crate::nbt::{self, Tag};

/// What one server costs: about this much memory, and this many cores to stay responsive.
pub const SERVER_MEMORY_MB: u64 = 500;
pub const CORES_PER_SERVER: usize = 2;
/// Servers at once, whatever the machine.
pub const MAX_SERVERS: usize = 8;
/// Servers one call may ask for in total; more run in waves of at most `max_servers`.
pub const MAX_REQUESTED: usize = 32;
/// The smallest grid cell for a test structure, and the space between neighbours.
const MIN_CELL: i32 = 16;
const CELL_GAP: i32 = 8;
/// Structure files larger than this are not read for their size.
const MAX_STRUCTURE_BYTES: u64 = 16 << 20;
const MAX_STRUCTURE_FILES: usize = 4_000;

/// The machine's room for servers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Capacity {
    pub cores: usize,
    /// Cores minus the one-minute load average, where the system reports one.
    pub free_cores: usize,
    pub free_memory_mb: Option<u64>,
    pub max_servers: usize,
}

impl Capacity {
    pub fn read() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |cores| cores.get());
        Self::from_parts(cores, load_average(), free_memory_mb())
    }

    /// `max_servers` = free cores / 2 and free memory / 500 MB, within 1 and [`MAX_SERVERS`].
    pub fn from_parts(cores: usize, load: Option<f64>, free_memory_mb: Option<u64>) -> Self {
        let free_cores = match load {
            Some(load) => cores.saturating_sub(load.round().max(0.0) as usize),
            None => cores,
        };
        let by_memory = free_memory_mb.map_or(MAX_SERVERS, |free| {
            usize::try_from(free / SERVER_MEMORY_MB).unwrap_or(MAX_SERVERS)
        });
        let max_servers = (free_cores / CORES_PER_SERVER)
            .min(by_memory)
            .clamp(1, MAX_SERVERS);
        Self {
            cores,
            free_cores,
            free_memory_mb,
            max_servers,
        }
    }

    pub fn to_json(self) -> Value {
        json!({
            "cores": self.cores,
            "free_cores": self.free_cores,
            "free_memory_mb": self.free_memory_mb,
            "max_servers": self.max_servers,
        })
    }
}

#[cfg(target_os = "linux")]
fn load_average() -> Option<f64> {
    fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn load_average() -> Option<f64> {
    None
}

#[cfg(target_os = "linux")]
fn free_memory_mb() -> Option<u64> {
    meminfo_available(&fs::read_to_string("/proc/meminfo").ok()?)
}

/// `MemAvailable:  123456 kB` → MB.
#[cfg(any(target_os = "linux", test))]
fn meminfo_available(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}

#[cfg(windows)]
fn free_memory_mb() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX is plain data; dwLength must hold its size before the call.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is a valid, writable MEMORYSTATUSEX with dwLength set.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return None;
    }
    Some(status.ullAvailPhys / (1024 * 1024))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn free_memory_mb() -> Option<u64> {
    None
}

/// One shard's tests in the order it runs them, as `runset` would: the tests without a batch
/// together, then each batch in turn. The tests of one step run at the same time.
#[derive(Clone, Debug, PartialEq)]
pub struct ShardPlan {
    pub steps: Vec<Vec<String>>,
}

impl ShardPlan {
    pub fn tests(&self) -> Vec<String> {
        self.steps.iter().flatten().cloned().collect()
    }
}

/// Split `tests` into at most `shards` shards. Tests of one batch stay together; groups go
/// largest first to the shard with the fewest tests, and each shard keeps registration order.
/// Fewer shards come back when there are fewer groups.
pub fn plan_shards(tests: &[Registered], shards: usize) -> Vec<ShardPlan> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (index, test) in tests.iter().enumerate() {
        let same_batch = test.batch.as_ref().and_then(|batch| {
            groups
                .iter()
                .position(|group| tests[group[0]].batch.as_ref() == Some(batch))
        });
        match same_batch {
            Some(group) => groups[group].push(index),
            None => groups.push(vec![index]),
        }
    }
    let count = shards.clamp(1, groups.len().max(1));
    // Stable: equal sizes keep registration order.
    groups.sort_by_key(|group| std::cmp::Reverse(group.len()));
    let mut planned: Vec<Vec<usize>> = vec![Vec::new(); count];
    for group in groups {
        let smallest = (0..count)
            .min_by_key(|&shard| planned[shard].len())
            .expect("at least one shard");
        planned[smallest].extend(group);
    }
    planned
        .into_iter()
        .filter(|shard| !shard.is_empty())
        .map(|mut shard| {
            shard.sort_unstable();
            let mut steps: Vec<(Option<&String>, Vec<String>)> = Vec::new();
            let unbatched: Vec<String> = shard
                .iter()
                .filter(|&&index| tests[index].batch.is_none())
                .map(|&index| tests[index].id.clone())
                .collect();
            if !unbatched.is_empty() {
                steps.push((None, unbatched));
            }
            for index in shard {
                let Some(batch) = tests[index].batch.as_ref() else {
                    continue;
                };
                let id = tests[index].id.clone();
                match steps.iter_mut().find(|(known, _)| *known == Some(batch)) {
                    Some((_, step)) => step.push(id),
                    None => steps.push((Some(batch), vec![id])),
                }
            }
            ShardPlan {
                steps: steps.into_iter().map(|(_, step)| step).collect(),
            }
        })
        .collect()
}

/// Offsets from the console's position for `count` structures in a square grid of `cell`
/// blocks, so tests started one by one do not overlap (`/gametest run` places every test at
/// the same spot).
pub fn grid(count: usize, cell: i32) -> Vec<(i32, i32)> {
    let columns = (count as f64).sqrt().ceil().max(1.0) as usize;
    (0..count)
        .map(|index| {
            let column = (index % columns) as i32;
            let row = (index / columns) as i32;
            (column * cell, row * cell)
        })
        .collect()
}

/// The grid cell for these behavior packs: the widest `.mcstructure` footprint plus a gap.
/// Structures saved only in a world are not seen, so they must fit in the result.
pub fn cell_size(packs: &[impl AsRef<Path>]) -> i32 {
    let mut widest = 0;
    let mut files = 0;
    for pack in packs {
        widest = widest.max(widest_structure(
            &pack.as_ref().join("structures"),
            &mut files,
        ));
    }
    widest.max(MIN_CELL) + CELL_GAP
}

fn widest_structure(dir: &Path, files: &mut usize) -> i32 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut widest = 0;
    for entry in entries.flatten() {
        if *files >= MAX_STRUCTURE_FILES {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            widest = widest.max(widest_structure(&path, files));
            continue;
        }
        if path
            .extension()
            .is_none_or(|extension| extension != "mcstructure")
        {
            continue;
        }
        *files += 1;
        if fs::metadata(&path).is_ok_and(|metadata| metadata.len() <= MAX_STRUCTURE_BYTES)
            && let Ok(data) = fs::read(&path)
        {
            widest = widest.max(structure_footprint(&data).unwrap_or(0));
        }
    }
    widest
}

/// The larger of a structure's x and z size.
fn structure_footprint(data: &[u8]) -> Option<i32> {
    let root = nbt::parse_compound(data).ok()?;
    let Some(Tag::List(_, size)) = root.get("size") else {
        return None;
    };
    match size.as_slice() {
        [Tag::Int(x), _, Tag::Int(z)] => Some((*x).max(*z)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{nbt::Compound, testdir::TestDir};

    fn test(id: &str, batch: Option<&str>) -> Registered {
        Registered {
            id: id.into(),
            class: "demo".into(),
            tags: Vec::new(),
            batch: batch.map(Into::into),
            unknown_chain: false,
        }
    }

    #[test]
    fn capacity_takes_the_smallest_limit() {
        let roomy = Capacity::from_parts(16, Some(1.0), Some(20_000));
        assert_eq!((roomy.free_cores, roomy.max_servers), (15, 7));
        let capped = Capacity::from_parts(64, None, Some(64_000));
        assert_eq!(capped.max_servers, MAX_SERVERS);
        let tight = Capacity::from_parts(16, Some(2.0), Some(1_200));
        assert_eq!(tight.max_servers, 2);
        let busy = Capacity::from_parts(4, Some(6.0), None);
        assert_eq!((busy.free_cores, busy.max_servers), (0, 1));
        assert_eq!(
            meminfo_available("MemTotal: 1 kB\nMemAvailable:   2048000 kB\n"),
            Some(2000)
        );
    }

    #[test]
    fn shards_balance_tests_and_keep_batches_together() {
        let tests: Vec<Registered> = (1..=7)
            .map(|n| test(&format!("demo:t{n}"), None))
            .chain([
                test("demo:n1", Some("night")),
                test("demo:n2", Some("night")),
            ])
            .collect();
        let plan = plan_shards(&tests, 3);
        assert_eq!(plan.len(), 3);
        let sizes: Vec<usize> = plan.iter().map(|shard| shard.tests().len()).collect();
        assert_eq!(sizes.iter().sum::<usize>(), 9);
        assert!(sizes.iter().all(|&size| size == 3), "{plan:?}");
        let night = plan
            .iter()
            .find(|shard| shard.tests().contains(&"demo:n1".to_owned()))
            .unwrap();
        // The unbatched test runs first, then the batch as its own step.
        assert_eq!(night.steps.len(), 2);
        assert_eq!(night.steps[1], ["demo:n1", "demo:n2"]);
        assert_eq!(plan_shards(&tests[..2], 5).len(), 2);
        let whole = plan_shards(&tests, 1);
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].steps.len(), 2);
        assert_eq!(whole[0].steps[0].len(), 7);
        assert!(plan_shards(&[], 3).is_empty());
    }

    #[test]
    fn grid_cells_do_not_overlap() {
        assert_eq!(grid(5, 24), [(0, 0), (24, 0), (48, 0), (0, 24), (24, 24)]);
        assert_eq!(grid(1, 24), [(0, 0)]);
    }

    #[test]
    fn cell_size_reads_structure_files() {
        let dir = TestDir::new("bds-cells");
        let structures = dir.path().join("bp/structures/demo");
        fs::create_dir_all(&structures).unwrap();
        let size = Tag::List(3, vec![Tag::Int(40), Tag::Int(5), Tag::Int(12)]);
        let root = Compound(vec![
            ("format_version".into(), Tag::Int(1)),
            ("size".into(), size),
        ]);
        let level = nbt::LevelDat {
            storage_version: 0,
            root_name: String::new(),
            root,
        };
        // A .mcstructure is the level.dat body without its 8-byte header.
        fs::write(
            structures.join("big.mcstructure"),
            &level.to_bytes().unwrap()[8..],
        )
        .unwrap();
        assert_eq!(cell_size(&[dir.path().join("bp")]), 40 + CELL_GAP);
        assert_eq!(cell_size(&[dir.path().join("none")]), MIN_CELL + CELL_GAP);
    }
}

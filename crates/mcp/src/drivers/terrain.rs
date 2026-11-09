//! Where the pathfinder's block cells come from, best first: the Bridge pack's
//! `re_mcp:blocks`, which classifies a whole box in one command, else one `testforblock` per
//! cell (single player with cheats), sent many at a time. Without either, walking goes
//! straight at the goal.

use std::thread;

use serde_json::Value;

use super::path::{Blocks, Cell};
use crate::{
    bridge::quoted,
    error::{Result, bail},
    world::{World, vanilla::parse_testforblock},
    ws::MAX_IN_FLIGHT,
};

/// The Bridge pack's box: 2048 cells, one command.
const BRIDGE_CHUNK: [i64; 3] = [16, 8, 16];
/// The test commands' box: 64 cells, sent at once.
const COMMAND_CHUNK: [i64; 3] = [4, 4, 4];
/// How many test commands go out at the same time: as many as the socket lets out, since a
/// box costs about one round trip per batch.
const COMMAND_WORKERS: usize = MAX_IN_FLIGHT;

/// Cells from the Bridge pack's `re_mcp:blocks`.
pub struct BridgeBlocks {
    pub world: World,
}

impl Blocks for BridgeBlocks {
    fn load(&mut self, [x, y, z]: [i64; 3], [dx, dy, dz]: [i64; 3]) -> Result<Vec<Cell>> {
        let spec = format!("x={x};y={y};z={z};dx={dx};dy={dy};dz={dz}");
        let reply = self
            .world
            .bridge_command(&format!("re_mcp:blocks {}", quoted(&spec)?))?;
        let Some(cells) = reply["cells"].as_str() else {
            bail!("re_mcp:blocks answered without cells: {reply}");
        };
        Ok(cells.chars().map(Cell::from_letter).collect())
    }

    fn chunk(&self) -> [i64; 3] {
        BRIDGE_CHUNK
    }
}

/// Cells from `testforblock <cell> air`: air passes, and a failed test names the block. A
/// cell whose reply names no block (unloaded, outside the world) is unknown.
pub struct CommandBlocks {
    pub world: World,
}

/// What one `testforblock` reply says about its cell; `None` when it names no block and the
/// cell is not outside the loaded world either, such as a refused command.
fn classify_reply(body: &Value) -> Option<Cell> {
    if body["statusCode"].as_i64().unwrap_or(0) == 0 {
        return Some(Cell::Open);
    }
    let message = body["statusMessage"].as_str().unwrap_or_default();
    if let Some(name) = parse_testforblock(message) {
        return Some(classify_name(&name));
    }
    let lower = message.to_ascii_lowercase();
    (lower.contains("outside") || lower.contains("unloaded")).then_some(Cell::Unknown)
}

/// A block's display name as a cell. Names of blocks without collision (plants, torches,
/// rails, carpets) are open; fences and walls are tall; any other block is solid, so an
/// unknown name keeps the path away rather than through it.
pub fn classify_name(name: &str) -> Cell {
    let name = name.to_ascii_lowercase();
    if name.contains("lava") {
        return Cell::Lava;
    }
    if name.contains("water") {
        return Cell::Water;
    }
    if name.contains("fence") || name.ends_with(" wall") {
        return Cell::Tall;
    }
    const SOLID_LOOKALIKES: [&str; 4] = ["grass block", "grass path", "mushroom block", "stem"];
    if SOLID_LOOKALIKES.iter().any(|solid| name.contains(solid)) {
        return Cell::Solid;
    }
    // Air passes the test itself, so it never gets here; "air" would also match "stairs".
    const OPEN: [&str; 23] = [
        "grass",
        "fern",
        "flower",
        "poppy",
        "dandelion",
        "tulip",
        "orchid",
        "allium",
        "bluet",
        "daisy",
        "cornflower",
        "lily of the valley",
        "torch",
        "sapling",
        "rail",
        "pressure plate",
        "button",
        "carpet",
        "vine",
        "dead bush",
        "mushroom",
        "lever",
        "redstone dust",
    ];
    if OPEN.iter().any(|open| name.contains(open)) {
        Cell::Open
    } else {
        Cell::Solid
    }
}

impl Blocks for CommandBlocks {
    fn load(&mut self, origin: [i64; 3], size: [i64; 3]) -> Result<Vec<Cell>> {
        let mut cells = Vec::new();
        for y in 0..size[1] {
            for z in 0..size[2] {
                for x in 0..size[0] {
                    cells.push([origin[0] + x, origin[1] + y, origin[2] + z]);
                }
            }
        }
        let per_worker = cells.len().div_ceil(COMMAND_WORKERS);
        let replies: Vec<Result<Option<Cell>>> = thread::scope(|scope| {
            let workers: Vec<_> = cells
                .chunks(per_worker)
                .map(|part| {
                    let world = self.world.clone();
                    scope.spawn(move || {
                        part.iter()
                            .map(|[x, y, z]| {
                                world
                                    .raw(&format!("testforblock {x} {y} {z} air"))
                                    .map(|body| classify_reply(&body))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap_or_default())
                .collect()
        });
        let mut found = Vec::with_capacity(replies.len());
        let mut unread = false;
        for reply in replies {
            match reply? {
                Some(cell) => found.push(cell),
                None => {
                    unread = true;
                    found.push(Cell::Unknown);
                }
            }
        }
        if unread && found.iter().all(|cell| *cell == Cell::Unknown) {
            bail!("testforblock answered without naming a block; it may need cheats");
        }
        Ok(found)
    }

    fn chunk(&self) -> [i64; 3] {
        COMMAND_CHUNK
    }
}

impl<B: Blocks + ?Sized> Blocks for Box<B> {
    fn load(&mut self, origin: [i64; 3], size: [i64; 3]) -> Result<Vec<Cell>> {
        (**self).load(origin, size)
    }

    fn chunk(&self) -> [i64; 3] {
        (**self).chunk()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn display_names_sort_into_cells() {
        assert_eq!(classify_name("Stone"), Cell::Solid);
        assert_eq!(classify_name("Grass Block"), Cell::Solid);
        assert_eq!(classify_name("Short Grass"), Cell::Open);
        assert_eq!(classify_name("Torch"), Cell::Open);
        assert_eq!(classify_name("Oak Fence"), Cell::Tall);
        assert_eq!(classify_name("Cobblestone Wall"), Cell::Tall);
        assert_eq!(classify_name("Water"), Cell::Water);
        assert_eq!(classify_name("Lava"), Cell::Lava);
        assert_eq!(classify_name("Brown Mushroom Block"), Cell::Solid);
    }

    #[test]
    fn replies_name_the_block_or_say_nothing() {
        let found = json!({"statusCode": 1, "statusMessage": "The block at 1,2,3 is Stone (expected: Air)."});
        assert_eq!(classify_reply(&found), Some(Cell::Solid));
        assert_eq!(classify_reply(&json!({"statusCode": 0})), Some(Cell::Open));
        let far =
            json!({"statusCode": 1, "statusMessage": "Cannot test for block outside of the world"});
        assert_eq!(classify_reply(&far), Some(Cell::Unknown));
        let refused = json!({"statusCode": 1, "statusMessage": "You do not have permission"});
        assert_eq!(classify_reply(&refused), None);
    }
}

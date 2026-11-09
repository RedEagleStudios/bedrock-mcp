//! Test scenes: named spots saved per world, and `scene_reset`, which puts the player, the
//! world and the inventory back into a known state in one call.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Session;
use crate::{
    bridge::{Actor, quoted},
    clock::poll,
    drivers,
    error::{Error, Result, bail},
    game_files,
    world::{World, direction, number, vanilla::parse_testforblock},
    worlds::{self, WorldFolder},
};

/// Reads the player the way `player_get` does.
pub type PlayerReader = fn(&mut Session, &Actor) -> Result<Value>;

/// How long a teleport gets for the chunks around the player to load.
const CHUNK_WAIT: Duration = Duration::from_secs(5);
const CHUNK_POLL: Duration = Duration::from_millis(200);
/// How far ahead of the player `scene_reset` wants open space.
const CLEAR_AHEAD: f64 = 3.0;
/// The heights above the feet that the spot check casts its rays from: legs and head.
const CHECK_HEIGHTS: [f64; 2] = [0.5, 1.5];
/// Eye height of a standing player, to turn a querytarget eye position into feet.
const EYE_HEIGHT: f64 = 1.62;
/// What the spot check without the Bridge pack tests, for the reply's notes.
const TESTFORBLOCK_NOTE: &str = "The Bridge pack is not active, so testforblock checked the \
     spot instead of a raycast: the blocks at the legs and head and up to 3 blocks ahead at both \
     heights must be air. Passable blocks such as grass or water count as in the way too.";
/// Where entities go: moved below the world instead of killed, so nothing dies in view.
const VOID_Y: i32 = -400;
/// The player fields the default reply keeps: where it stands and faces, its health and its
/// selected slot.
const PLAYER_FIELDS: [&str; 7] = [
    "source",
    "dimension",
    "location",
    "yaw",
    "pitch",
    "health",
    "selectedSlot",
];

/// A place to stand: feet position, optional rotation and dimension.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Place {
    x: f64,
    y: f64,
    z: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    yaw: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pitch: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dimension: Option<String>,
}

impl Place {
    /// The `re_mcp:player_set` spec: a Script API teleport, which writes nothing to chat.
    fn bridge_spec(&self) -> Result<String> {
        let mut spec = vec![
            format!("x={}", number(self.x)),
            format!("y={}", number(self.y)),
            format!("z={}", number(self.z)),
        ];
        if let Some(yaw) = self.yaw {
            spec.push(format!("yaw={}", number(yaw)));
            spec.push(format!("pitch={}", number(self.pitch.unwrap_or(0.0))));
        }
        if let Some(dimension) = &self.dimension {
            spec.push(format!("dimension={}", identifier(dimension)?));
        }
        quoted(&spec.join(";"))
    }

    /// The `/tp` line, for worlds without the Bridge pack; the game shows it in chat.
    fn command(&self) -> Result<String> {
        let mut tp = format!(
            "tp @s {} {} {}",
            number(self.x),
            number(self.y),
            number(self.z)
        );
        if let Some(yaw) = self.yaw {
            tp.push_str(&format!(
                " {} {}",
                number(yaw),
                number(self.pitch.unwrap_or(0.0))
            ));
        }
        Ok(match &self.dimension {
            Some(dimension) => {
                let dimension = identifier(dimension)?;
                let dimension = dimension.strip_prefix("minecraft:").unwrap_or(dimension);
                format!("execute in {dimension} run {tp}")
            }
            None => tp,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneSaveArgs {
    name: String,
    world: Option<String>,
    at: Option<Place>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Clear {
    All(bool),
    Types(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Time {
    Ticks(u32),
    Named(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Effect {
    effect: String,
    seconds: Option<u32>,
    amplifier: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Give {
    item: String,
    count: Option<u32>,
    slot: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneResetArgs {
    world: Option<String>,
    spot: Option<String>,
    teleport: Option<Place>,
    clear_entities: Option<Clear>,
    time: Option<Time>,
    weather: Option<String>,
    difficulty: Option<String>,
    gamemode: Option<String>,
    effects: Option<Vec<Effect>>,
    #[serde(default)]
    clear_inventory: bool,
    #[serde(default)]
    give: Vec<Give>,
    #[serde(default)]
    heal: bool,
    hotbar: Option<u8>,
    perspective: Option<String>,
    settle_ms: Option<u64>,
    /// Swap the hotbar slot and back and walk a block and back, so the held item animates.
    warm_up: Option<bool>,
    /// Reply with every step's command and output and the whole player.
    #[serde(default)]
    verbose: bool,
}

/// Named spots of one world, as stored in `scenes/<folder>.json` in the agent directory.
#[derive(Default, Deserialize, Serialize)]
struct Spots {
    world: String,
    spots: BTreeMap<String, Place>,
}

/// A namespaced id such as `minecraft:zombie` or `speed`: nothing a command could read as
/// another argument.
fn identifier(text: &str) -> Result<&str> {
    let text = text.trim();
    let valid = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'));
    if !valid {
        bail!("{text:?} is not an id; ids look like minecraft:zombie or demo:ruby_sword.");
    }
    Ok(text)
}

fn one_of<'a>(field: &str, value: &'a str, allowed: &[&str]) -> Result<&'a str> {
    if allowed.contains(&value) {
        Ok(value)
    } else {
        bail!(
            "{field} must be one of {}, not {value:?}.",
            allowed.join(", ")
        )
    }
}

/// The commands `scene_reset` runs after the teleport, in order, each with its step.
fn commands(args: &SceneResetArgs) -> Result<Vec<(&'static str, String)>> {
    let mut steps = Vec::new();
    match &args.clear_entities {
        None | Some(Clear::All(false)) => {}
        Some(Clear::All(true)) => {
            steps.push((
                "clear_entities",
                format!("tp @e[type=!player] ~ {VOID_Y} ~"),
            ));
        }
        Some(Clear::Types(types)) => {
            for kind in types {
                let kind = identifier(kind)?;
                if kind.trim_start_matches("minecraft:") == "player" {
                    bail!("clear_entities never removes players.");
                }
                steps.push(("clear_entities", format!("tp @e[type={kind}] ~ {VOID_Y} ~")));
            }
        }
    }
    if let Some(time) = &args.time {
        let value = match time {
            Time::Ticks(ticks) => ticks.to_string(),
            Time::Named(name) => one_of(
                "time",
                name,
                &["day", "noon", "sunset", "night", "midnight", "sunrise"],
            )?
            .to_owned(),
        };
        steps.push(("time", format!("time set {value}")));
    }
    if let Some(weather) = &args.weather {
        let weather = one_of("weather", weather, &["clear", "rain", "thunder"])?;
        steps.push(("weather", format!("weather {weather}")));
    }
    if let Some(difficulty) = &args.difficulty {
        let difficulty = one_of(
            "difficulty",
            difficulty,
            &["peaceful", "easy", "normal", "hard"],
        )?;
        steps.push(("difficulty", format!("difficulty {difficulty}")));
    }
    if let Some(mode) = &args.gamemode {
        let mode = one_of(
            "gamemode",
            mode,
            &["survival", "creative", "adventure", "spectator"],
        )?;
        steps.push(("gamemode", format!("gamemode {mode} @s")));
    }
    if let Some(effects) = &args.effects {
        steps.push(("effects", "effect @s clear".to_owned()));
        for effect in effects {
            steps.push((
                "effects",
                format!(
                    "effect @s {} {} {} true",
                    identifier(&effect.effect)?,
                    effect.seconds.unwrap_or(600),
                    effect.amplifier.unwrap_or(0)
                ),
            ));
        }
    }
    if args.clear_inventory {
        steps.push(("clear_inventory", "clear @s".to_owned()));
    }
    for give in &args.give {
        let item = identifier(&give.item)?;
        let count = give.count.unwrap_or(1);
        steps.push((
            "give",
            match give.slot {
                Some(slot) if slot > 8 => bail!("give slot {slot} is outside the hotbar (0 to 8)."),
                Some(slot) => format!("replaceitem entity @s slot.hotbar {slot} {item} {count}"),
                None => format!("give @s {item} {count}"),
            },
        ));
    }
    if args.heal {
        steps.push(("heal", "effect @s instant_health 1 20 true".to_owned()));
        steps.push(("heal", "effect @s saturation 1 20 true".to_owned()));
    }
    Ok(steps)
}

/// Whether a command only removes things, so a failure means there was nothing to remove: no
/// entity to move, no effect to clear, an empty inventory. The game's message for that is in
/// the player's language, so the command, not the message, decides.
fn may_find_nothing(command: &str) -> bool {
    command.starts_with("tp @e[") || command == "effect @s clear" || command == "clear @s"
}

/// The status a command the game cannot parse or does not know answers with: i32::MIN on the
/// public 1.26.62 build, where a command that finds nothing answers with another negative status.
const SYNTAX_ERROR: i64 = -2_147_483_648;

/// Run the commands in order. A selector that matched nothing, or a command that only removes
/// things and found nothing, is fine; any other failure stops the run and names the step.
fn run(world: &World, commands: &[(&'static str, String)], report: &mut Vec<Value>) -> Result<()> {
    for (step, command) in commands {
        let body = world.raw(command)?;
        let code = body["statusCode"].as_i64().unwrap_or(0);
        let message = body["statusMessage"].as_str().unwrap_or_default();
        let nothing = code != 0
            && (message.contains("No targets matched")
                || (code != SYNTAX_ERROR && may_find_nothing(command)));
        let mut entry = json!({
            "step": step,
            "command": command,
            "ok": code == 0 || nothing,
            "status_message": message,
        });
        if nothing {
            entry["nothing_to_do"] = true.into();
        }
        report.push(entry);
        if code != 0 && !nothing {
            bail!(
                "scene_reset stopped at {step}: /{command} failed: {message}. Steps that ran: {}",
                Value::Array(report.clone())
            );
        }
    }
    Ok(())
}

/// Teleport the player: with the Bridge pack through the Script API, which writes nothing to
/// chat, else with `/tp`.
fn teleport(world: &World, place: &Place, report: &mut Vec<Value>) -> Result<()> {
    if !world.bridge(true)? {
        return run(world, &[("teleport", place.command()?)], report);
    }
    let command = format!("re_mcp:player_set {}", place.bridge_spec()?);
    let outcome = world.bridge_write(&command)?;
    let rejected = &outcome["rejected"];
    let ok = rejected.as_object().is_none_or(|map| map.is_empty());
    report.push(json!({ "step": "teleport", "command": command, "ok": ok, "rejected": rejected }));
    if !ok {
        bail!("scene_reset stopped at teleport: the game refused it: {rejected}");
    }
    Ok(())
}

/// The player's feet: its location, else its eye lowered by [`EYE_HEIGHT`].
fn feet_of(player: &Value) -> Option<[f64; 3]> {
    let point = |at: &Value| Some([at["x"].as_f64()?, at["y"].as_f64()?, at["z"].as_f64()?]);
    point(&player["location"])
        .or_else(|| point(&player["eye"]).map(|[x, y, z]| [x, y - EYE_HEIGHT, z]))
}

/// "legs are" or "head is", for a check height.
fn body_part(height: f64) -> (&'static str, &'static str) {
    if height < 1.0 {
        ("legs", "are")
    } else {
        ("head", "is")
    }
}

/// Problems with where the player stands: inside a block, in water, or a block within
/// [`CLEAR_AHEAD`] blocks ahead. The Bridge pack's raycast finds solid blocks; without it,
/// testforblock finds every block that is not air.
fn check_spot(world: &World, player: &Value, notes: &mut Vec<String>) -> Result<Vec<String>> {
    let (Some(feet), Some(yaw)) = (feet_of(player), player["yaw"].as_f64()) else {
        notes.push(
            "The spot was not checked for blocks: the player read gave no position and yaw."
                .to_owned(),
        );
        return Ok(Vec::new());
    };
    let mut problems = Vec::new();
    let flags = &player["flags"];
    if flags["inWater"] == true {
        problems.push(format!("The player is in water at {}.", player["location"]));
    } else if flags["onGround"] == false {
        notes.push(format!(
            "The player is not on the ground at {}; settle_ms gives a fall time to end.",
            player["location"]
        ));
    }
    if world.bridge(true)? {
        problems.extend(raycast_problems(world, feet, yaw)?);
    } else {
        notes.push(TESTFORBLOCK_NOTE.to_owned());
        problems.extend(testforblock_problems(world, feet, yaw, notes)?);
    }
    Ok(problems)
}

/// The spot check with the Bridge pack's raycast from the legs and the head.
fn raycast_problems(world: &World, [x, y, z]: [f64; 3], yaw: f64) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let [dx, _, dz] = direction(yaw, 0.0);
    for height in CHECK_HEIGHTS {
        let from = [x, y + height, z];
        let spec = format!(
            "dx={};dy=0;dz={};blocks=1;entities=0;max={};x={};y={};z={}",
            number(dx),
            number(dz),
            number(CLEAR_AHEAD),
            number(from[0]),
            number(from[1]),
            number(from[2])
        );
        let hit = world.bridge_command(&format!("re_mcp:raycast {}", quoted(&spec)?))?;
        let block = &hit["block"];
        if block.is_null() {
            continue;
        }
        let id = block["id"].as_str().unwrap_or("a block");
        let at = &block["location"];
        let inside = [at["x"].as_f64(), at["y"].as_f64(), at["z"].as_f64()]
            == [
                Some(from[0].floor()),
                Some(from[1].floor()),
                Some(from[2].floor()),
            ];
        let (part, verb) = body_part(height);
        problems.push(if inside {
            format!("The player's {part} {verb} inside {id} at {at}.")
        } else {
            format!(
                "{id} at {at} blocks the way {} blocks ahead at {part} height.",
                block["distance"]
            )
        });
    }
    Ok(problems)
}

/// The spot check with `testforblock <cell> air`: the cells at the legs and the head, then
/// one per block ahead up to [`CLEAR_AHEAD`] at both heights. A reply that does not name the
/// block goes to `notes` instead.
fn testforblock_problems(
    world: &World,
    [x, y, z]: [f64; 3],
    yaw: f64,
    notes: &mut Vec<String>,
) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let mut tested = Vec::new();
    let [dx, _, dz] = direction(yaw, 0.0);
    for height in CHECK_HEIGHTS {
        let (part, verb) = body_part(height);
        for ahead in 0..=CLEAR_AHEAD as u32 {
            let distance = f64::from(ahead);
            let cell = [x + dx * distance, y + height, z + dz * distance].map(|v| v.floor() as i64);
            if tested.contains(&cell) {
                continue;
            }
            tested.push(cell);
            let [cx, cy, cz] = cell;
            let body = world.raw(&format!("testforblock {cx} {cy} {cz} air"))?;
            if body["statusCode"].as_i64().unwrap_or(0) == 0 {
                continue;
            }
            let message = body["statusMessage"].as_str().unwrap_or_default();
            let at = format!("{cx},{cy},{cz}");
            match parse_testforblock(message) {
                Some(block) if ahead == 0 => {
                    problems.push(format!(
                        "The player's {part} {verb} inside {block} at {at}."
                    ));
                }
                Some(block) => problems.push(format!(
                    "{block} at {at} blocks the way {ahead} blocks ahead at {part} height."
                )),
                None => notes.push(format!(
                    "testforblock at {at} answered {message:?}, so that block was not checked."
                )),
            }
        }
    }
    Ok(problems)
}

/// When something under `path` last changed: `level.dat` and the files in `db`.
fn last_change(path: &Path) -> Option<SystemTime> {
    let modified = |path: PathBuf| fs::metadata(path).and_then(|meta| meta.modified()).ok();
    let db = fs::read_dir(path.join("db"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| modified(entry.path()));
    db.chain(modified(path.join("level.dat"))).max()
}

/// The world `key` names, or else the world the game wrote last: the open one.
fn scene_world(shared: &Path, key: Option<&str>) -> Result<WorldFolder> {
    if let Some(key) = key {
        return worlds::find(shared, key);
    }
    worlds::list(shared)
        .into_iter()
        .max_by_key(|world| last_change(&world.path))
        .ok_or_else(|| Error::msg("No local world was found."))
}

impl Session {
    fn spots_path(&self, world: &WorldFolder) -> PathBuf {
        self.install
            .agent_dir()
            .join("scenes")
            .join(format!("{}.json", world.folder))
    }

    fn read_spots(&self, world: &WorldFolder) -> Result<Spots> {
        match fs::read(self.spots_path(world)) {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Spots {
                world: world.name.clone(),
                ..Spots::default()
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn current_world(&self, key: Option<&str>) -> Result<WorldFolder> {
        let shared = game_files::shared_dir(&self.install.prefix)?;
        scene_world(&shared, key)
    }

    pub(crate) fn scene_save(&mut self, args: SceneSaveArgs, read: PlayerReader) -> Result<Value> {
        let name = args.name.trim();
        if name.is_empty() {
            bail!("scene_save needs a name.");
        }
        let (mut source, mut notes) = (json!("argument"), json!([]));
        let place = match args.at {
            Some(place) => place,
            None => {
                let player = read(self, &Actor::Me)?;
                source = player["source"].clone();
                notes = player["notes"].clone();
                let location = &player["location"];
                let (Some(x), Some(y), Some(z)) = (
                    location["x"].as_f64(),
                    location["y"].as_f64(),
                    location["z"].as_f64(),
                ) else {
                    bail!(
                        "The player's feet position needs the Bridge pack, or the unstable \
                         layer's world_state capability in single player ({}); pass at {{x, y, \
                         z, yaw, pitch}} instead.",
                        player["notes"]
                    );
                };
                Place {
                    x,
                    y,
                    z,
                    yaw: player["yaw"].as_f64(),
                    pitch: player["pitch"].as_f64(),
                    dimension: player["dimension"].as_str().map(str::to_owned),
                }
            }
        };
        let world = self.current_world(args.world.as_deref())?;
        let mut spots = self.read_spots(&world)?;
        spots.world = world.name.clone();
        spots.spots.insert(name.to_owned(), place.clone());
        let path = self.spots_path(&world);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, serde_json::to_vec_pretty(&spots)?)?;
        Ok(json!({
            "source": source,
            "notes": notes,
            "saved": name,
            "spot": place,
            "world": { "name": world.name, "folder": world.folder },
            "spots": spots.spots.keys().collect::<Vec<_>>(),
        }))
    }

    pub(crate) fn scene_reset(
        &mut self,
        args: SceneResetArgs,
        read: PlayerReader,
    ) -> Result<Value> {
        let mut notes = Vec::new();
        let mut world_used = Value::Null;
        let place = match (&args.spot, &args.teleport) {
            (Some(_), Some(_)) => bail!("Give spot or teleport, not both."),
            (None, place) => place.clone(),
            (Some(name), None) => {
                let world = self.current_world(args.world.as_deref())?;
                let spots = self.read_spots(&world)?;
                world_used = json!({ "name": world.name, "folder": world.folder });
                let Some(place) = spots.spots.get(name.trim()) else {
                    bail!(
                        "No spot named {name:?} in {:?}; saved spots: {:?}. Save one with \
                         scene_save.",
                        world.name,
                        spots.spots.keys().collect::<Vec<_>>()
                    );
                };
                Some(place.clone())
            }
        };
        let perspective = args
            .perspective
            .as_deref()
            .map(|value| {
                one_of(
                    "perspective",
                    value,
                    &["first_person", "third_person_back", "third_person_front"],
                )
            })
            .transpose()?;
        let commands = commands(&args)?;
        let world = self.ensure_world()?;
        let mut steps = Vec::new();
        if let Some(place) = &place {
            teleport(&world, place, &mut steps)?;
            let clock = self.clock.clone();
            let loaded = poll(&*clock, CHUNK_WAIT, CHUNK_POLL, || {
                Ok(world.chunks_loaded(1)?.then_some(()))
            })?;
            if loaded.is_none() {
                notes.push(format!(
                    "The chunks around the spot did not load within {} s.",
                    CHUNK_WAIT.as_secs()
                ));
            }
        }
        run(&world, &commands, &mut steps)?;
        if let Some(slot) = args.hotbar {
            if slot > 8 {
                bail!("hotbar {slot} is outside 0 to 8.");
            }
            let outcome = if world.bridge(true)? {
                world.bridge_write(&format!("re_mcp:hotbar {slot}"))?
            } else {
                self.input(serde_json::from_value(json!({ "hotbar": slot + 1 }))?)?
            };
            steps.push(json!({ "step": "hotbar", "ok": true, "result": outcome }));
        }
        if let Some(perspective) = perspective {
            if self.unstable_capability("camera", &mut notes) {
                let outcome = self.camera_set(serde_json::from_value(
                    json!({ "perspective": perspective }),
                )?)?;
                steps.push(json!({
                    "step": "perspective",
                    "ok": true,
                    "perspective": outcome["perspective"],
                    "presses": outcome["presses"],
                }));
            } else {
                notes.push(format!(
                    "perspective {perspective} was skipped: it needs the unstable layer's camera \
                     capability; press F5 with input_send instead."
                ));
            }
        }
        if args.warm_up.unwrap_or(true) {
            // The warm-up puts the player back where it stood, so the spot holds.
            let mut report = drivers::warm_up(self);
            report["step"] = "warm_up".into();
            report["ok"] = report["done"].clone();
            steps.push(report);
        }
        if let Some(ms) = args.settle_ms {
            self.clock.sleep(Duration::from_millis(ms));
        }
        let mut player = read(self, &Actor::Me)?;
        let more = player
            .as_object_mut()
            .and_then(|fields| fields.remove("notes"));
        if let Some(Value::Array(more)) = more {
            notes.extend(more.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        let problems = if place.is_some() || args.spot.is_some() {
            check_spot(&world, &player, &mut notes)?
        } else {
            Vec::new()
        };
        let reset = Reset {
            world: world_used,
            spot: place,
            steps,
            player,
            problems,
            notes,
        };
        Ok(reset.reply(args.verbose))
    }
}

/// What a scene_reset did, for its reply.
struct Reset {
    world: Value,
    spot: Option<Place>,
    steps: Vec<Value>,
    player: Value,
    problems: Vec<String>,
    notes: Vec<String>,
}

impl Reset {
    /// `{ok, spot_problems, player, warm_up, notes}`: the player afterwards (where it stands
    /// and faces, health, selected slot and the perspective set) and the warm-up as `{done,
    /// detail}`. A failed step never gets here: it stops the call with an error. `verbose` adds
    /// the world, the spot, every step and the whole player.
    fn reply(self, verbose: bool) -> Value {
        let step = |name: &str| self.steps.iter().find(|step| step["step"] == name);
        let perspective = step("perspective").map(|step| step["perspective"].clone());
        let mut player = self.player;
        if let (Some(fields), Some(perspective)) = (player.as_object_mut(), perspective) {
            fields.insert("perspective".to_owned(), perspective);
        }
        let mut answer = json!({
            "ok": self.problems.is_empty(),
            "spot_problems": self.problems,
            "notes": self.notes,
        });
        if let Some(warm) = step("warm_up") {
            answer["warm_up"] = drivers::warm_summary(warm);
        }
        if verbose {
            answer["world"] = self.world;
            answer["spot"] = json!(self.spot);
            answer["steps"] = self.steps.into();
            answer["player"] = player;
            return answer;
        }
        let mut brief = serde_json::Map::new();
        for field in PLAYER_FIELDS.iter().chain(["perspective"].iter()) {
            if let Some(value) = player.get(*field) {
                brief.insert((*field).to_owned(), value.clone());
            }
        }
        if player.get("location").is_none()
            && let Some(eye) = player.get("eye")
        {
            brief.insert("eye".to_owned(), eye.clone());
        }
        answer["player"] = brief.into();
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bridge::fake, testdir::TestDir};

    fn args(value: Value) -> SceneResetArgs {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn commands_follow_the_documented_order() {
        let place = Place {
            x: -289.5,
            y: 63.0,
            z: -146.5,
            yaw: Some(0.0),
            pitch: None,
            dimension: Some("minecraft:overworld".into()),
        };
        let args = args(json!({
            "give": [{"item": "demo:ruby_sword", "slot": 0}, {"item": "minecraft:apple", "count": 3}],
            "clear_entities": true,
            "heal": true,
            "clear_inventory": true,
            "time": "noon",
            "effects": [{"effect": "speed", "seconds": 30, "amplifier": 1}],
        }));
        assert_eq!(
            place.command().unwrap(),
            "execute in overworld run tp @s -289.5 63 -146.5 0 0"
        );
        assert_eq!(
            place.bridge_spec().unwrap(),
            "\"x=-289.5;y=63;z=-146.5;yaw=0;pitch=0;dimension=minecraft:overworld\""
        );
        let lines: Vec<String> = commands(&args)
            .unwrap()
            .into_iter()
            .map(|(_, line)| line)
            .collect();
        assert_eq!(
            lines,
            [
                "tp @e[type=!player] ~ -400 ~",
                "time set noon",
                "effect @s clear",
                "effect @s speed 30 1 true",
                "clear @s",
                "replaceitem entity @s slot.hotbar 0 demo:ruby_sword 1",
                "give @s minecraft:apple 3",
                "effect @s instant_health 1 20 true",
                "effect @s saturation 1 20 true",
            ]
        );
    }

    #[test]
    fn clear_entities_removes_everything_but_players_or_only_the_listed_types() {
        let lines = |value: Value| -> Vec<String> {
            commands(&args(value))
                .unwrap()
                .into_iter()
                .map(|(_, line)| line)
                .collect()
        };
        assert_eq!(
            lines(json!({"clear_entities": true})),
            ["tp @e[type=!player] ~ -400 ~"]
        );
        assert_eq!(
            lines(json!({"clear_entities": ["zombie", "minecraft:item"]})),
            [
                "tp @e[type=zombie] ~ -400 ~",
                "tp @e[type=minecraft:item] ~ -400 ~"
            ]
        );
        assert!(commands(&args(json!({"clear_entities": ["minecraft:player"]}))).is_err());
    }

    /// A reset as the game answers a typical call: teleport, clear_entities, time, weather,
    /// gamemode, give, hotbar, perspective and warm_up.
    fn typical_reset() -> Reset {
        let (world, _) = fake::game(|line| {
            if line.starts_with("tp @e") {
                fake::failure("No targets matched selector")
            } else if line.starts_with("re_mcp:player_set") {
                fake::json_reply(json!({"write": 3}))
            } else if line.starts_with("re_mcp:result") {
                fake::json_reply(json!({"pending": false, "applied": ["teleport"], "rejected": {}}))
            } else if line == "re_mcp:ping" {
                fake::json_reply(json!({"version": 2, "tick": 5}))
            } else {
                json!({ "statusCode": 0, "statusMessage": format!("Ran {line}") })
            }
        });
        let place: Place = serde_json::from_value(json!({
            "x": -289.5, "y": 63, "z": -146.5, "yaw": 0, "pitch": 0,
            "dimension": "minecraft:overworld"
        }))
        .unwrap();
        let reset = args(json!({
            "clear_entities": true, "time": "noon", "weather": "clear", "gamemode": "survival",
            "give": [{"item": "demo:ruby_sword", "slot": 0}, {"item": "minecraft:apple", "count": 3}],
        }));
        let mut steps = Vec::new();
        teleport(&world, &place, &mut steps).unwrap();
        run(&world, &commands(&reset).unwrap(), &mut steps).unwrap();
        steps.push(
            json!({"step": "hotbar", "ok": true, "result": {"pending": false, "selectedSlot": 0}}),
        );
        steps.push(json!({"step": "perspective", "ok": true, "perspective": "third_person_back", "presses": 1}));
        steps.push(json!({
            "step": "warm_up", "ok": true, "done": true, "swapped_slot": 1, "walked": true,
            "attachable": {"animating": true, "players": ["controller.sword"], "attachables": 1},
            "attempts": 1, "detail": ""
        }));
        let player = json!({
            "source": "bridge", "name": "Steve", "id": "-4294967295",
            "dimension": "minecraft:overworld", "location": {"x": -289.5, "y": 63.0, "z": -146.5},
            "eye": {"x": -289.5, "y": 64.62, "z": -146.5}, "velocity": {"x": 0.0, "y": -0.08, "z": 0.0},
            "yaw": 0.0, "pitch": 0.0, "health": {"current": 20, "max": 20},
            "hunger": {"current": 20, "max": 20}, "gameMode": "Survival", "selectedSlot": 0,
            "effects": [], "tags": [],
            "flags": {"sneaking": false, "sprinting": false, "onGround": true, "flying": false,
                "gliding": false, "swimming": false, "inWater": false, "sleeping": false,
                "falling": false, "climbing": false},
            "spawnPoint": null, "level": 0
        });
        Reset {
            world: json!({"name": "Arena", "folder": "AbCdEfGhIjk="}),
            spot: Some(place),
            steps,
            player,
            problems: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn the_reply_holds_the_player_and_everything_when_verbose() {
        let brief = typical_reset().reply(false);
        assert_eq!(brief["ok"], true, "{brief}");
        for field in ["world", "spot", "steps"] {
            assert!(brief.get(field).is_none(), "{field}: {brief}");
        }
        assert_eq!(brief["spot_problems"], json!([]));
        assert_eq!(brief["notes"], json!([]));
        let player = brief["player"].as_object().unwrap();
        let mut fields: Vec<&str> = player.keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "dimension",
                "health",
                "location",
                "perspective",
                "pitch",
                "selectedSlot",
                "source",
                "yaw"
            ]
        );
        assert_eq!(player["perspective"], "third_person_back");
        assert_eq!(brief["warm_up"]["done"], true);
        assert_eq!(brief["warm_up"].as_object().unwrap().len(), 2);

        let verbose = typical_reset().reply(true);
        assert_eq!(verbose["steps"].as_array().unwrap().len(), 10, "{verbose}");
        assert_eq!(verbose["world"]["name"], "Arena");
        assert_eq!(verbose["spot"]["x"], -289.5);
        assert_eq!(verbose["player"]["flags"]["onGround"], true);
        assert_eq!(verbose["player"]["perspective"], "third_person_back");
    }

    #[test]
    fn ids_that_could_inject_arguments_are_refused() {
        assert!(commands(&args(json!({"give": [{"item": "stone 1 0 {}"}]}))).is_err());
        assert!(commands(&args(json!({"clear_entities": ["player"]}))).is_err());
        assert!(commands(&args(json!({"weather": "snow"}))).is_err());
    }

    #[test]
    fn empty_selectors_pass_and_the_first_real_failure_stops() {
        let (world, seen) = fake::game(|line| {
            if line.starts_with("tp @e") {
                fake::failure("No targets matched selector")
            } else if line.starts_with("give") {
                fake::failure("Unknown item")
            } else {
                json!({ "statusCode": 0, "statusMessage": "ok" })
            }
        });
        let args = args(json!({
            "clear_entities": true,
            "give": [{"item": "nope"}],
            "heal": true,
        }));
        let mut report = Vec::new();
        let error = run(&world, &commands(&args).unwrap(), &mut report)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("give") && error.contains("Unknown item"),
            "{error}"
        );
        assert_eq!(report.len(), 2);
        assert_eq!(report[0]["ok"], true);
        assert!(
            !seen
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.starts_with("effect")),
            "steps after the failure do not run"
        );
    }

    #[test]
    fn removing_nothing_passes_in_any_language() {
        // Each removal fails with a localized "nothing there" message; a typo still fails.
        let (world, _) = fake::game(|line| match line {
            "effect @s clear" => fake::failure("Konnte keine Effekte entfernen"),
            "clear @s" => fake::failure("Inventar ist leer"),
            _ if line.starts_with("tp @e") => fake::failure("Keine Ziele"),
            "time set noon" => json!({"statusCode": -2147483648, "statusMessage": "Syntax error"}),
            _ => json!({ "statusCode": 0, "statusMessage": "ok" }),
        });
        let args_all =
            args(json!({"clear_entities": true, "effects": [], "clear_inventory": true}));
        let mut report = Vec::new();
        run(&world, &commands(&args_all).unwrap(), &mut report).unwrap();
        assert_eq!(report.len(), 3);
        assert!(
            report
                .iter()
                .all(|step| step["ok"] == true && step["nothing_to_do"] == true)
        );
        let typo = args(json!({"time": "noon"}));
        assert!(run(&world, &commands(&typo).unwrap(), &mut Vec::new()).is_err());
    }

    #[test]
    fn the_spot_check_reports_blocks_inside_and_ahead() {
        let (world, _) = fake::game(|line| {
            if let Some(reply) = fake::bridge_basics(line) {
                return reply;
            }
            let reply = if line.contains("y=63.5") {
                json!({"block": {"id": "minecraft:stone", "location": {"x": -290, "y": 63, "z": -147}, "distance": 0.5}})
            } else {
                json!({"block": {"id": "minecraft:dirt", "location": {"x": -290, "y": 64, "z": -145}, "distance": 2}})
            };
            fake::json_reply(reply)
        });
        let player = json!({"location": {"x": -289.5, "y": 63, "z": -146.5}, "yaw": 0});
        let problems = check_spot(&world, &player, &mut Vec::new()).unwrap();
        assert!(
            problems[0].contains("inside minecraft:stone"),
            "{problems:?}"
        );
        assert!(problems[1].contains("blocks the way"), "{problems:?}");
    }

    #[test]
    fn without_the_pack_testforblock_checks_the_spot() {
        let (world, seen) = fake::game(|line| match line {
            _ if line.starts_with("re_mcp:") => fake::unknown(line),
            "testforblock -290 63 -145 air" => json!({
                "statusCode": -2147352576,
                "statusMessage": "The block at -290,63,-145 is Stone (expected: Air)."
            }),
            "testforblock -290 64 -147 air" => fake::failure("Hors du monde"),
            _ => json!({"statusCode": 0, "statusMessage": "Successfully found the block."}),
        });
        // Only commands answered: the eye, 1.62 above the feet at y 63, facing south (+z).
        let player = json!({"eye": {"x": -289.5, "y": 64.62, "z": -146.5}, "yaw": 0});
        let mut notes = Vec::new();
        let problems = check_spot(&world, &player, &mut notes).unwrap();
        assert_eq!(
            problems,
            ["Stone at -290,63,-145 blocks the way 2 blocks ahead at legs height."]
        );
        assert!(notes[0].contains("testforblock"), "{notes:?}");
        assert!(notes[1].contains("Hors du monde"), "{notes:?}");
        let seen = seen.lock().unwrap();
        let tests = seen
            .iter()
            .filter(|line| line.starts_with("testforblock"))
            .count();
        assert_eq!(tests, 8, "{seen:?}");
        assert!(seen.iter().all(|line| !line.starts_with("re_mcp:raycast")));
    }

    #[test]
    fn spots_are_found_in_the_world_written_last() {
        let dir = TestDir::new("scene");
        let worlds = dir.path().join("minecraftWorlds");
        for (folder, name) in [("old=", "Old"), ("new=", "New")] {
            fs::create_dir_all(worlds.join(folder).join("db")).unwrap();
            fs::write(worlds.join(folder).join("levelname.txt"), name).unwrap();
            fs::write(worlds.join(folder).join("level.dat"), b"").unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        fs::write(worlds.join("old=").join("db").join("000001.log"), b"x").unwrap();
        assert_eq!(scene_world(dir.path(), None).unwrap().folder, "old=");
        assert_eq!(scene_world(dir.path(), Some("New")).unwrap().folder, "new=");
    }
}

/**
 * bedrock-mcp Bridge: `re_mcp:*` commands that answer with JSON in the command's
 * status message, so the bedrock-mcp server reads game state over Bedrock's
 * WebSocket without anything showing in chat. New commands register only
 * when a world loads; `/reload` updates the code behind existing ones.
 *
 * Commands that act on a player take a trailing optional `player` name. The
 * bridge looks it up among world.getPlayers(), which includes GameTest
 * simulated players; without it the command acts on the player who ran it.
 *
 * Custom commands run in restricted execution, where the Script API refuses
 * writes. Writing commands (look, aim, hotbar, player_set) therefore queue a job
 * with system.run and answer `{"write": id}`; `re_mcp:result <id>` answers
 * `{"pending": true}` until the job ran, then the job's outcome.
 */
import {
	BlockComponentTypes,
	CommandPermissionLevel,
	Container,
	CustomCommandOrigin,
	CustomCommandParamType,
	CustomCommandResult,
	CustomCommandStatus,
	Dimension,
	EntityComponentTypes,
	EquipmentSlot,
	GameMode,
	LiquidType,
	Player,
	StartupEvent,
	system,
	world,
} from "@minecraft/server";
import * as describe from "./describe.ts";
import { poll, subscribe, weather } from "./events.ts";

const VERSION = 3;
const ENTITY_PAGE = 16;
const ENTITY_SNAPSHOT_LIMIT = 4096;
const SNAPSHOT_HISTORY = 16;
const WRITE_HISTORY = 64;
const EVENT_LIMIT = 128;
const DEFAULT_RADIUS = 32;
const RAYCAST_DISTANCE = 64;
const CONTAINER_PAGE = 27;
/** The most cells one `re_mcp:blocks` reads. */
const BLOCK_GRID_LIMIT = 4096;

type Parameter = { name: string; type: CustomCommandParamType };
type Vector = { x: number; y: number; z: number };

const PLAYER: Parameter = { name: "player", type: CustomCommandParamType.String };

function ok(value: unknown): CustomCommandResult {
	return { status: CustomCommandStatus.Success, message: JSON.stringify(value) };
}

function fail(message: string): CustomCommandResult {
	return { status: CustomCommandStatus.Failure, message };
}

/** Register `re_mcp:<name>`; the handler gets the calling player, then the mandatory and optional arguments. */
function register(
	event: StartupEvent,
	name: string,
	description: string,
	mandatory: Parameter[],
	optional: Parameter[],
	handler: (caller: Player, ...args: never[]) => CustomCommandResult,
): void {
	event.customCommandRegistry.registerCommand({
		name: `re_mcp:${name}`,
		description: `bedrock-mcp: ${description}`,
		permissionLevel: CommandPermissionLevel.Any,
		cheatsRequired: false,
		mandatoryParameters: mandatory,
		optionalParameters: optional,
	}, (origin: CustomCommandOrigin, ...args: unknown[]) => {
		const caller = origin.sourceEntity;
		if (!(caller instanceof Player)) return fail("Run as a player");
		try {
			return (handler as (caller: Player, ...args: unknown[]) => CustomCommandResult)(
				caller,
				...args,
			);
		} catch (error) {
			return fail(`re_mcp:${name} failed: ${String(error)}`);
		}
	});
}

/** The player a command acts on: `name` among world.getPlayers(), else the caller. */
function actor(caller: Player, name: string | undefined): Player {
	if (!name) return caller;
	const found = world.getPlayers({ name });
	if (found.length === 0) throw new Error(`No player named ${JSON.stringify(name)}`);
	return found[0];
}

function entityById(id: string) {
	const found = world.getEntity(id);
	if (!found) throw new Error(`No loaded entity with id ${id}`);
	return found;
}

/** `key=value;key=value` options of the commands that take many optional values. */
function options(spec: string | undefined): Map<string, string> {
	const parsed = new Map<string, string>();
	for (const pair of (spec ?? "").split(";")) {
		const at = pair.indexOf("=");
		if (at > 0) parsed.set(pair.slice(0, at).trim(), pair.slice(at + 1).trim());
	}
	return parsed;
}

function numberOption(parsed: Map<string, string>, key: string): number | undefined {
	const text = parsed.get(key);
	if (text === undefined) return undefined;
	const value = Number(text);
	if (!Number.isFinite(value)) throw new Error(`${key}=${text} is not a number`);
	return value;
}

function vectorOption(
	parsed: Map<string, string>,
	x: string,
	y: string,
	z: string,
): Vector | undefined {
	const values = [numberOption(parsed, x), numberOption(parsed, y), numberOption(parsed, z)];
	if (values.every((value) => value === undefined)) return undefined;
	if (values.some((value) => value === undefined)) {
		throw new Error(`Give all of ${x}, ${y} and ${z}`);
	}
	return { x: values[0]!, y: values[1]!, z: values[2]! };
}

/** The block or entity container the `x,y,z,dimension` or `entity` options name. */
function containerOf(caller: Player, parsed: Map<string, string>): Container {
	const entityId = parsed.get("entity");
	if (entityId) {
		const container = entityById(entityId).getComponent(EntityComponentTypes.Inventory)?.container;
		if (!container) throw new Error(`Entity ${entityId} has no inventory`);
		return container;
	}
	const location = vectorOption(parsed, "x", "y", "z");
	if (!location) throw new Error("Give a block location (x, y, z) or an entity id");
	const dimension = describe.dimensionOf(parsed.get("dimension"), caller.dimension);
	const block = dimension.getBlock(location);
	if (!block) throw new Error("That block is not loaded");
	const container = block.getComponent(BlockComponentTypes.Inventory)?.container;
	if (!container) {
		throw new Error(
			`${block.typeId} at ${location.x} ${location.y} ${location.z} has no inventory`,
		);
	}
	return container;
}

const snapshots = new Map<number, string[]>();
let lastSnapshot = 0;

function entityPage(snapshot: number, offset: number, limit: number, origin: Vector) {
	const ids = snapshots.get(snapshot);
	if (!ids) throw new Error(`Entity snapshot ${snapshot} expired; list again`);
	const end = Math.min(ids.length, offset + limit);
	const entities = ids.slice(offset, end).map((id) => {
		const found = world.getEntity(id);
		return found?.isValid ? describe.entity(found, origin) : { id, removed: true };
	});
	return { snapshot, total: ids.length, offset, entities, next: end < ids.length ? end : null };
}

const writes = new Map<number, Record<string, unknown> | null>();
let lastWrite = 0;

/** Queue a write for the next tick; `re_mcp:result` reports its outcome. */
function defer(job: () => Record<string, unknown>): CustomCommandResult {
	const id = ++lastWrite;
	writes.set(id, null);
	writes.delete(id - WRITE_HISTORY);
	system.run(() => {
		try {
			writes.set(id, job());
		} catch (error) {
			writes.set(id, { error: String(error) });
		}
	});
	return ok({ write: id });
}

/** Turn a player: setRotation does not turn players, a teleport in place does. */
function turn(target: Player, yaw: number, pitch: number) {
	target.teleport(target.location, { rotation: { x: pitch, y: yaw } });
	const rotation = target.getRotation();
	return { yaw: describe.round(rotation.y), pitch: describe.round(rotation.x) };
}

/** The Bedrock yaw and pitch that face `to` from `from`. */
function facing(from: Vector, to: Vector) {
	const [dx, dy, dz] = [to.x - from.x, to.y - from.y, to.z - from.z];
	const degrees = 180 / Math.PI;
	const pitch = -Math.atan2(dy, Math.hypot(dx, dz)) * degrees;
	return { yaw: Math.atan2(-dx, dz) * degrees, pitch: Math.max(-90, Math.min(90, pitch)) };
}

/** The unit view direction of a Bedrock yaw and pitch. */
function direction(yaw: number, pitch: number): Vector {
	const [y, p] = [yaw * Math.PI / 180, pitch * Math.PI / 180];
	return { x: -Math.sin(y) * Math.cos(p), y: -Math.sin(p), z: Math.cos(y) * Math.cos(p) };
}

/** The block and the first other entity along a player's sight from its eyes: along `along` when given, else its view direction. */
function sight(target: Player, along?: Vector) {
	const eye = target.getHeadLocation();
	const options = { maxDistance: RAYCAST_DISTANCE };
	const hit = describe.attempt(() =>
		along
			? target.dimension.getBlockFromRay(eye, along, options)
			: target.getBlockFromViewDirection(options)
	);
	const entities =
		(describe.attempt(() =>
			along
				? target.dimension.getEntitiesFromRay(eye, along, options)
				: target.getEntitiesFromViewDirection(options)
		) ??
			[]).filter((found) => found.entity.id !== target.id);
	return {
		maxDistance: RAYCAST_DISTANCE,
		block: hit
			? {
				...describe.block(hit.block),
				face: hit.face,
				faceLocation: describe.vector(hit.faceLocation),
				distance: describe.distance(hit.block.center(), eye),
			}
			: null,
		entity: entities.length
			? {
				...describe.entity(entities[0].entity, eye),
				distance: describe.round(entities[0].distance),
			}
			: null,
	};
}

/** Turn a player toward an entity's middle (halfway between its feet and eyes) or a point, measured from its eyes on the tick the job runs; the tick, the angles, and what lies along the new sight. With `turn=0` it only measures: yaw and pitch stay the player's own, since a teleport also stops a walking player. */
function aim(target: Player, parsed: Map<string, string>) {
	const id = parsed.get("entity");
	let point = vectorOption(parsed, "x", "y", "z");
	if (id) {
		const entity = entityById(id);
		const [feet, head] = [entity.location, entity.getHeadLocation()];
		point = { x: (feet.x + head.x) / 2, y: (feet.y + head.y) / 2, z: (feet.z + head.z) / 2 };
	}
	if (!point) throw new Error("Give entity, or x, y and z");
	const eye = target.getHeadLocation();
	const requested = facing(eye, point);
	const rotation = target.getRotation();
	const turned = parsed.get("turn") === "0"
		? { yaw: describe.round(rotation.y), pitch: describe.round(rotation.x) }
		: turn(target, requested.yaw, requested.pitch);
	return {
		tick: system.currentTick,
		name: target.name,
		gameMode: target.getGameMode(),
		requested: { yaw: describe.round(requested.yaw), pitch: describe.round(requested.pitch) },
		...turned,
		eye: describe.vector(eye),
		point: describe.vector(point),
		...sight(target, direction(requested.yaw, requested.pitch)),
	};
}

/** One letter per cell for the server's walking pathfinder: `.` open (air, or a block that flowing water washes away, which has no collision), `#` solid, `t` a fence or wall (too high to step onto), `w` water, `l` lava, `?` not loaded. */
function cellLetter(dimension: Dimension, location: Vector): string {
	const block = describe.attempt(() => dimension.getBlock(location));
	if (!block?.isValid) return "?";
	if (block.isAir) return ".";
	if (block.isLiquid) return block.typeId.includes("lava") ? "l" : "w";
	if (describe.attempt(() => block.canBeDestroyedByLiquidSpread(LiquidType.Water))) return ".";
	return /fence|_wall$/.test(block.typeId) ? "t" : "#";
}

/** The cells of the box at `x,y,z` spanning `dx,dy,dz` in the caller's dimension, x fastest, then z, then y. */
function blockGrid(dimension: Dimension, parsed: Map<string, string>) {
	const origin = vectorOption(parsed, "x", "y", "z");
	const size = vectorOption(parsed, "dx", "dy", "dz");
	if (!origin || !size) throw new Error("Give x, y, z and dx, dy, dz");
	const [sx, sy, sz] = [size.x, size.y, size.z].map(Math.floor);
	if (Math.min(sx, sy, sz) < 1 || sx * sy * sz > BLOCK_GRID_LIMIT) {
		throw new Error(`The box must hold 1 to ${BLOCK_GRID_LIMIT} cells`);
	}
	let cells = "";
	for (let y = 0; y < sy; y++) {
		for (let z = 0; z < sz; z++) {
			for (let x = 0; x < sx; x++) {
				cells += cellLetter(dimension, { x: origin.x + x, y: origin.y + y, z: origin.z + z });
			}
		}
	}
	return { origin: describe.vector(origin), size: [sx, sy, sz], cells };
}

/** Apply each `player_set` option on its own; a refused option lands in `rejected` with the game's reason. */
function setPlayer(target: Player, parsed: Map<string, string>) {
	const applied: string[] = [];
	const rejected: Record<string, string> = {};
	const step = (key: string, write: () => void) => {
		try {
			write();
			applied.push(key);
		} catch (error) {
			rejected[key] = String(error);
		}
	};
	const location = vectorOption(parsed, "x", "y", "z");
	const yaw = numberOption(parsed, "yaw");
	const pitch = numberOption(parsed, "pitch");
	const dimension = parsed.get("dimension");
	if (location || yaw !== undefined || pitch !== undefined || dimension) {
		step("teleport", () => {
			const rotation = target.getRotation();
			target.teleport(location ?? target.location, {
				dimension: dimension ? world.getDimension(dimension) : undefined,
				rotation: { x: pitch ?? rotation.x, y: yaw ?? rotation.y },
			});
		});
	}
	const mode = parsed.get("game_mode");
	if (mode) {
		step("game_mode", () => {
			const known = Object.values(GameMode) as string[];
			if (!known.includes(mode)) {
				throw new Error(`Unknown game mode ${mode}; use ${known.join(", ")}`);
			}
			target.setGameMode(mode as GameMode);
		});
	}
	const health = numberOption(parsed, "health");
	if (health !== undefined) {
		step("health", () => {
			const component = target.getComponent(EntityComponentTypes.Health);
			if (!component) throw new Error("This player has no health component");
			if (!component.setCurrentValue(health)) {
				throw new Error(
					`${health} is outside the health range ${component.effectiveMin} to ${component.effectiveMax}`,
				);
			}
		});
	}
	const velocity = vectorOption(parsed, "vx", "vy", "vz");
	if (velocity) {
		step("velocity", () => {
			target.clearVelocity();
			target.applyImpulse(velocity);
		});
	}
	return { applied, rejected };
}

system.beforeEvents.startup.subscribe((event) => {
	register(
		event,
		"ping",
		"bridge version and current tick",
		[],
		[],
		() => ok({ version: VERSION, tick: system.currentTick }),
	);
	register(
		event,
		"result",
		"the outcome of a queued write",
		[
			{ name: "write", type: CustomCommandParamType.Integer },
		],
		[],
		(_caller, id: number) => {
			if (!writes.has(id)) return fail(`Unknown or expired write ${id}`);
			const outcome = writes.get(id);
			return ok(outcome === null ? { pending: true } : { pending: false, ...outcome });
		},
	);
	register(
		event,
		"state",
		"a player's state",
		[],
		[PLAYER],
		(caller, name?: string) => ok(describe.player(actor(caller, name))),
	);
	register(
		event,
		"inventory",
		"a player's inventory and equipment",
		[],
		[PLAYER],
		(caller, name?: string) => ok(describe.inventory(actor(caller, name))),
	);
	register(
		event,
		"target",
		"the block and entity under a player's crosshair",
		[],
		[PLAYER],
		(caller, name?: string) => ok(sight(actor(caller, name))),
	);
	register(event, "entities", "entities near a player, nearest first, as a paged snapshot", [], [
		{ name: "radius", type: CustomCommandParamType.Float },
		{ name: "type", type: CustomCommandParamType.String },
		{ name: "limit", type: CustomCommandParamType.Integer },
		PLAYER,
	], (caller, radius?: number, type?: string, limit?: number, name?: string) => {
		const target = actor(caller, name);
		const origin = target.location;
		const found = target.dimension.getEntities({
			location: origin,
			maxDistance: radius ?? DEFAULT_RADIUS,
			type: type && type !== "*" ? type : undefined,
			closest: ENTITY_SNAPSHOT_LIMIT + 1,
		}).filter((entity) => entity.id !== target.id);
		const snapshot = ++lastSnapshot;
		snapshots.set(snapshot, found.slice(0, ENTITY_SNAPSHOT_LIMIT).map((entity) => entity.id));
		snapshots.delete(snapshot - SNAPSHOT_HISTORY);
		return ok({
			...entityPage(snapshot, 0, limit ?? ENTITY_PAGE, origin),
			truncated: found.length > ENTITY_SNAPSHOT_LIMIT,
		});
	});
	register(
		event,
		"entities_page",
		"more of an entity snapshot",
		[
			{ name: "snapshot", type: CustomCommandParamType.Integer },
			{ name: "offset", type: CustomCommandParamType.Integer },
			{ name: "limit", type: CustomCommandParamType.Integer },
		],
		[PLAYER],
		(caller, snapshot: number, offset: number, limit: number, name?: string) =>
			ok(entityPage(snapshot, offset, limit, actor(caller, name).location)),
	);
	register(
		event,
		"entity",
		"an entity's identity, transform, health, tags, families and flags",
		[
			{ name: "id", type: CustomCommandParamType.String },
		],
		[],
		(_caller, id: string) => ok(describe.entityDetail(entityById(id))),
	);
	register(
		event,
		"entity_components",
		"an entity's components and their readable fields",
		[
			{ name: "id", type: CustomCommandParamType.String },
		],
		[],
		(_caller, id: string) => {
			const found = entityById(id);
			return ok({ id, type: found.typeId, components: describe.components(found) });
		},
	);
	register(
		event,
		"entity_properties",
		"entity properties by name",
		[
			{ name: "id", type: CustomCommandParamType.String },
		],
		[{ name: "names", type: CustomCommandParamType.String }],
		(_caller, id: string, names?: string) => {
			const found = entityById(id);
			const properties: Record<string, unknown> = {};
			const undefinedProperties: string[] = [];
			for (
				const property of (names ?? "").split(",").map((part) => part.trim()).filter(Boolean)
			) {
				const value = describe.attempt(() => found.getProperty(property));
				if (value === null || value === undefined) undefinedProperties.push(property);
				else properties[property] = value;
			}
			return ok({ id, type: found.typeId, properties, undefinedProperties });
		},
	);
	register(
		event,
		"block",
		"the block at a position",
		[
			{ name: "location", type: CustomCommandParamType.Location },
		],
		[{ name: "dimension", type: CustomCommandParamType.String }],
		(caller, location: Vector, dimension?: string) => {
			const found = describe.dimensionOf(dimension, caller.dimension).getBlock(location);
			if (!found) return fail("That block is not loaded");
			return ok(describe.block(found));
		},
	);
	register(
		event,
		"raycast",
		"blocks and entities along a ray",
		[
			{ name: "spec", type: CustomCommandParamType.String },
		],
		[PLAYER],
		(caller, spec: string, name?: string) => {
			const target = actor(caller, name);
			const parsed = options(spec);
			const direction = vectorOption(parsed, "dx", "dy", "dz");
			if (!direction) return fail("Give a direction (dx, dy, dz)");
			const from = vectorOption(parsed, "x", "y", "z") ?? target.getHeadLocation();
			const maxDistance = numberOption(parsed, "max") ?? RAYCAST_DISTANCE;
			const hit = parsed.get("blocks") === "0"
				? undefined
				: describe.attempt(() =>
					target.dimension.getBlockFromRay(from, direction, { maxDistance })
				) ?? undefined;
			const entities = parsed.get("entities") === "0"
				? []
				: (describe.attempt(() =>
					target.dimension.getEntitiesFromRay(from, direction, { maxDistance })
				) ?? [])
					.filter((found) => found.entity.id !== target.id);
			return ok({
				from: describe.vector(from),
				direction: describe.vector(direction),
				maxDistance,
				block: hit
					? {
						...describe.block(hit.block),
						face: hit.face,
						faceLocation: describe.vector(hit.faceLocation),
						distance: describe.distance(hit.block.center(), from),
					}
					: null,
				entities: entities.map((found) => ({
					...describe.entity(found.entity),
					distance: describe.round(found.distance),
				})),
			});
		},
	);
	register(
		event,
		"blocks",
		"a grid of walkable, solid and liquid cells for paths",
		[
			{ name: "spec", type: CustomCommandParamType.String },
		],
		[],
		(caller, spec: string) => ok(blockGrid(caller.dimension, options(spec))),
	);
	register(
		event,
		"container",
		"a page of a block or entity container's slots",
		[
			{ name: "spec", type: CustomCommandParamType.String },
		],
		[],
		(caller, spec: string) => {
			const parsed = options(spec);
			const container = containerOf(caller, parsed);
			return ok(describe.containerPage(
				container,
				Math.max(0, numberOption(parsed, "offset") ?? 0),
				Math.max(1, numberOption(parsed, "limit") ?? CONTAINER_PAGE),
			));
		},
	);
	register(
		event,
		"item",
		"one item stack in detail",
		[
			{ name: "spec", type: CustomCommandParamType.String },
		],
		[PLAYER],
		(caller, spec: string, name?: string) => {
			const holder = actor(caller, name);
			const parsed = options(spec);
			const equipment = parsed.get("equipment");
			if (equipment) {
				const slots = describe.EQUIPMENT as string[];
				if (!slots.includes(equipment)) {
					return fail(
						`Unknown equipment slot ${equipment}; use ${slots.join(", ")}`,
					);
				}
				const stack = holder.getComponent(EntityComponentTypes.Equippable)?.getEquipment(
					equipment as EquipmentSlot,
				);
				return ok({ equipment, item: describe.itemDetail(stack, holder) });
			}
			const slot = numberOption(parsed, "slot");
			if (slot === undefined) return fail("Give a slot or an equipment slot");
			const container = parsed.has("entity") || parsed.has("x")
				? containerOf(caller, parsed)
				: holder.getComponent(EntityComponentTypes.Inventory)?.container;
			if (!container) return fail("No inventory to read");
			if (slot < 0 || slot >= container.size) {
				return fail(
					`Slot ${slot} is outside 0 to ${container.size - 1}`,
				);
			}
			return ok({ slot, item: describe.itemDetail(container.getItem(slot), holder) });
		},
	);
	register(
		event,
		"world",
		"time, weather, difficulty, game rules and spawn",
		[],
		[],
		(caller) =>
			ok({
				timeOfDay: world.getTimeOfDay(),
				absoluteTime: world.getAbsoluteTime(),
				day: world.getDay(),
				moonPhase: world.getMoonPhase(),
				weather,
				difficulty: world.getDifficulty(),
				defaultSpawn: world.getDefaultSpawnLocation(),
				dimension: caller.dimension.id,
				gameRules: describe.gameRules(),
			}),
	);
	register(event, "scores", "scoreboard objectives and their scores", [], [
		{ name: "objective", type: CustomCommandParamType.String },
	], (_caller, objective?: string) => {
		const objectives = objective
			? [world.scoreboard.getObjective(objective)].filter((found) => found !== undefined)
			: world.scoreboard.getObjectives();
		return ok(objectives.map((found) => ({
			id: found.id,
			displayName: found.displayName,
			scores: found.getScores().map((score) => ({
				participant: score.participant.displayName,
				type: score.participant.type,
				score: score.score,
			})),
		})));
	});
	register(event, "events", "script events after a cursor", [], [
		{ name: "since", type: CustomCommandParamType.Integer },
	], (_caller, since?: number) => ok(poll(since ?? 0, EVENT_LIMIT)));
	register(
		event,
		"look",
		"turn a player to an absolute yaw and pitch",
		[
			{ name: "yaw", type: CustomCommandParamType.Float },
			{ name: "pitch", type: CustomCommandParamType.Float },
		],
		[PLAYER],
		(caller, yaw: number, pitch: number, name?: string) => {
			const target = actor(caller, name);
			return defer(() => turn(target, yaw, pitch));
		},
	);
	register(
		event,
		"aim",
		"turn a player toward an entity or point from its eyes, and report its new sight",
		[
			{ name: "spec", type: CustomCommandParamType.String },
		],
		[PLAYER],
		(caller, spec: string, name?: string) => {
			const target = actor(caller, name);
			const parsed = options(spec);
			return defer(() => aim(target, parsed));
		},
	);
	register(
		event,
		"hotbar",
		"select a player's hotbar slot",
		[
			{ name: "slot", type: CustomCommandParamType.Integer },
		],
		[PLAYER],
		(caller, slot: number, name?: string) => {
			const target = actor(caller, name);
			if (slot < 0 || slot > 8) {
				return fail(`Hotbar slot ${slot} is outside 0 to 8`);
			}
			return defer(() => {
				target.selectedSlotIndex = slot;
				return { selectedSlot: target.selectedSlotIndex };
			});
		},
	);
	register(
		event,
		"player_set",
		"teleport, turn, change dimension, game mode, health or velocity",
		[
			{ name: "spec", type: CustomCommandParamType.String },
		],
		[PLAYER],
		(caller, spec: string, name?: string) => {
			const target = actor(caller, name);
			const parsed = options(spec);
			return defer(() => setPlayer(target, parsed));
		},
	);
});

subscribe();

/** `/scriptevent re_mcp:log <text>` writes `[bedrock-mcp] <text>` to the content log as a warning,
 * so tools can confirm they see content-log messages. */
system.afterEvents.scriptEventReceive.subscribe((event) => {
	if (event.id === "re_mcp:log") console.warn(`[bedrock-mcp] ${event.message}`);
});

/**
 * bedrock-mcp GameTest: `re_mcp:gametest_*` commands that spawn, list, drive and remove GameTest
 * simulated players for the bedrock-mcp server. Each command answers with JSON in its status
 * message, like the bedrock-mcp Bridge. The world needs the Beta APIs experiment, because
 * @minecraft/server-gametest is a beta module.
 *
 * Custom command callbacks run in restricted execution, where simulated player methods throw.
 * So spawn, remove and act schedule their work with system.run and answer with a ticket;
 * `re_mcp:gametest_result <ticket>` answers with the outcome once the work ran.
 */
import {
	CommandPermissionLevel,
	CustomCommandOrigin,
	CustomCommandParamType,
	CustomCommandResult,
	CustomCommandStatus,
	Direction,
	Entity,
	EntityComponentTypes,
	GameMode,
	ItemStack,
	StartupEvent,
	Vector3,
	system,
	world,
} from "@minecraft/server";
import {
	LookDuration,
	NavigationResult,
	SimulatedPlayer,
	spawnSimulatedPlayer,
} from "@minecraft/server-gametest";

const VERSION = 2;
/** Outcomes kept for `re_mcp:gametest_result`; older ones are dropped first. */
const OUTCOME_LIMIT = 256;
/** Ticks a new simulated player gets to settle (join items, collisions) before it is described. */
const SPAWN_SETTLE_TICKS = 10;
/** Hurt ticks remembered per entity; older entries are dropped first. */
const HURT_LIMIT = 1024;
/** Actions a dead simulated player can still take. */
const DEAD_ACTIONS: Record<string, true> = {
	respawn: true,
	chat: true,
	select_slot: true,
	set_item: true,
	give_item: true,
	stop_moving: true,
	stop_using_item: true,
	stop_breaking_block: true,
};

type Parameter = { name: string; type: CustomCommandParamType };
type Outcome = { done: true; ok: true; value: unknown } | { done: true; ok: false; error: string };

const outcomes = new Map<number, Outcome>();
let nextTicket = 1;
/** Simulated players this pack spawned, by name. */
const spawned = new Map<string, SimulatedPlayer>();
/** The tick each entity was last hurt, by entity id. */
const lastHurt = new Map<string, number>();

world.afterEvents.entityHurt.subscribe(({ hurtEntity }) => {
	const id = attempt(() => hurtEntity.id);
	if (id === null) return;
	lastHurt.delete(id);
	lastHurt.set(id, system.currentTick);
	while (lastHurt.size > HURT_LIMIT) lastHurt.delete(lastHurt.keys().next().value!);
});

function ok(value: unknown): CustomCommandResult {
	return { status: CustomCommandStatus.Success, message: JSON.stringify(value) };
}

function fail(message: string): CustomCommandResult {
	return { status: CustomCommandStatus.Failure, message };
}

function attempt<T>(read: () => T): T | null {
	try {
		return read();
	} catch {
		return null;
	}
}

function round(value: number): number {
	return Math.round(value * 1000) / 1000;
}

function vector(value: Vector3) {
	return { x: round(value.x), y: round(value.y), z: round(value.z) };
}

function item(stack: ItemStack | undefined) {
	return stack ? { id: stack.typeId, amount: stack.amount } : null;
}

/** Whether a player is alive: a dead one stays in the world with no health until it respawns. */
function alive(player: SimulatedPlayer): boolean {
	const health = attempt(() => player.getComponent(EntityComponentTypes.Health));
	return !health || health.currentValue > 0;
}

/** What `re_mcp:gametest_list` and every action report about a simulated player. */
function describe(player: SimulatedPlayer) {
	const rotation = player.getRotation();
	const head = attempt(() => player.headRotation);
	const eye = attempt(() => player.getHeadLocation());
	const health = attempt(() => player.getComponent(EntityComponentTypes.Health));
	const container = attempt(() => player.getComponent(EntityComponentTypes.Inventory)?.container);
	return {
		name: player.name,
		id: player.id,
		dimension: player.dimension.id,
		location: vector(player.location),
		eye: eye ? vector(eye) : null,
		yaw: round(rotation.y),
		pitch: round(rotation.x),
		headYaw: head ? round(head.y) : null,
		headPitch: head ? round(head.x) : null,
		health: health ? { current: round(health.currentValue), max: health.effectiveMax } : null,
		alive: alive(player),
		gameMode: player.getGameMode(),
		selectedSlot: player.selectedSlotIndex,
		selectedItem: item(attempt(() => container?.getItem(player.selectedSlotIndex)) ?? undefined),
		sprinting: player.isSprinting,
		onGround: player.isOnGround,
	};
}

/** What decides whether an attack lands on `target`, read before the attack. */
function targetState(target: Entity) {
	const health = attempt(() => target.getComponent(EntityComponentTypes.Health));
	const hurt = lastHurt.get(target.id);
	return {
		id: target.id,
		valid: target.isValid,
		health: health ? round(health.currentValue) : null,
		hurtTicksAgo: hurt === undefined ? null : system.currentTick - hurt,
	};
}

/**
 * Drop `amount` items from the selected slot. dropSelectedItem drops the whole stack, so a
 * smaller amount is split off first: the slot holds just that part while it drops, then the
 * rest goes back.
 */
function dropAmount(player: SimulatedPlayer, amount: number): boolean {
	const container = player.getComponent(EntityComponentTypes.Inventory)?.container;
	const slot = player.selectedSlotIndex;
	const stack = container?.getItem(slot);
	if (!container || !stack) return false;
	if (!Number.isInteger(amount) || amount < 1) throw new Error(`Cannot drop ${amount} items`);
	if (amount >= stack.amount) return player.dropSelectedItem();
	const part = stack.clone();
	part.amount = amount;
	const rest = stack.clone();
	rest.amount = stack.amount - amount;
	container.setItem(slot, part);
	const dropped = player.dropSelectedItem();
	container.setItem(slot, dropped ? rest : stack);
	return dropped;
}

/** Every simulated player in the world: the ones this pack spawned and any a test spawned. */
function simulatedPlayers(): SimulatedPlayer[] {
	for (const [name, player] of spawned) {
		if (!player.isValid) spawned.delete(name);
	}
	const found = new Map<string, SimulatedPlayer>(spawned);
	for (const player of world.getPlayers()) {
		if (player instanceof SimulatedPlayer && !found.has(player.name)) {
			found.set(player.name, player);
		}
	}
	return [...found.values()];
}

function simulatedPlayer(name: string): SimulatedPlayer {
	const player = simulatedPlayers().find((candidate) => candidate.name === name);
	if (!player) throw new Error(`No simulated player named ${JSON.stringify(name)}`);
	return player;
}

/**
 * Run `work` outside restricted execution, on the next tick or on game tick `at` when that is
 * later; answer with its ticket. Work that returns a promise finishes when the promise settles.
 */
function schedule(work: () => unknown, at?: number): CustomCommandResult {
	const ticket = nextTicket++;
	const settle = (outcome: Outcome) => {
		outcomes.set(ticket, outcome);
		while (outcomes.size > OUTCOME_LIMIT) outcomes.delete(outcomes.keys().next().value!);
	};
	const delay = at === undefined ? 0 : at - system.currentTick;
	system.runTimeout(() => {
		let value: unknown;
		try {
			value = work();
		} catch (error) {
			settle({ done: true, ok: false, error: String(error) });
			return;
		}
		Promise.resolve(value).then(
			(settled) => settle({ done: true, ok: true, value: settled }),
			(error) => settle({ done: true, ok: false, error: String(error) }),
		);
	}, Math.max(delay, 0));
	return ok({ ticket });
}

const BASE64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/** Decode unpadded base64url UTF-8 text. */
function decodeBase64Url(text: string): string {
	const bytes: number[] = [];
	let buffer = 0;
	let bits = 0;
	for (const char of text) {
		const digit = BASE64URL.indexOf(char);
		if (digit < 0) throw new Error(`Invalid base64url character ${JSON.stringify(char)}`);
		buffer = (buffer << 6) | digit;
		bits += 6;
		if (bits >= 8) {
			bits -= 8;
			bytes.push((buffer >> bits) & 0xff);
		}
	}
	let decoded = "";
	for (let index = 0; index < bytes.length;) {
		const lead = bytes[index++];
		const extra = lead >= 0xf0 ? 3 : lead >= 0xe0 ? 2 : lead >= 0xc0 ? 1 : 0;
		let point = extra ? lead & (0x3f >> extra) : lead;
		for (let count = 0; count < extra; count++) point = (point << 6) | (bytes[index++] & 0x3f);
		decoded += String.fromCodePoint(point);
	}
	return decoded;
}

/** Reach counts to an entity's surface: half a typical body width off its centre line. */
const BODY_RADIUS = 0.3;

/** How far a player's eyes are from an entity's body: the line from its feet to its eyes. */
function reachGap(player: SimulatedPlayer, target: Entity): number {
	const eye = player.getHeadLocation();
	const feet = target.location;
	const head = attempt(() => target.getHeadLocation()) ?? feet;
	const y = Math.min(Math.max(eye.y, Math.min(feet.y, head.y)), Math.max(feet.y, head.y));
	const gap = Math.hypot(eye.x - feet.x, eye.y - y, eye.z - feet.z) - BODY_RADIUS;
	return Math.max(gap, 0);
}

/** The reach `reach` ({survival, creative}) gives this player, or undefined without one. */
function reachLimit(player: SimulatedPlayer, reach: unknown): number | undefined {
	const { survival, creative } = (reach ?? {}) as { survival?: number; creative?: number };
	if (typeof survival !== "number" || typeof creative !== "number") return undefined;
	return player.getGameMode() === GameMode.Creative ? creative : survival;
}

const FACES: Record<string, Direction> = {
	down: Direction.Down,
	up: Direction.Up,
	north: Direction.North,
	south: Direction.South,
	east: Direction.East,
	west: Direction.West,
};

const DURATIONS: Record<string, LookDuration> = {
	instant: LookDuration.Instant,
	continuous: LookDuration.Continuous,
	until_move: LookDuration.UntilMove,
};

const GAME_MODES: Record<string, GameMode> = {
	survival: GameMode.Survival,
	creative: GameMode.Creative,
	adventure: GameMode.Adventure,
	spectator: GameMode.Spectator,
};

/** An action as the bedrock-mcp server's `gametest::Action` serializes it. */
type Action = Record<string, unknown> & { action: string };

function lookup<T>(table: Record<string, T>, key: unknown, what: string): T | undefined {
	if (key === undefined || key === null) return undefined;
	const found = table[String(key)];
	if (found === undefined) throw new Error(`Unknown ${what} ${JSON.stringify(key)}`);
	return found;
}

function entity(id: unknown): Entity {
	const found = world.getEntity(String(id));
	if (!found) throw new Error(`No entity with id ${JSON.stringify(id)}`);
	return found;
}

function location(value: unknown): Vector3 {
	const { x, y, z } = (value ?? {}) as Partial<Vector3>;
	if (typeof x !== "number" || typeof y !== "number" || typeof z !== "number") {
		throw new Error(`Expected a location {x, y, z}, got ${JSON.stringify(value)}`);
	}
	return { x, y, z };
}

/** Call the SimulatedPlayer method `action` names; its return value, or null for void methods. */
function perform(player: SimulatedPlayer, action: Action): unknown {
	const speed = action.speed as number | undefined;
	const face = () => lookup(FACES, action.face, "face");
	const duration = () => lookup(DURATIONS, action.duration, "look duration");
	const moveOptions = () => ({ faceTarget: action.face_target as boolean | undefined, speed });
	const stack = () => new ItemStack(String(action.item), action.amount as number | undefined);
	const navigation = (result: NavigationResult) => ({
		isFullPath: result.isFullPath,
		path: result.getPath().map(vector),
	});
	switch (action.action) {
		case "look_at_location":
			player.lookAtLocation(location(action.location), duration());
			break;
		case "look_at_entity":
			player.lookAtEntity(entity(action.entity), duration());
			break;
		case "look_at_block":
			player.lookAtBlock(location(action.block), duration());
			break;
		case "move_to_location":
			player.moveToLocation(location(action.location), moveOptions());
			break;
		case "move_to_block":
			player.moveToBlock(location(action.block), moveOptions());
			break;
		case "navigate_to_location":
			return navigation(player.navigateToLocation(location(action.location), speed));
		case "navigate_to_block":
			return navigation(player.navigateToBlock(location(action.block), speed));
		case "navigate_to_entity":
			return navigation(player.navigateToEntity(entity(action.entity), speed));
		case "move":
			player.move(Number(action.west_east), Number(action.north_south), speed);
			break;
		case "move_relative":
			player.moveRelative(Number(action.left_right), Number(action.backward_forward), speed);
			break;
		case "stop_moving":
			player.stopMoving();
			break;
		case "jump":
			return player.jump();
		case "attack":
			return player.attack();
		case "attack_entity":
			return player.attackEntity(entity(action.entity));
		case "interact":
			return player.interact();
		case "interact_with_block":
			return player.interactWithBlock(location(action.block), face());
		case "interact_with_entity":
			return player.interactWithEntity(entity(action.entity));
		case "use_item_in_slot":
			return player.useItemInSlot(Number(action.slot));
		case "use_item_in_slot_on_block":
			return player.useItemInSlotOnBlock(
				Number(action.slot),
				location(action.block),
				face(),
				action.face_location ? location(action.face_location) : undefined,
			);
		case "stop_using_item":
			return item(player.stopUsingItem());
		case "break_block":
			return player.breakBlock(location(action.block), face());
		case "stop_breaking_block":
			player.stopBreakingBlock();
			break;
		case "drop_selected_item":
			return player.dropSelectedItem();
		case "drop_amount":
			return dropAmount(player, Number(action.amount));
		case "select_slot":
			player.selectedSlotIndex = Number(action.slot);
			break;
		case "set_item":
			return player.setItem(stack(), Number(action.slot), action.select as boolean | undefined);
		case "give_item":
			return player.giveItem(stack(), action.select as boolean | undefined);
		case "chat":
			player.chat(String(action.message));
			break;
		case "respawn":
			return player.respawn();
		default:
			throw new Error(`Unknown action ${JSON.stringify(action.action)}`);
	}
	return null;
}

/** Register `re_mcp:<name>`; the handler gets the command's origin. */
function register(
	event: StartupEvent,
	name: string,
	description: string,
	mandatory: Parameter[],
	optionalParameters: Parameter[],
	handler: (origin: CustomCommandOrigin, ...args: never[]) => CustomCommandResult,
): void {
	event.customCommandRegistry.registerCommand({
		name: `re_mcp:${name}`,
		description: `bedrock-mcp GameTest: ${description}`,
		permissionLevel: CommandPermissionLevel.GameDirectors,
		cheatsRequired: true,
		mandatoryParameters: mandatory,
		optionalParameters: optionalParameters,
	}, (origin: CustomCommandOrigin, ...args: unknown[]) => {
		try {
			return (handler as (origin: CustomCommandOrigin, ...args: unknown[]) => CustomCommandResult)(
				origin,
				...args,
			);
		} catch (error) {
			return fail(`re_mcp:${name} failed: ${String(error)}`);
		}
	});
}

system.beforeEvents.startup.subscribe((event) => {
	const name = { name: "name", type: CustomCommandParamType.String };
	register(
		event,
		"gametest_ping",
		"pack version and current tick",
		[],
		[],
		() => ok({ version: VERSION, tick: system.currentTick }),
	);
	register(
		event,
		"gametest_list",
		"every simulated player",
		[],
		[],
		() => ok({ players: simulatedPlayers().map(describe) }),
	);
	register(event, "gametest_spawn", "spawn a simulated player (answers with a ticket)", [name], [
		{ name: "location", type: CustomCommandParamType.Location },
		{ name: "gameMode", type: CustomCommandParamType.String },
	], (origin, playerName: string, at?: Vector3, gameMode?: string) => {
		const source = origin.sourceEntity;
		const dimension = source?.dimension ?? world.getDimension("overworld");
		const spot = at ?? source?.location;
		if (!spot) return fail("Give a location: the command has no source entity");
		const mode = lookup(GAME_MODES, gameMode || "survival", "game mode")!;
		return schedule(async () => {
			if (simulatedPlayers().some((player) => player.name === playerName)) {
				throw new Error(`A simulated player named ${JSON.stringify(playerName)} already exists`);
			}
			const player = spawnSimulatedPlayer({ dimension, ...spot }, playerName, mode);
			spawned.set(playerName, player);
			await system.waitTicks(SPAWN_SETTLE_TICKS);
			return describe(player);
		});
	});
	register(
		event,
		"gametest_remove",
		"disconnect a simulated player (answers with a ticket)",
		[name],
		[],
		(
			_origin,
			playerName: string,
		) =>
			schedule(() => {
				simulatedPlayer(playerName).disconnect();
				spawned.delete(playerName);
				return { removed: playerName };
			}),
	);
	register(
		event,
		"gametest_act",
		"call a SimulatedPlayer method given as base64url JSON, now or at a game tick (answers with a ticket)",
		[name, { name: "action", type: CustomCommandParamType.String }],
		[{ name: "at", type: CustomCommandParamType.Integer }],
		(_origin, playerName: string, encoded: string, at?: number) => {
			// Null fields are absent: the API's optional parameters take undefined.
			const action = JSON.parse(
				decodeBase64Url(encoded),
				(_key, value) => value ?? undefined,
			) as Action;
			return schedule(() => {
				const player = simulatedPlayer(playerName);
				if (!alive(player) && !DEAD_ACTIONS[action.action]) {
					throw new Error(`Simulated player ${JSON.stringify(playerName)} is dead`);
				}
				const tick = system.currentTick;
				const aimed = action.action === "attack_entity" || action.action === "interact_with_entity"
					? entity(action.entity)
					: undefined;
				const target = action.action === "attack_entity" && aimed ? targetState(aimed) : undefined;
				const limit = aimed ? reachLimit(player, action.reach) : undefined;
				const distance = aimed ? round(reachGap(player, aimed)) : undefined;
				if (limit !== undefined && distance !== undefined && distance > limit) {
					return {
						action: action.action,
						result: false,
						refused: "reach",
						distance,
						reach: limit,
						player: describe(player),
						target,
						tick,
					};
				}
				const result = perform(player, action);
				return { action: action.action, result, player: describe(player), target, distance, tick };
			}, at);
		},
	);
	register(
		event,
		"gametest_result",
		"the outcome of a ticket, once its work ran",
		[{ name: "ticket", type: CustomCommandParamType.Integer }],
		[],
		(_origin, ticket: number) => {
			const outcome = outcomes.get(ticket);
			if (!outcome) {
				return ticket > 0 && ticket < nextTicket
					? ok({ done: false })
					: fail(`Unknown ticket ${ticket}`);
			}
			outcomes.delete(ticket);
			return ok(outcome);
		},
	);
});

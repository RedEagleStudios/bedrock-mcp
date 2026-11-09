import { Entity, EntityComponentTypes, Player, system, world } from "@minecraft/server";
import { attempt, block, item, vector } from "./describe.ts";

/** Events kept for polling; older ones are dropped first. */
const CAPACITY = 2048;

/** Identifies this script instance: sequence numbers restart when scripts reload. */
const EPOCH = `${Date.now().toString(36)}${Math.floor(Math.random() * 0xffffff).toString(36)}`;

type Event = { seq: number; tick: number; type: string; [key: string]: unknown };

const events: Event[] = [];
let sequence = 0;
export let weather: string | null = null;

function push(type: string, details: Record<string, unknown>): void {
	sequence += 1;
	// The event's own type goes last, so a detail field can never replace it.
	events.push({ ...details, seq: sequence, tick: system.currentTick, type });
	if (events.length > CAPACITY) events.shift();
}

/** An acting or affected entity: id, type, name (player name or name tag) and location. */
function who(entity: Entity | undefined) {
	if (!entity) return null;
	return {
		id: attempt(() => entity.id),
		type: attempt(() => entity.typeId),
		name: attempt(() => (entity instanceof Player ? entity.name : entity.nameTag || null)),
		location: attempt(() => vector(entity.location)),
	};
}

/** Events after `since`, at most `limit`, plus the cursor for the next call and the newest
 * sequence number (`latest`), so a caller can start from now. */
export function poll(since: number, limit: number) {
	const first = events.length ? events[0].seq : sequence + 1;
	const newer = events.filter((event) => event.seq > since).slice(0, limit);
	return {
		epoch: EPOCH,
		latest: sequence,
		events: newer,
		next: newer.length ? newer[newer.length - 1].seq : Math.max(since, sequence),
		dropped: since + 1 < first && since < sequence ? first - since - 1 : 0,
	};
}

/** The item an item entity carries, for drop confirmation. */
function carried(entity: Entity) {
	if (attempt(() => entity.typeId) !== "minecraft:item") return null;
	return item(
		attempt(() => entity.getComponent(EntityComponentTypes.Item)?.itemStack) ?? undefined,
	);
}

export function subscribe(): void {
	const after = world.afterEvents;
	after.entityDie.subscribe((event) =>
		push("entity_die", {
			entity: who(event.deadEntity),
			cause: event.damageSource.cause,
			killer: who(event.damageSource.damagingEntity),
		})
	);
	after.entityHurt.subscribe((event) =>
		push("entity_hurt", {
			entity: who(event.hurtEntity),
			damage: event.damage,
			cause: event.damageSource.cause,
			attacker: who(event.damageSource.damagingEntity),
		})
	);
	after.entityHitEntity.subscribe((event) =>
		push("entity_hit_entity", { attacker: who(event.damagingEntity), target: who(event.hitEntity) })
	);
	after.entityHitBlock.subscribe((event) =>
		push("entity_hit_block", {
			attacker: who(event.damagingEntity),
			block: event.hitBlockPermutation.type.id,
			location: vector(event.hitBlock.location),
			face: event.blockFace,
		})
	);
	after.entitySpawn.subscribe((event) =>
		push("entity_spawn", {
			entity: who(event.entity),
			cause: event.cause,
			item: carried(event.entity),
		})
	);
	after.entityRemove.subscribe((event) =>
		push("entity_remove", { id: event.removedEntityId, entityType: event.typeId })
	);
	after.playerBreakBlock.subscribe((event) =>
		push("block_break", {
			player: who(event.player),
			block: event.brokenBlockPermutation.type.id,
			location: vector(event.block.location),
			tool: item(event.itemStackBeforeBreak),
		})
	);
	after.playerPlaceBlock.subscribe((event) =>
		push("block_place", { player: who(event.player), block: block(event.block) })
	);
	after.playerInteractWithBlock.subscribe((event) =>
		push("player_interact_block", {
			player: who(event.player),
			block: block(event.block),
			face: event.blockFace,
			item: item(event.beforeItemStack),
			first: event.isFirstEvent,
		})
	);
	after.playerInteractWithEntity.subscribe((event) =>
		push("player_interact_entity", {
			player: who(event.player),
			target: who(event.target),
			item: item(event.beforeItemStack),
		})
	);
	after.itemUse.subscribe((event) =>
		push("item_use", { player: who(event.source), item: item(event.itemStack) })
	);
	after.itemStartUse.subscribe((event) =>
		push("item_start_use", {
			player: who(event.source),
			item: item(event.itemStack),
			duration: event.useDuration,
		})
	);
	after.itemStopUse.subscribe((event) =>
		push("item_stop_use", {
			player: who(event.source),
			item: item(event.itemStack),
			duration: event.useDuration,
		})
	);
	after.itemReleaseUse.subscribe((event) =>
		push("item_release_use", {
			player: who(event.source),
			item: item(event.itemStack),
			duration: event.useDuration,
		})
	);
	after.itemCompleteUse.subscribe((event) =>
		push("item_complete_use", {
			player: who(event.source),
			item: item(event.itemStack),
			duration: event.useDuration,
		})
	);
	after.itemStartUseOn.subscribe((event) =>
		push("item_start_use_on", {
			player: who(event.source),
			item: item(event.itemStack),
			block: block(event.block),
			face: event.blockFace,
		})
	);
	after.itemStopUseOn.subscribe((event) =>
		push("item_stop_use_on", {
			player: who(event.source),
			item: item(event.itemStack),
			block: block(event.block),
		})
	);
	after.playerInventoryItemChange.subscribe((event) =>
		push("inventory_change", {
			player: who(event.player),
			inventory: event.inventoryType,
			slot: event.slot,
			before: item(event.beforeItemStack),
			after: item(event.itemStack),
		})
	);
	after.playerHotbarSelectedSlotChange.subscribe((event) =>
		push("hotbar_select", {
			player: who(event.player),
			from: event.previousSlotSelected,
			to: event.newSlotSelected,
			item: item(event.itemStack),
		})
	);
	after.projectileHitBlock.subscribe((event) =>
		push("projectile_hit_block", {
			projectile: event.projectile.typeId,
			source: who(event.source),
			location: vector(event.location),
		})
	);
	after.projectileHitEntity.subscribe((event) =>
		push("projectile_hit_entity", {
			projectile: event.projectile.typeId,
			source: who(event.source),
			target: who(event.getEntityHit().entity),
		})
	);
	after.blockContainerOpened.subscribe((event) =>
		push("container_open", { block: block(event.block) })
	);
	after.blockContainerClosed.subscribe((event) =>
		push("container_close", { block: block(event.block) })
	);
	after.playerDimensionChange.subscribe((event) =>
		push("dimension_change", {
			player: who(event.player),
			from: event.fromDimension.id,
			to: event.toDimension.id,
		})
	);
	after.playerSpawn.subscribe((event) =>
		push("player_spawn", { player: who(event.player), initial: event.initialSpawn })
	);
	after.weatherChange.subscribe((event) => {
		weather = event.newWeather;
		push("weather_change", { from: event.previousWeather, to: event.newWeather });
	});
	after.gameRuleChange.subscribe((event) =>
		push("game_rule_change", { rule: event.rule, value: event.value })
	);
}

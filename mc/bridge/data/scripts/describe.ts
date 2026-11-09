import {
	Block,
	BlockComponentTypes,
	Container,
	Dimension,
	Entity,
	EntityComponentTypes,
	EquipmentSlot,
	ItemComponentTypes,
	ItemStack,
	Player,
	world,
} from "@minecraft/server";

type Vector = { x: number; y: number; z: number };

/** Read a value, or null when the API refuses (unloaded, removed, not allowed). */
export function attempt<T>(read: () => T): T | null {
	try {
		return read();
	} catch {
		return null;
	}
}

export function round(value: number): number {
	return Math.round(value * 1000) / 1000;
}

export function vector(value: Vector) {
	return { x: round(value.x), y: round(value.y), z: round(value.z) };
}

export function distance(a: Vector, b: Vector): number {
	return round(Math.hypot(a.x - b.x, a.y - b.y, a.z - b.z));
}

function isVector(value: unknown): value is Vector {
	if (typeof value !== "object" || value === null) return false;
	const candidate = value as Record<string, unknown>;
	return ["x", "y", "z"].every((axis) => typeof candidate[axis] === "number");
}

/** A JSON-safe copy of a plain value: primitives, vectors and arrays of them; null otherwise. */
function plain(value: unknown, depth = 0): unknown {
	if (value === undefined || value === null) return null;
	if (typeof value === "number") return Number.isFinite(value) ? round(value) : String(value);
	if (typeof value === "string" || typeof value === "boolean") return value;
	if (isVector(value)) return vector(value);
	if (Array.isArray(value) && depth < 2) return value.map((entry) => plain(entry, depth + 1));
	if (typeof value === "object" && depth < 2) {
		const record = value as Record<string, unknown>;
		if (typeof record.id === "string") return record.id;
		if (typeof record.typeId === "string") return record.typeId;
	}
	return undefined;
}

/** Every readable non-function property on an API object's prototype chain, as plain values. */
export function readable(source: object): Record<string, unknown> {
	const fields: Record<string, unknown> = {};
	const record = source as Record<string, unknown>;
	for (
		let prototype = Object.getPrototypeOf(source);
		prototype && prototype !== Object.prototype;
	) {
		for (const key of Object.getOwnPropertyNames(prototype)) {
			if (key === "constructor" || key in fields) continue;
			const value = attempt(() => record[key]);
			if (typeof value === "function") continue;
			const copy = plain(value);
			if (copy !== undefined) fields[key] = copy;
		}
		prototype = Object.getPrototypeOf(prototype);
	}
	return fields;
}

export function item(stack: ItemStack | undefined) {
	if (!stack) return null;
	const durability = attempt(() => stack.getComponent(ItemComponentTypes.Durability));
	const enchantable = attempt(() => stack.getComponent(ItemComponentTypes.Enchantable));
	return {
		id: stack.typeId,
		amount: stack.amount,
		name: stack.nameTag ?? null,
		lore: attempt(() => stack.getLore()) ?? [],
		damage: durability ? durability.damage : null,
		maxDurability: durability ? durability.maxDurability : null,
		enchantments: enchantable
			? (attempt(() => enchantable.getEnchantments()) ?? []).map((enchantment) => ({
				id: enchantment.type.id,
				level: enchantment.level,
			}))
			: [],
	};
}

/** Everything the stable API tells about an item stack; `holder` reads its cooldown. */
export function itemDetail(stack: ItemStack | undefined, holder?: Player) {
	if (!stack) return null;
	const component = <T>(read: () => T | undefined) => attempt(read) ?? null;
	const durability = component(() => stack.getComponent(ItemComponentTypes.Durability));
	const enchantable = component(() => stack.getComponent(ItemComponentTypes.Enchantable));
	const food = component(() => stack.getComponent(ItemComponentTypes.Food));
	const cooldown = component(() => stack.getComponent(ItemComponentTypes.Cooldown));
	const potion = component(() => stack.getComponent(ItemComponentTypes.Potion));
	return {
		id: stack.typeId,
		amount: stack.amount,
		maxAmount: stack.maxAmount,
		stackable: stack.isStackable,
		name: stack.nameTag ?? null,
		lore: attempt(() => stack.getLore()) ?? [],
		tags: attempt(() => stack.getTags()) ?? [],
		canDestroy: attempt(() => stack.getCanDestroy()) ?? [],
		canPlaceOn: attempt(() => stack.getCanPlaceOn()) ?? [],
		keepOnDeath: stack.keepOnDeath,
		lockMode: stack.lockMode,
		components: (attempt(() => stack.getComponents()) ?? []).map((found) => found.typeId),
		durability: durability
			? {
				damage: durability.damage,
				max: durability.maxDurability,
				unbreakable: durability.unbreakable,
			}
			: null,
		enchantable: enchantable
			? {
				slots: enchantable.slots,
				enchantments: (attempt(() => enchantable.getEnchantments()) ?? []).map((enchantment) => ({
					id: enchantment.type.id,
					level: enchantment.level,
					maxLevel: enchantment.type.maxLevel,
				})),
			}
			: null,
		food: food
			? {
				nutrition: food.nutrition,
				saturationModifier: food.saturationModifier,
				canAlwaysEat: food.canAlwaysEat,
				usingConvertsTo: food.usingConvertsTo || null,
			}
			: null,
		cooldown: cooldown
			? {
				category: cooldown.cooldownCategory,
				ticks: cooldown.cooldownTicks,
				remaining: holder ? attempt(() => cooldown.getCooldownTicksRemaining(holder)) : null,
			}
			: null,
		potion: potion
			? {
				effect: attempt(() => potion.potionEffectType.id),
				durationTicks: attempt(() => potion.potionEffectType.durationTicks ?? null),
				delivery: attempt(() => potion.potionDeliveryType.id),
			}
			: null,
	};
}

export function health(entity: Entity) {
	const component = attempt(() => entity.getComponent(EntityComponentTypes.Health));
	return component ? { current: round(component.currentValue), max: component.effectiveMax } : null;
}

function name(target: Entity): string | null {
	return target.nameTag || (target instanceof Player ? target.name : null);
}

/** An entity in a list. */
export function entity(target: Entity, origin?: Vector) {
	const location = target.location;
	return {
		id: target.id,
		type: target.typeId,
		name: name(target),
		location: vector(location),
		distance: origin ? distance(location, origin) : null,
		health: health(target),
		tags: attempt(() => target.getTags()) ?? [],
	};
}

/** An entity in full: identity, transform, health, tags, families and flags. */
export function entityDetail(target: Entity) {
	const rotation = target.getRotation();
	const families = attempt(() =>
		target.getComponent(EntityComponentTypes.TypeFamily)?.getTypeFamilies()
	);
	return {
		id: target.id,
		type: target.typeId,
		name: name(target),
		nameTag: target.nameTag,
		player: target instanceof Player,
		dimension: target.dimension.id,
		location: vector(target.location),
		eye: attempt(() => vector(target.getHeadLocation())),
		velocity: attempt(() => vector(target.getVelocity())),
		yaw: round(rotation.y),
		pitch: round(rotation.x),
		viewDirection: attempt(() => vector(target.getViewDirection())),
		health: health(target),
		tags: attempt(() => target.getTags()) ?? [],
		families: families ?? [],
		flags: {
			onGround: target.isOnGround,
			inWater: target.isInWater,
			swimming: target.isSwimming,
			falling: target.isFalling,
			climbing: target.isClimbing,
			sleeping: target.isSleeping,
			sneaking: target.isSneaking,
			sprinting: target.isSprinting,
		},
	};
}

/** Each component's type id and readable fields. */
export function components(target: Entity) {
	return target.getComponents().map((component) => ({
		type: component.typeId,
		fields: readable(component),
	}));
}

export function block(target: Block | undefined) {
	if (!target) return null;
	return {
		id: target.typeId,
		location: vector(target.location),
		dimension: target.dimension.id,
		states: attempt(() => target.permutation.getAllStates()) ?? {},
		tags: attempt(() => target.getTags()) ?? [],
		air: target.isAir,
		liquid: target.isLiquid,
		waterlogged: target.isWaterlogged,
		redstonePower: attempt(() => target.getRedstonePower()) ?? null,
		light: attempt(() => target.getLightLevel()),
		skyLight: attempt(() => target.getSkyLightLevel()),
		components: (attempt(() => target.getComponents()) ?? []).map((found) => found.typeId),
		inventory: attempt(() => target.getComponent(BlockComponentTypes.Inventory)?.container?.size) ??
			null,
	};
}

export function player(target: Player) {
	const rotation = target.getRotation();
	const hunger = attempt(() => target.getComponent(EntityComponentTypes.Hunger));
	return {
		name: target.name,
		id: target.id,
		dimension: target.dimension.id,
		location: vector(target.location),
		eye: vector(target.getHeadLocation()),
		velocity: vector(target.getVelocity()),
		yaw: round(rotation.y),
		pitch: round(rotation.x),
		health: health(target),
		hunger: hunger ? round(hunger.currentValue) : null,
		gameMode: target.getGameMode(),
		selectedSlot: target.selectedSlotIndex,
		effects: (attempt(() => target.getEffects()) ?? []).map((effect) => ({
			id: effect.typeId,
			amplifier: effect.amplifier,
			ticks: effect.duration,
		})),
		tags: attempt(() => target.getTags()) ?? [],
		flags: {
			sneaking: target.isSneaking,
			sprinting: target.isSprinting,
			onGround: target.isOnGround,
			flying: target.isFlying,
			gliding: target.isGliding,
			swimming: target.isSwimming,
			inWater: target.isInWater,
			sleeping: target.isSleeping,
			falling: target.isFalling,
			climbing: target.isClimbing,
		},
		spawnPoint: attempt(() => {
			const spawn = target.getSpawnPoint();
			return spawn ? { ...vector(spawn), dimension: spawn.dimension.id } : null;
		}),
		level: target.level,
	};
}

export const EQUIPMENT = [
	EquipmentSlot.Head,
	EquipmentSlot.Chest,
	EquipmentSlot.Legs,
	EquipmentSlot.Feet,
	EquipmentSlot.Mainhand,
	EquipmentSlot.Offhand,
];

export function inventory(target: Player) {
	const container = attempt(() => target.getComponent(EntityComponentTypes.Inventory)?.container);
	const slots: unknown[] = [];
	if (container) {
		for (let slot = 0; slot < container.size; slot++) {
			const stack = attempt(() => container.getItem(slot));
			if (stack) slots.push({ slot, ...itemDetail(stack, target) });
		}
	}
	const equippable = attempt(() => target.getComponent(EntityComponentTypes.Equippable));
	const equipment: Record<string, unknown> = {};
	for (const slot of EQUIPMENT) {
		equipment[slot] = equippable
			? itemDetail(attempt(() => equippable.getEquipment(slot)) ?? undefined, target)
			: null;
	}
	return { size: container?.size ?? 0, selectedSlot: target.selectedSlotIndex, slots, equipment };
}

/** One page of a container's occupied slots. */
export function containerPage(
	container: Container,
	offset: number,
	limit: number,
	holder?: Player,
) {
	const end = Math.min(container.size, offset + limit);
	const slots: unknown[] = [];
	for (let slot = offset; slot < end; slot++) {
		const stack = attempt(() => container.getItem(slot));
		if (stack) slots.push({ slot, ...itemDetail(stack, holder) });
	}
	return {
		size: container.size,
		emptySlots: container.emptySlotsCount,
		offset,
		slots,
		next: end < container.size ? end : null,
	};
}

export function dimensionOf(id: string | undefined, fallback: Dimension): Dimension {
	return id ? world.getDimension(id) : fallback;
}

/** Readable game rules: every getter the GameRules object exposes. */
export function gameRules() {
	const rules: Record<string, unknown> = {};
	const source = world.gameRules as unknown as Record<string, unknown>;
	for (const key of Object.getOwnPropertyNames(Object.getPrototypeOf(source))) {
		if (key === "constructor") continue;
		const value = attempt(() => source[key]);
		if (typeof value !== "function") rules[key] = value;
	}
	return rules;
}

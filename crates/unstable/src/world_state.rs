//! World state from the game's own local server: entities and dynamic properties, read on
//! the server thread without the Script API.
//!
//! Single player runs a server inside the game process, on its own thread. `ServerInstance`
//! derives from `GameCallbacks` (`src/mc/server/ServerInstance.h`), whose ten-slot vtable is
//! `[dtor, onLevelCorrupt, onGameModeChanged, onBeforeSimTick, onTick, ...]`
//! (`src/mc/world/GameCallbacks.h`); the count is checked before slot 4, `onTick`, is replaced.
//! The hook runs after each server tick. It reads nothing unless a request waits (or the
//! capability is not confirmed yet), then answers every waiting request there, between ticks,
//! so nothing it reads changes under it.
//!
//! The paths, with layouts from LeviLamina's headers. Only `ServerLevel` is found by its vtable
//! (RTTI, or the anchor rule on its `_subTick`); everything else is recognized by its contents:
//! - `ServerInstance::mMinecraft` is a `Minecraft` (`src/mc/world/Minecraft.h`), whose
//!   `mGameCallbacks` refers back to the instance's `GameCallbacks`; the code checks that
//!   reference on each candidate. Its `mGameSession` points at the `GameSession`
//!   (`src/mc/world/GameSession.h`), whose `mLevel` reaches the `ServerLevel`.
//! - Entities live in an EnTT registry (`src/mc/deps/ecs/gamerefs_entity/EntityContext.h`).
//!   `Level::mActorManager` (`src/mc/world/level/ActorManager.h`) owns `OwnerPtr<EntityContext>`
//!   values, an optional `EntityContext` (`OwnerStorageEntity.h`) that names the registry; the
//!   manager is the level's object whose `mEntities` read as such values, all naming one
//!   registry. Each component type has its own pool, an `entt::basic_storage<T, EntityId>`. A
//!   pool's `descriptor` points at its `entt::type_info`, whose `alias` is the component's name
//!   and whose `identifier` is that name's FNV-1a hash, so a pool is found by name among the
//!   registry's pool table. A pool keeps the entity ids in `packed` and the components in pages
//!   of `payload`. (EnTT v4's `sparse_set.hpp`, `storage.hpp`, `type_info.hpp` and
//!   `hashed_string.hpp`, the EnTT release LeviLamina's `xmake.lua` pins; not LeviLamina
//!   headers.)
//! - `ActorOwnerComponent` (`src/mc/entity/components/ActorOwnerComponent.h`) owns each `Actor`.
//!   `Actor::mEntityContext` (`src/mc/world/actor/Actor.h`) points back at the registry and the
//!   id, which every actor must match. `Actor::mBuiltInComponents` (`BuiltInActorComponents.h`:
//!   state vector, AABB, rotation) sits 64 bytes after `Actor::mLevel`, past `mActorRendererId`,
//!   `mCategories` and `mVersion`; `mLevel` is found by its value, and the state vector it
//!   points at must be the entity's `StateVectorComponent`
//!   (`src/mc/deps/vanilla_components/`). `ActorRotationComponent`
//!   (`src/mc/entity/components/`) holds the rotation. The id scripts see is the
//!   `ActorUniqueIDComponent`.
//! - Type, name tag, health and dimension come from the `ActorDefinitionIdentifierComponent`,
//!   `SynchedActorDataComponent` (a `DataItem2<std::string>` of `DataItemType::String` with id
//!   4, `ActorDataIDs::Name`; `src/mc/world/actor/SynchedActorData.h`, `DataItem.h`,
//!   `DataItem2.h`), `AttributesComponent` (`minecraft:health` in `BaseAttributeMap`,
//!   `src/mc/world/attribute/`) and `DimensionTypeComponent` pools.
//! - `DynamicProperties` (`src/mc/world/persistence/DynamicProperties.h`) maps a collection name
//!   to a map from key to `variant<double, float, bool, string, Vec3>`. The world's sits behind
//!   `ServerLevel::mDynamicPropertiesManager` (`src/mc/server/ServerLevel.h`,
//!   `src/mc/world/persistence/DynamicPropertiesManager.h`), 152 bytes after the two
//!   `ResourcePackManager` references that open `ServerLevel`'s own members; an entity's is its
//!   `DynamicPropertiesComponent` (`src/mc/entity/components/DynamicPropertiesComponent.h`).
//!   The references are two objects of one class, and 40 bytes after them `mMobEvents` points
//!   at the `MobEvents` (`src/mc/world/level/MobEvents.h`, `MobEvent.h`), whose events carry
//!   the names the `/mobevent` command takes.
//! - The player read adds what only a player has. Hunger and saturation are the
//!   `minecraft:player.hunger` and `minecraft:player.saturation` attributes (`Player::HUNGER()`
//!   and `Player::SATURATION()` in `src/mc/world/actor/player/Player.h`; the names are in the
//!   public game's strings). The selected slot is `PlayerInventory::mSelected`
//!   (`src/mc/world/actor/player/PlayerInventory.h`) behind `Player::mInventory`, 992 bytes
//!   after `Actor::mLevel` by the member sizes in LeviLamina's `Actor.h`, `Mob.h` and
//!   `Player.h`. The pointer is tried there, then searched for in the player, and what it
//!   points at must have `PlayerInventory`'s shape: two vtables, a slot of 0 to 8, an
//!   `ItemStack` (which has a vtable), the `Inventory` container id (`ContainerID.h`) and an
//!   owned `Inventory` with a vtable. Effects are the `MobEffectsComponent`
//!   (`src/mc/entity/components/MobEffectsComponent.h`), a vector of 168-byte
//!   `MobEffectInstance`s (`src/mc/world/effect/MobEffectInstance.h`: `mId`, `mDuration`,
//!   `mAmplifier` at 32, and a nested vector of effects at 40). A runtime check found each
//!   effect at the index of its id, with empty (id 0) instances in the slots between.
//!
//! Objects found by their contents must still have a vtable in the game's image where their
//! class has one.
//!
//! Every pointer is checked readable before it is followed; nothing is written, freed, locked
//! or called. A client joined to a remote server runs no local server: the hook never fires and
//! requests say so instead of reading the client's partial copy of the world.

use std::{
    collections::HashMap,
    ffi::c_void,
    ops::Range,
    sync::{
        Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use endpoint::log;
use protocol::unstable::{
    CapabilityState, EntitySnapshot, NativeEffect, NativeEntity, NativePlayer, Property,
    PropertyCollection, PropertySnapshot, PropertyTarget, PropertyValue,
};
use rtti::anchors::{self, Slots};

use crate::{capability, memory::Memory, vtable::Resolver};

const NAME: &str = "world_state";
const INSTANCE_CLASS: &str = "ServerInstance";
const CALLBACK_SLOTS: usize = 10;
const ON_TICK_SLOT: usize = 4;
const LEVEL_CLASS: &str = "ServerLevel";
/// `ILevel`'s 405 virtuals and `Level`'s 5 more, in LeviLamina's headers.
const LEVEL_SLOTS: usize = 410;

/// Bytes scanned for pointers: `ServerInstance`, `Minecraft`, `GameSession`, `ServerLevel`.
const INSTANCE_SPAN: usize = 0x400;
const MINECRAFT_SPAN: usize = 0x400;
const SESSION_SPAN: usize = 0x80;
const LEVEL_SPAN: usize = 0x2000;
/// Where `ServerLevel`'s own members may start (after `Level`'s).
const LEVEL_OWN_FROM: usize = 0x400;
/// From `ServerLevel::mServerResourcePackManager` to `mMobEvents` and to
/// `mDynamicPropertiesManager`.
const LEVEL_MOB_EVENTS: usize = 40;
const LEVEL_PROPERTIES_MANAGER: usize = 152;
/// `DynamicPropertiesManager::mLevelDynamicProperties`.
const MANAGER_PROPERTIES: usize = 32;
/// Bytes of `MobEvents` searched for its events' names (LeviLamina's header puts its
/// `std::array<MobEvent, 3>` after the vtable, each event's `mName` first), and the names.
const MOB_EVENTS_SPAN: usize = 0x100;
const MOB_EVENT_NAMES: [&str; 3] = [
    "minecraft:pillager_patrols_event",
    "minecraft:wandering_trader_event",
    "minecraft:ender_dragon_event",
];

/// `ActorManager::mEntities`, a vector of `OwnerPtr<EntityContext>` (32 bytes: registry,
/// EnTT registry, id, engaged flag).
const MANAGER_ENTITIES: usize = 8;
const OWNER_PTR_SIZE: usize = 32;
const OWNER_PTR_ENGAGED: usize = 24;
/// `entt::basic_registry<EntityId>`, as `EntityRegistry::mRegistry` sizes it.
const REGISTRY_SIZE: usize = 304;
const MAX_POOL_TABLE: usize = 256 * 1024;

/// `basic_sparse_set::packed` and `descriptor`, and `basic_storage::payload` (EnTT v4's
/// `sparse_set.hpp` and `storage.hpp`). `payload` is tried there first, then at every word
/// after `packed` within [`POOL_PAYLOAD_SPAN`] bytes of the pool.
const POOL_PACKED: usize = 32;
const POOL_DESCRIPTOR: usize = 56;
const POOL_PAYLOAD: usize = 80;
const POOL_PAYLOAD_SPAN: usize = 0x100;
/// Bytes of a pool searched for its descriptor when it is not where EnTT puts it.
const POOL_HEAD_SPAN: usize = 0x60;
/// `entt::type_info`: `seq` and `identifier` (32-bit `id_type`s), then `alias`, a
/// `string_view`.
const TYPE_INFO_IDENTIFIER: usize = 4;
const TYPE_INFO_ALIAS: usize = 8;
const TYPE_INFO_SIZE: usize = 24;
/// EnTT's `hashed_string`: 32-bit FNV-1a.
const FNV_OFFSET: u32 = 0x811C_9DC5;
const FNV_PRIME: u32 = 0x0100_0193;

/// The component pools the reads use, by the name EnTT gives each type.
const OWNER: &str = "ActorOwnerComponent";
const UNIQUE_ID: &str = "ActorUniqueIDComponent";
const STATE_VECTOR: &str = "StateVectorComponent";
const IDENTIFIER: &str = "ActorDefinitionIdentifierComponent";
const SYNCHED: &str = "SynchedActorDataComponent";
const ATTRIBUTES: &str = "AttributesComponent";
const DIMENSION: &str = "DimensionTypeComponent";
const PROPERTIES: &str = "DynamicPropertiesComponent";
const MOB_EFFECTS: &str = "MobEffectsComponent";
const COMPONENTS: [&str; 9] = [
    OWNER,
    UNIQUE_ID,
    STATE_VECTOR,
    IDENTIFIER,
    SYNCHED,
    ATTRIBUTES,
    DIMENSION,
    PROPERTIES,
    MOB_EFFECTS,
];

/// Components per `payload` page (`ENTT_PACKED_PAGE`): LeviLamina's `xmake.lua` builds EnTT
/// with 128, EnTT's own default is 1024. The layer picks the one every pool's shape agrees
/// with ([`page_sizes`]) and checks it against the actors when the shapes leave both open.
const PAGE_SIZES: [usize; 2] = [128, 1024];
/// Entries below this sit at the same place under every page size.
const FIRST_PAGE: usize = PAGE_SIZES[0];
/// Calibration checks up to this many actors and needs at least the minimum to decide.
const CALIBRATE_SAMPLE: usize = 16;
const CALIBRATE_MIN: usize = 2;
const MAX_ENTITIES: usize = 1 << 16;
const MAX_PAGES: usize = MAX_ENTITIES / FIRST_PAGE + 1;
/// `EntityId`'s split (`src/mc/deps/ecs/EntityIdTraits.h`): 18 entity bits, then a 14-bit
/// version. EnTT marks an entry deleted in place by setting every version bit.
const ENTITY_BITS: u32 = 18;
const VERSION_MASK: u32 = 0x3FFF;

/// `Actor::mEntityContext`: the EnTT registry and the id.
const ACTOR_REGISTRY: usize = 16;
const ACTOR_ENTITY: usize = 24;
/// Where `Actor::mLevel` may sit, and its neighbours relative to it.
const ACTOR_LEVEL_FROM: usize = 0x100;
const ACTOR_LEVEL_TO: usize = 0x800;
const LEVEL_TO_STATE_VECTOR: usize = 64;
const LEVEL_TO_AABB: usize = 72;
const LEVEL_TO_ROTATION: usize = 80;
/// `AABBShapeComponent::mAABB`: min and max corners
/// (`src/mc/deps/vanilla_components/AABBShapeComponent.h`).
const AABB_SIZE: usize = 24;
/// The farthest the feet may lie below the moved position, in blocks.
const MAX_FEET_DROP: f32 = 16.0;

/// Component sizes and fields.
const OWNER_SIZE: usize = 8;
const UNIQUE_ID_SIZE: usize = 8;
const STATE_VECTOR_SIZE: usize = 36;
const IDENTIFIER_SIZE: usize = 176;
/// `ActorDefinitionIdentifier::mNamespace`, `mIdentifier` and `mCanonicalName`'s string.
const IDENTIFIER_NAMESPACE: usize = 0;
const IDENTIFIER_NAME: usize = 32;
const IDENTIFIER_CANONICAL: usize = 136;
/// `SynchedActorDataComponent`: LeviLamina's `SynchedActorData.h` makes it 72 bytes with
/// `mItemsArray` first, which is tried first. Other layouts are searched for: the item vector
/// among the first words of a component, and the stride as the distance to the next
/// component's vector.
const SYNCHED_SIZE: usize = 72;
const SYNCHED_ITEMS_SPAN: usize = 32;
const MAX_SYNCHED_STRIDE: usize = 256;
/// MSVC `std::vector`: first, last, end.
const VECTOR_SIZE: usize = 24;
/// Items checked per sampled component: the vector is indexed by `DataItem::mId`.
const SYNCHED_CHECK_ITEMS: usize = 8;
const SYNCHED_SAMPLE: usize = 64;
/// `DataItem::mType` and `mId`, and `DataItem2<T>::mValue`.
const ITEM_TYPE: usize = 8;
const ITEM_ID: usize = 10;
const ITEM_VALUE: usize = 16;
const ITEM_TYPE_STRING: u8 = 4;
/// `DataItemType::Unknown`, past the last real type (`src/mc/world/actor/DataItemType.h`).
const ITEM_TYPE_UNKNOWN: u8 = 9;
const ITEM_ID_NAME: u16 = 4;
const ATTRIBUTES_SIZE: usize = 80;
/// `AttributeInstance::mAttribute`, `mCurrentValues` (min, max, value);
/// `Attribute::mName`'s string.
const INSTANCE_ATTRIBUTE: usize = 8;
const INSTANCE_CURRENT: usize = 116;
const ATTRIBUTE_NAME: usize = 16;
const HEALTH: &str = "minecraft:health";
const HUNGER: &str = "minecraft:player.hunger";
const SATURATION: &str = "minecraft:player.saturation";
const DIMENSION_SIZE: usize = 4;
const PROPERTIES_SIZE: usize = 64;
/// `MobEffectsComponent::mMobEffects`, a vector of `MobEffectInstance`: its size, `mDuration`,
/// `mAmplifier` and the nested `mEffects` vector.
const MOB_EFFECTS_SIZE: usize = 24;
const EFFECT_SIZE: usize = 168;
const EFFECT_DURATION: usize = 4;
const EFFECT_AMPLIFIER: usize = 32;
const EFFECT_NESTED: usize = 40;
const MAX_EFFECTS: usize = 64;
/// `Player::mInventory` after `Actor::mLevel` in LeviLamina's headers, and the bytes of the
/// player object searched for it otherwise (LeviLamina's `Player` is 0xCE0 bytes).
const LEVEL_TO_INVENTORY: usize = 0x3E0;
const PLAYER_SPAN: usize = 0x1000;
/// `PlayerInventory`: the two listener vtables, `mSelected`, `mInfiniteItem` (an `ItemStack`),
/// `mSelectedContainerId` (`ContainerID::Inventory` is 0) and `mInventory`.
const INVENTORY_SIZE: usize = 232;
const INVENTORY_SELECTED: usize = 16;
const INVENTORY_ITEM: usize = 24;
const INVENTORY_CONTAINER: usize = 176;
const INVENTORY_OWNED: usize = 184;
const HOTBAR_SLOTS: u8 = 9;

/// MSVC `std::unordered_map`: the list head and size, the bucket vector, then the mask and
/// the bucket count.
const MAP_HEAD: usize = 8;
const MAP_VECTOR: usize = 24;
const MAP_MASK: usize = 48;
/// A list node's value, after the next and previous links.
const NODE_VALUE: usize = 16;
/// Collection node: name, then `PropertyCollection` (byte count, map).
const COLLECTION_BYTES: usize = 48;
const COLLECTION_MAP: usize = 56;
/// Property node: key, then the variant (40 bytes, index at 32).
const PROPERTY_VALUE: usize = 48;
const PROPERTY_INDEX: usize = 80;

/// Bounds of one read.
const MAX_ACTORS: usize = 8192;
const MAX_LIMIT: usize = 4096;
const DEFAULT_LIMIT: usize = 256;
const MAX_ITEMS: usize = 512;
const MAX_ATTRIBUTES: usize = 128;
const MAX_NAME: usize = 256;
const MAX_COLLECTIONS: usize = 256;
const MAX_PROPERTIES: usize = 4096;
const MAX_VALUE: usize = 32 * 1024;
const MAX_TEXT_TOTAL: usize = 4 * 1024 * 1024;
/// Larger than any world coordinate.
const MAX_COORDINATE: f32 = 1.0e8;

/// How long a request waits for a server tick, and how many `onTick` calls pass between
/// checks of an unconfirmed capability (20; LeviLamina names `onTick`, and the documented tick
/// rate is 20 ticks per second).
const WAIT: Duration = Duration::from_secs(2);
const CHECK_EVERY: u64 = 20;
/// How many `onTick` calls a lone actor must read as the same player before it confirms the
/// layout: 60.
const SETTLE_TICKS: u64 = 60;
/// Layout checks that may fail in a row on one world before the capability reports failed,
/// since a loading world can hold actors that do not pass the checks yet. Checks go on after
/// that, so a world that settles still turns the capability active.
const FAIL_AFTER: u32 = 30;

const NOT_TICKING: &str = "the game's local server did not tick within 2 s: no world is open, \
     or the game is joined to a remote server, which runs no local server (entities and \
     dynamic properties live on that server, where only its own scripts can read them)";

type OnTick = unsafe extern "system" fn(*mut c_void, i32, i32);

/// What the reads recognize, located at install: the `ServerLevel` vtable, and the game's
/// image, where the vtables of the classes found by their contents must lie.
struct Classes {
    server_level: usize,
    image: Range<usize>,
}

static CLASSES: OnceLock<Classes> = OnceLock::new();
static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
/// Offset of the `GameCallbacks` subobject inside `ServerInstance`.
static SUBOBJECT: AtomicUsize = AtomicUsize::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);
static CONFIRMED: AtomicBool = AtomicBool::new(false);
/// Calibrated offsets and the component page size; zero until found.
static PAYLOAD: AtomicUsize = AtomicUsize::new(0);
static PAGE: AtomicUsize = AtomicUsize::new(0);
static ACTOR_LEVEL: AtomicUsize = AtomicUsize::new(0);
static DESCRIPTOR: AtomicUsize = AtomicUsize::new(0);
/// The offsets of `ServerInstance::mMinecraft` and `Minecraft::mGameSession`, plus one; zero
/// until found.
static MINECRAFT_AT: AtomicUsize = AtomicUsize::new(0);
static SESSION_AT: AtomicUsize = AtomicUsize::new(0);
/// The `SynchedActorDataComponent` layout last found, as `stride << 8 | items`.
static SYNCHED_LAYOUT: AtomicUsize = AtomicUsize::new(0);
/// The offset of `Player::mInventory`; zero until found.
static INVENTORY_AT: AtomicUsize = AtomicUsize::new(0);
static LOGGED_WORLD_PATH: AtomicBool = AtomicBool::new(false);
/// The level the last check saw, and how many layout checks in a row failed on it.
static CHECKED_LEVEL: AtomicUsize = AtomicUsize::new(0);
static LAYOUT_FAILS: AtomicU32 = AtomicU32::new(0);
/// The lone player an unconfirmed check saw, and the tick it first read as that player.
static LONE: Mutex<Option<(i64, u64)>> = Mutex::new(None);

enum Job {
    Entities(Query),
    Player(Option<String>),
    Properties {
        target: PropertyTarget,
        collection: Option<String>,
    },
}

enum Answer {
    Entities(Result<EntitySnapshot, String>),
    Player(Result<NativePlayer, String>),
    Properties(Result<PropertySnapshot, String>),
}

/// What an entity request asks for.
#[derive(Clone)]
pub struct Query {
    pub id: Option<i64>,
    pub player: Option<String>,
    pub radius: Option<f64>,
    pub entity_type: Option<String>,
    pub limit: Option<u32>,
}

static NEXT_JOB: AtomicU64 = AtomicU64::new(0);
static QUEUED: AtomicUsize = AtomicUsize::new(0);
static JOBS: Mutex<Vec<(u64, Job)>> = Mutex::new(Vec::new());
static ANSWERS: Mutex<Vec<(u64, Answer)>> = Mutex::new(Vec::new());
static ANSWERED: Condvar = Condvar::new();

/// The offset of `ServerInstance`'s `GameCallbacks` subobject: its only subobject with that
/// slot count, by RTTI or among the anchor rules.
fn callbacks_offset(resolver: &Resolver<'_>) -> Result<u32, String> {
    let offsets: Vec<u32> = match resolver
        .classes
        .get(&rtti::decorated(INSTANCE_CLASS, false))
    {
        Some(tables) => tables
            .iter()
            .filter(|table| table.offset != 0 && table.slots.len() == CALLBACK_SLOTS)
            .map(|table| table.offset)
            .collect(),
        None => anchors::RULES
            .iter()
            .filter(|rule| {
                rule.class == INSTANCE_CLASS
                    && rule.offset != 0
                    && rule.slots == Slots::Exact(CALLBACK_SLOTS)
            })
            .map(|rule| rule.offset)
            .collect(),
    };
    match offsets[..] {
        [offset] => Ok(offset),
        _ => Err(format!(
            "{INSTANCE_CLASS} has no single {CALLBACK_SLOTS}-slot GameCallbacks vtable"
        )),
    }
}

/// The addresses the game's image spans.
fn image_range(resolver: &Resolver<'_>) -> Range<usize> {
    let base = resolver.image.base as usize;
    let size = resolver
        .image
        .sections
        .iter()
        .map(|section| section.rva as usize + section.bytes.len())
        .max()
        .unwrap_or(0);
    base..base + size
}

pub fn install(resolver: &Resolver<'_>) {
    let result = callbacks_offset(resolver).and_then(|offset| {
        let table = resolver.table(INSTANCE_CLASS, offset, CALLBACK_SLOTS)?;
        let level = resolver.table_at_least(LEVEL_CLASS, 0, LEVEL_SLOTS)?;
        capability::found(
            NAME,
            &[(INSTANCE_CLASS, table.found), (LEVEL_CLASS, level.found)],
        );
        let classes = Classes {
            server_level: level.address,
            image: image_range(resolver),
        };
        CLASSES
            .set(classes)
            .map_err(|_| "installed twice".to_owned())?;
        SUBOBJECT.store(offset as usize, Ordering::Release);
        table.hook(ON_TICK_SLOT, on_tick as OnTick as usize, &ORIGINAL)
    });
    match result {
        Ok(()) => capability::set(
            NAME,
            CapabilityState::Pending,
            "hooked ServerInstance::onTick; waiting for a local world to tick",
        ),
        Err(reason) => capability::set(NAME, CapabilityState::Unavailable, reason),
    }
}

unsafe extern "system" fn on_tick(this: *mut c_void, tick: i32, max_tick: i32) {
    // SAFETY: stored from the slot this hook replaced.
    let original: OnTick = unsafe { std::mem::transmute(ORIGINAL.load(Ordering::Acquire)) };
    unsafe { original(this, tick, max_tick) };
    let count = TICKS.fetch_add(1, Ordering::AcqRel) + 1;
    let waiting = QUEUED.load(Ordering::Acquire) != 0;
    let check = !CONFIRMED.load(Ordering::Acquire) && count % CHECK_EVERY == 1;
    if !waiting && !check {
        return;
    }
    let Some(classes) = CLASSES.get() else {
        return;
    };
    let instance = (this as usize).wrapping_sub(SUBOBJECT.load(Ordering::Acquire));
    serve(classes, instance, count);
}

/// Everything one tick read: the level, its registry and the pools found in it.
struct World {
    level: usize,
    registry: usize,
    pools: HashMap<&'static str, Pool>,
}

/// One component pool: `packed` holds `count` entity ids in room for `capacity`, `payload`
/// holds `pages` pointers to pages of `page` components each.
#[derive(Clone, Copy)]
struct Pool {
    packed: usize,
    count: usize,
    capacity: usize,
    payload: usize,
    pages: usize,
    page: usize,
}

/// Whether a packed entry is a live entity rather than a deleted one.
fn live(entity: u32) -> bool {
    entity >> ENTITY_BITS != VERSION_MASK
}

impl Pool {
    fn entity(&self, memory: &mut Memory, index: usize) -> Option<u32> {
        memory.read::<u32>(self.packed + index * 4)
    }

    fn component(&self, memory: &mut Memory, index: usize, size: usize) -> Option<usize> {
        if index / self.page >= self.pages {
            return None;
        }
        let page = memory.read::<usize>(self.payload + (index / self.page) * 8)?;
        (page != 0).then(|| page + (index % self.page) * size)
    }

    /// Whether pages of `page` components agree with this pool's shape. EnTT's
    /// `basic_storage` allocates the page of each packed position it uses (or reserves) and
    /// frees pages only when it shrinks `packed` too, so the pages hold every entry and each
    /// but the last starts below `packed`'s capacity.
    fn fits(&self, page: usize) -> bool {
        self.pages * page >= self.count
            && (self.pages == 0 || (self.pages - 1) * page < self.capacity)
    }

    fn with_page(self, page: usize) -> Pool {
        Pool { page, ..self }
    }

    /// Component address by entity id.
    fn index(&self, memory: &mut Memory, size: usize) -> HashMap<u32, usize> {
        let mut index = HashMap::with_capacity(self.count);
        for at in 0..self.count {
            if let (Some(entity), Some(component)) =
                (self.entity(memory, at), self.component(memory, at, size))
                && live(entity)
            {
                index.insert(entity, component);
            }
        }
        index
    }

    /// The component of one entity, or None when the pool does not hold it.
    fn find(&self, memory: &mut Memory, entity: u32, size: usize) -> Option<usize> {
        let at = (0..self.count).find(|&at| self.entity(memory, at) == Some(entity))?;
        self.component(memory, at, size)
    }
}

fn serve(classes: &Classes, instance: usize, tick: u64) {
    let jobs = match JOBS.lock() {
        Ok(mut jobs) => {
            QUEUED.store(0, Ordering::Release);
            std::mem::take(&mut *jobs)
        }
        Err(_) => return,
    };
    let start = Instant::now();
    let mut memory = Memory::default();
    let found = server_level(&mut memory, classes, instance);
    let world = found
        .clone()
        .and_then(|(session, level)| locate(&mut memory, classes, session, level));
    let actors = world
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|world| actors(&mut memory, classes, world));
    if !CONFIRMED.load(Ordering::Acquire) {
        confirm(found.ok().map(|(_, level)| level), &actors, tick);
    }
    let mut answers = Vec::with_capacity(jobs.len());
    for (id, job) in jobs {
        let answer = match job {
            Job::Entities(query) => Answer::Entities(
                actors
                    .as_ref()
                    .map_err(Clone::clone)
                    .map(|(entities, skipped)| snapshot(entities, *skipped, &query, tick, start)),
            ),
            Job::Player(name) => Answer::Player(
                world
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|world| {
                        let (actors, _) = actors.as_ref().map_err(Clone::clone)?;
                        read_player(&mut memory, classes, world, actors, name.as_deref())
                    })
                    .map(|mut player| {
                        player.tick = tick;
                        player.read_us = start.elapsed().as_micros() as u64;
                        player
                    }),
            ),
            Job::Properties { target, collection } => Answer::Properties(
                world
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|world| {
                        properties(
                            &mut memory,
                            classes,
                            world,
                            &actors,
                            &target,
                            collection.as_deref(),
                        )
                    })
                    .map(|mut snapshot| {
                        snapshot.tick = tick;
                        snapshot.read_us = start.elapsed().as_micros() as u64;
                        snapshot
                    }),
            ),
        };
        answers.push((id, answer));
    }
    if answers.is_empty() {
        return;
    }
    if let Ok(mut done) = ANSWERS.lock() {
        done.extend(answers);
        // Answers nobody waits for any more (their request timed out) go first.
        let excess = done.len().saturating_sub(64);
        done.drain(..excess);
    }
    ANSWERED.notify_all();
}

/// Report what an unconfirmed check found. A layout that fails on a world still loading is
/// pending; only a run of failures on one world reports failed, and checks go on after it.
/// A lone actor confirms the layout only once it has read as the same player for
/// [`SETTLE_TICKS`] `onTick` calls, since an actor that does not pass the checks yet can read
/// as no player, or not yet as its final self.
fn confirm(level: Option<usize>, actors: &Result<(Vec<Actor>, u32), String>, tick: u64) {
    let level = level.unwrap_or(0);
    if CHECKED_LEVEL.swap(level, Ordering::AcqRel) != level {
        LAYOUT_FAILS.store(0, Ordering::Release);
    }
    let lone_settled = match actors {
        Ok((actors, _)) if actors.len() == 1 => LONE
            .lock()
            .is_ok_and(|mut lone| settled(&mut lone, &actors[0].native, tick)),
        _ => false,
    };
    let (state, detail) = match actors {
        Ok((actors, _)) if actors.len() == 1 && !lone_settled => {
            LAYOUT_FAILS.store(0, Ordering::Release);
            (
                CapabilityState::Pending,
                "the local server holds one actor; it confirms the layout once it reads as the \
                 same player for 60 onTick calls"
                    .to_owned(),
            )
        }
        Ok((actors, skipped)) if !actors.is_empty() => {
            CONFIRMED.store(true, Ordering::Release);
            (
                CapabilityState::Active,
                format!(
                    "read {} entities on the server thread ({skipped} failed their checks)",
                    actors.len()
                ),
            )
        }
        Ok(_) => {
            LAYOUT_FAILS.store(0, Ordering::Release);
            (
                CapabilityState::Pending,
                "the local server holds no actors yet".to_owned(),
            )
        }
        Err(reason) if reason.starts_with("layout") => {
            let fails = LAYOUT_FAILS.fetch_add(1, Ordering::AcqRel) + 1;
            if fails < FAIL_AFTER {
                (
                    CapabilityState::Pending,
                    "a local world is loading; the entity layout is checked again until it \
                     settles"
                        .to_owned(),
                )
            } else {
                (
                    CapabilityState::Failed,
                    format!("{reason}; checked again every 20 onTick calls"),
                )
            }
        }
        Err(reason) => {
            LAYOUT_FAILS.store(0, Ordering::Release);
            (CapabilityState::Pending, format!("waiting: {reason}"))
        }
    };
    capability::set(NAME, state, detail);
}

/// Whether `actor`, the only one on the local server, has read as the same player for
/// [`SETTLE_TICKS`]; `lone` keeps the player and when it was first seen.
fn settled(lone: &mut Option<(i64, u64)>, actor: &NativeEntity, tick: u64) -> bool {
    if !actor.player {
        *lone = None;
        return false;
    }
    match *lone {
        Some((id, since)) if id == actor.id => tick.saturating_sub(since) >= SETTLE_TICKS,
        _ => {
            *lone = Some((actor.id, tick));
            false
        }
    }
}

/// The first word in `span` bytes from `base` that satisfies `test`, with its offset.
fn find_word(
    memory: &mut Memory,
    base: usize,
    span: usize,
    mut test: impl FnMut(&mut Memory, usize) -> bool,
) -> Option<(usize, usize)> {
    if !memory.object(base, span) {
        return None;
    }
    (0..span).step_by(8).find_map(|offset| {
        let word = memory.read::<usize>(base + offset)?;
        (word >= 0x10000 && word.is_multiple_of(8) && test(memory, word)).then_some((word, offset))
    })
}

fn has_vtable(memory: &mut Memory, object: usize, vtable: usize) -> bool {
    memory.object(object, 8) && memory.read::<usize>(object) == Some(vtable)
}

/// Whether `object` starts with a vtable in the game's image: a pointer into the image whose
/// first slot points into it too.
fn image_vtable(memory: &mut Memory, classes: &Classes, object: usize) -> bool {
    memory.object(object, 8)
        && memory.read::<usize>(object).is_some_and(|vtable| {
            classes.image.contains(&vtable)
                && memory
                    .read::<usize>(vtable)
                    .is_some_and(|slot| classes.image.contains(&slot))
        })
}

/// The `GameSession` and its `ServerLevel`, reached from `ServerInstance` through `Minecraft`:
/// the code takes as the `Minecraft` an object with a vtable whose `Minecraft::mGameCallbacks`
/// candidate refers to the instance's `GameCallbacks`, and its session is the first object it
/// points at that holds the level. The offsets of the two pointers are kept once found.
fn server_level(
    memory: &mut Memory,
    classes: &Classes,
    instance: usize,
) -> Result<(usize, usize), String> {
    let callbacks = instance + SUBOBJECT.load(Ordering::Acquire);
    let through = |memory: &mut Memory, minecraft: usize, session_at: usize| {
        let session = memory.read::<usize>(minecraft + session_at)?;
        if session < 0x10000 || !session.is_multiple_of(8) {
            return None;
        }
        find_word(memory, session, SESSION_SPAN, |memory, word| {
            has_vtable(memory, word, classes.server_level)
        })
        .map(|(level, _)| (session, level))
    };
    let minecraft_at = MINECRAFT_AT.load(Ordering::Acquire);
    let session_at = SESSION_AT.load(Ordering::Acquire);
    if minecraft_at != 0
        && session_at != 0
        && let Some(minecraft) = memory.read::<usize>(instance + minecraft_at - 1)
        && memory.object(minecraft, MINECRAFT_SPAN)
        && let Some(found) = through(memory, minecraft, session_at - 1)
    {
        return Ok(found);
    }
    // `Minecraft::mGameSession` points at a session that points at the level; both are
    // missing while no world is loaded.
    if memory.object(instance, INSTANCE_SPAN) {
        for minecraft_at in (0..INSTANCE_SPAN).step_by(8) {
            let Some(minecraft) = memory.read::<usize>(instance + minecraft_at) else {
                break;
            };
            if !image_vtable(memory, classes, minecraft)
                || find_word(memory, minecraft, MINECRAFT_SPAN, |_, word| {
                    word == callbacks
                })
                .is_none()
            {
                continue;
            }
            for session_at in (0..MINECRAFT_SPAN).step_by(8) {
                if let Some(found) = through(memory, minecraft, session_at) {
                    MINECRAFT_AT.store(minecraft_at + 1, Ordering::Release);
                    SESSION_AT.store(session_at + 1, Ordering::Release);
                    return Ok(found);
                }
            }
        }
    }
    Err("no world is loaded on the game's local server".into())
}

/// The registries the level's actor manager names: each object of the level with a vtable
/// whose `mEntities` read as engaged entity contexts that all name one registry.
fn managed_registries(memory: &mut Memory, classes: &Classes, level: usize) -> Vec<usize> {
    let mut registries = Vec::new();
    if !memory.object(level, LEVEL_SPAN) {
        return registries;
    }
    for offset in (0..LEVEL_SPAN).step_by(8) {
        let Some(manager) = memory.read::<usize>(level + offset) else {
            break;
        };
        if !image_vtable(memory, classes, manager) {
            continue;
        }
        let Some((first, count)) =
            memory.vector(manager + MANAGER_ENTITIES, OWNER_PTR_SIZE, MAX_ENTITIES)
        else {
            continue;
        };
        let mut named = None;
        let consistent = (0..count.min(8))
            .map(|index| first + index * OWNER_PTR_SIZE)
            .all(|at| match memory.read::<u8>(at + OWNER_PTR_ENGAGED) {
                Some(0) => true,
                Some(1) => memory.read::<usize>(at + 8).is_some_and(|registry| {
                    registry >= 0x10000
                        && registry.is_multiple_of(8)
                        && *named.get_or_insert(registry) == registry
                }),
                _ => false,
            });
        if consistent
            && let Some(registry) = named
            && !registries.contains(&registry)
        {
            registries.push(registry);
        }
    }
    registries
}

fn locate(
    memory: &mut Memory,
    classes: &Classes,
    session: usize,
    level: usize,
) -> Result<World, String> {
    // The registry: named by the actor manager's entity contexts and by the session's level
    // entity (`OwnerPtr<EntityContext>`: the `EntityRegistry`, then its EnTT registry). The
    // `EntityRegistry` embeds the EnTT registry, so its pool table is in reach from both; the
    // one the actors' own entity contexts name wins.
    let mut candidates = managed_registries(memory, classes, level);
    if memory.object(session, SESSION_SPAN) {
        for offset in (0..SESSION_SPAN).step_by(8) {
            if let Some(word) = memory.read::<usize>(session + offset) {
                candidates.push(word);
            }
        }
    }
    let mut seen = Vec::new();
    let mut failure: Option<String> = None;
    for registry in candidates {
        if registry < 0x10000 || !registry.is_multiple_of(8) || seen.contains(&registry) {
            continue;
        }
        seen.push(registry);
        let found = scan_pools(memory, classes, registry);
        let Some(&owner) = found.get(OWNER) else {
            continue;
        };
        match calibrate_payload(memory, registry, owner) {
            Ok(payload) => return world(memory, level, registry, found, payload),
            // A registry with too few actors to check keeps the capability pending, over one
            // whose actors fail.
            Err(reason) => {
                if failure
                    .as_ref()
                    .is_none_or(|kept| kept.starts_with("layout") && !reason.starts_with("layout"))
                {
                    failure = Some(reason);
                }
            }
        }
    }
    Err(failure
        .unwrap_or_else(|| "layout: no EnTT registry with an ActorOwnerComponent pool".into()))
}

/// The world read through `registry`, whose pools `found` holds by component.
fn world(
    memory: &mut Memory,
    level: usize,
    registry: usize,
    found: HashMap<&'static str, usize>,
    payload: usize,
) -> Result<World, String> {
    let pools: HashMap<&'static str, Pool> = found
        .into_iter()
        .filter_map(|(name, address)| pool(memory, address, payload).map(|pool| (name, pool)))
        .collect();
    let owner = pools
        .get(OWNER)
        .ok_or("layout: the ActorOwnerComponent pool is not readable")?;
    let page = calibrate_page(memory, registry, owner, pools.values())?;
    let pools = pools
        .into_iter()
        .map(|(name, pool)| (name, pool.with_page(page)))
        .collect();
    Ok(World {
        level,
        registry,
        pools,
    })
}

/// Every pool of [`COMPONENTS`] in the registry, by component: the registry's pool table is a
/// vector of nodes holding `shared_ptr`s to the pools.
fn scan_pools(
    memory: &mut Memory,
    classes: &Classes,
    registry: usize,
) -> HashMap<&'static str, usize> {
    let mut found = HashMap::new();
    if !memory.object(registry, REGISTRY_SIZE) {
        return found;
    }
    for offset in (0..REGISTRY_SIZE - 16).step_by(8) {
        let Some([first, last, end]) = memory.read::<[usize; 3]>(registry + offset) else {
            continue;
        };
        if first < 0x10000
            || !first.is_multiple_of(8)
            || first > last
            || last > end
            || last - first > MAX_POOL_TABLE
            || !memory.readable(first, last - first)
        {
            continue;
        }
        for at in (first..last).step_by(8) {
            let Some(pool) = memory.read::<usize>(at) else {
                break;
            };
            if pool < 0x10000 || !pool.is_multiple_of(8) || !image_vtable(memory, classes, pool) {
                continue;
            }
            if let Some(name) = pool_component(memory, pool) {
                found.entry(name).or_insert(pool);
            }
        }
        if found.len() == COMPONENTS.len() {
            break;
        }
    }
    found
}

/// Which of [`COMPONENTS`] the pool at `pool` holds, by its descriptor. The descriptor's
/// offset is learned from the first pool whose descriptor checks out.
fn pool_component(memory: &mut Memory, pool: usize) -> Option<&'static str> {
    let known = DESCRIPTOR.load(Ordering::Acquire);
    if known != 0 {
        return type_name(memory, pool + known);
    }
    let (name, offset) = std::iter::once(POOL_DESCRIPTOR)
        .chain((8..POOL_HEAD_SPAN).step_by(8))
        .find_map(|offset| type_name(memory, pool + offset).map(|name| (name, offset)))?;
    log!("world_state: pool descriptor at {offset:#x} (EnTT's is {POOL_DESCRIPTOR:#x})");
    DESCRIPTOR.store(offset, Ordering::Release);
    Some(name)
}

/// The component of [`COMPONENTS`] that the `entt::type_info` pointer at `at` names: its
/// `alias` must be that name and its `identifier` the name's hash.
fn type_name(memory: &mut Memory, at: usize) -> Option<&'static str> {
    let info = memory.read::<usize>(at)?;
    if !memory.object(info, TYPE_INFO_SIZE) {
        return None;
    }
    let identifier = memory.read::<u32>(info + TYPE_INFO_IDENTIFIER)?;
    let [alias, length] = memory.read::<[usize; 2]>(info + TYPE_INFO_ALIAS)?;
    if !COMPONENTS.iter().any(|name| name.len() == length) || !memory.readable(alias, length) {
        return None;
    }
    // SAFETY: checked readable above.
    let bytes = unsafe { std::slice::from_raw_parts(alias as *const u8, length) };
    let name = COMPONENTS
        .into_iter()
        .find(|name| name.as_bytes() == bytes)?;
    (hashed(bytes) == identifier).then_some(name)
}

/// EnTT's `hashed_string` value of `bytes`.
fn hashed(bytes: &[u8]) -> u32 {
    bytes.iter().fold(FNV_OFFSET, |hash, &byte| {
        (hash ^ u32::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

/// The pool at `address`, with `payload` at that offset, in pages of [`FIRST_PAGE`]
/// components until [`calibrate_page`] decides; None unless some page size fits it.
fn pool(memory: &mut Memory, address: usize, payload: usize) -> Option<Pool> {
    let (packed, count) = memory.vector(address + POOL_PACKED, 4, MAX_ENTITIES)?;
    let end = memory.read::<usize>(address + POOL_PACKED + 16)?;
    let (pages_first, pages) = memory.vector(address + payload, 8, MAX_PAGES)?;
    let pool = Pool {
        packed,
        count,
        capacity: end.saturating_sub(packed) / 4,
        payload: pages_first,
        pages,
        page: FIRST_PAGE,
    };
    PAGE_SIZES
        .into_iter()
        .any(|page| pool.fits(page))
        .then_some(pool)
}

/// The page sizes every pool's shape agrees with.
fn page_sizes<'a>(pools: impl IntoIterator<Item = &'a Pool> + Clone) -> Vec<usize> {
    PAGE_SIZES
        .into_iter()
        .filter(|&page| pools.clone().into_iter().all(|pool| pool.fits(page)))
        .collect()
}

/// The actor (if any) the owner pool's entry `index` holds, checked against its own
/// `mEntityContext`: `Some(None)` for an empty entry, `None` for a failed check.
fn owned_actor(
    memory: &mut Memory,
    registry: usize,
    owner: &Pool,
    index: usize,
) -> Option<Option<(usize, u32)>> {
    let entity = owner.entity(memory, index)?;
    if !live(entity) {
        return Some(None);
    }
    let slot = owner.component(memory, index, OWNER_SIZE)?;
    let actor = memory.read::<usize>(slot)?;
    if actor == 0 {
        return Some(None);
    }
    (memory.object(actor, ACTOR_LEVEL_TO)
        && memory.read::<usize>(actor + ACTOR_REGISTRY) == Some(registry)
        && memory.read::<u32>(actor + ACTOR_ENTITY) == Some(entity))
    .then_some(Some((actor, entity)))
}

/// How many owner pool entries in `indices` hold an actor that matches its own entity
/// context, and how many fail; empty and deleted entries say nothing either way, so they are
/// skipped. Stops once [`CALIBRATE_SAMPLE`] actors are checked.
fn sample_actors(
    memory: &mut Memory,
    registry: usize,
    owner: &Pool,
    indices: Range<usize>,
) -> (usize, usize) {
    let mut matched = 0;
    let mut failed = 0;
    for index in indices {
        match owned_actor(memory, registry, owner, index) {
            Some(Some(_)) => matched += 1,
            Some(None) => {}
            None => failed += 1,
        }
        if matched + failed >= CALIBRATE_SAMPLE {
            break;
        }
    }
    (matched, failed)
}

/// The offset of `basic_storage::payload`: the one under which the sampled actors match their
/// own entity contexts. The sample stays in the first page, where every page size puts an
/// entry at the same place; a world that removed entities while it loaded keeps empty and
/// deleted entries at the front, which the sample skips.
/// A single actor that fails proves nothing: a loading world can hold one actor that does not
/// match its entity context yet, so fewer than [`CALIBRATE_MIN`] actors keep it pending.
fn calibrate_payload(memory: &mut Memory, registry: usize, owner: usize) -> Result<usize, String> {
    let known = PAYLOAD.load(Ordering::Acquire);
    let candidates: Vec<usize> = if known != 0 {
        vec![known]
    } else {
        std::iter::once(POOL_PAYLOAD)
            .chain(
                (POOL_PACKED + VECTOR_SIZE..=POOL_PAYLOAD_SPAN - VECTOR_SIZE)
                    .step_by(8)
                    .filter(|&offset| offset != POOL_PAYLOAD),
            )
            .collect()
    };
    let mut tried = Vec::new();
    // The most actors any offset found: under a wrong offset, entries without an actor may
    // read as empty.
    let mut most = 0;
    for payload in candidates {
        let Some(pool) = pool(memory, owner, payload) else {
            continue;
        };
        let (matched, failed) =
            sample_actors(memory, registry, &pool, 0..pool.count.min(FIRST_PAGE));
        // A wrong offset matches nothing; the right one may meet entries of actors being
        // removed.
        if matched > failed {
            if known == 0 {
                log!(
                    "world_state: pool payload at {payload:#x} (EnTT's is {POOL_PAYLOAD:#x}), {} actors",
                    pool.count
                );
                PAYLOAD.store(payload, Ordering::Release);
            }
            return Ok(payload);
        }
        most = most.max(matched + failed);
        tried.push(format!("{payload:#x}: {matched} of {}", matched + failed));
    }
    if most < CALIBRATE_MIN {
        return Err(format!(
            "the local server holds {most} checkable actors, too few to check the layout yet"
        ));
    }
    Err(format!(
        "layout: no ActorOwnerComponent pool layout matches the actors' own entity contexts \
         (payload offset: actors matched; {})",
        tried.join(", ")
    ))
}

/// The number of components per pool page. The pools' shapes decide it once any pool has
/// outgrown a small page; when they leave more than one size open, the owner pool's actors
/// past the first page decide. Until either does, LeviLamina's size serves without being
/// remembered: entries in the first page sit at the same place under every size.
fn calibrate_page<'a>(
    memory: &mut Memory,
    registry: usize,
    owner: &Pool,
    pools: impl IntoIterator<Item = &'a Pool> + Clone,
) -> Result<usize, String> {
    let fitting = page_sizes(pools);
    let known = PAGE.load(Ordering::Acquire);
    if known != 0 {
        return if fitting.contains(&known) {
            Ok(known)
        } else {
            Err(format!(
                "layout: the component pools no longer fit pages of {known} components"
            ))
        };
    }
    let page = match fitting[..] {
        [] => {
            return Err(format!(
                "layout: no component page size ({PAGE_SIZES:?}) fits every pool's pages and \
                 capacity"
            ));
        }
        [page] => page,
        _ => {
            let sampled = fitting.iter().copied().find(|&page| {
                let (matched, failed) = sample_actors(
                    memory,
                    registry,
                    &owner.with_page(page),
                    FIRST_PAGE..owner.count,
                );
                matched >= CALIBRATE_MIN && matched > failed
            });
            match sampled {
                Some(page) => page,
                None => return Ok(FIRST_PAGE),
            }
        }
    };
    log!("world_state: {page} components per pool page");
    PAGE.store(page, Ordering::Release);
    Ok(page)
}

/// One entity read on the server thread, and its `Actor`.
struct Actor {
    entity: u32,
    address: usize,
    native: NativeEntity,
}

/// Every actor the owner pool holds, read; and how many failed their checks.
fn actors(
    memory: &mut Memory,
    classes: &Classes,
    world: &World,
) -> Result<(Vec<Actor>, u32), String> {
    let owner = world.pools[OWNER];
    let index_of = |name: &str, size: usize, memory: &mut Memory| {
        world
            .pools
            .get(name)
            .map(|pool| pool.index(memory, size))
            .unwrap_or_default()
    };
    let unique_ids = index_of(UNIQUE_ID, UNIQUE_ID_SIZE, memory);
    let state_vectors = index_of(STATE_VECTOR, STATE_VECTOR_SIZE, memory);
    let identifiers = index_of(IDENTIFIER, IDENTIFIER_SIZE, memory);
    // Item vectors by entity id.
    let synched = world
        .pools
        .get(SYNCHED)
        .and_then(|pool| {
            let (stride, items) = synched_layout(memory, pool)?;
            let mut index = pool.index(memory, stride);
            index.values_mut().for_each(|component| *component += items);
            Some(index)
        })
        .unwrap_or_default();
    let attributes = index_of(ATTRIBUTES, ATTRIBUTES_SIZE, memory);
    let dimensions = index_of(DIMENSION, DIMENSION_SIZE, memory);
    let mut actors = Vec::new();
    let mut skipped = 0;
    let mut mismatched_states = 0;
    for index in 0..owner.count.min(MAX_ACTORS) {
        let (actor, entity) = match owned_actor(memory, world.registry, &owner, index) {
            Some(Some(found)) => found,
            Some(None) => continue,
            None => {
                skipped += 1;
                continue;
            }
        };
        let Some(level_field) = level_field(memory, actor, world.level) else {
            skipped += 1;
            continue;
        };
        // The id scripts see is the entity's `ActorUniqueIDComponent`.
        let Some(id) = unique_ids
            .get(&entity)
            .and_then(|&component| memory.read::<i64>(component))
        else {
            skipped += 1;
            continue;
        };
        // The state vector the actor points at must be the entity's pooled component.
        let state = memory.read::<usize>(actor + level_field + LEVEL_TO_STATE_VECTOR);
        if let Some(&pooled) = state_vectors.get(&entity)
            && state != Some(pooled)
        {
            mismatched_states += 1;
            skipped += 1;
            continue;
        }
        let Some((position, rotation)) = transform(memory, actor + level_field) else {
            skipped += 1;
            continue;
        };
        let type_id = identifiers
            .get(&entity)
            .and_then(|&component| identifier(memory, component));
        let player = type_id.as_deref() == Some("minecraft:player");
        actors.push(Actor {
            entity,
            address: actor,
            native: NativeEntity {
                id,
                type_id,
                name_tag: synched
                    .get(&entity)
                    .filter(|&&component| item_vector(memory, component))
                    .and_then(|&component| name_tag(memory, classes, component)),
                player,
                dimension: dimensions
                    .get(&entity)
                    .and_then(|&component| memory.read::<i32>(component))
                    .filter(|dimension| (0..1024).contains(dimension)),
                position,
                rotation,
                health: attributes
                    .get(&entity)
                    .and_then(|&component| attribute(memory, classes, component, HEALTH)),
                distance: None,
            },
        });
    }
    if mismatched_states > 0 && actors.is_empty() {
        return Err(format!(
            "layout: the state vector after Actor::mLevel is not the StateVectorComponent for all {mismatched_states} actors"
        ));
    }
    if actors.is_empty() && skipped > 0 {
        return Err(format!(
            "layout: all {skipped} actors failed their checks (Actor::mLevel or the components it points at)"
        ));
    }
    Ok((actors, skipped))
}

/// The offset of `Actor::mLevel`: the word that holds the level, found once.
fn level_field(memory: &mut Memory, actor: usize, level: usize) -> Option<usize> {
    let known = ACTOR_LEVEL.load(Ordering::Acquire);
    if known != 0 {
        return (memory.read::<usize>(actor + known) == Some(level)).then_some(known);
    }
    let found = (ACTOR_LEVEL_FROM..ACTOR_LEVEL_TO)
        .step_by(8)
        .find(|offset| memory.read::<usize>(actor + offset) == Some(level))?;
    log!("world_state: Actor::mLevel at {found:#x}");
    ACTOR_LEVEL.store(found, Ordering::Release);
    Some(found)
}

/// Feet position, and (pitch, yaw) when readable, through `mBuiltInComponents`, 64 bytes after
/// `mLevel`. The state vector's position can differ from the AABB bottom, so the code uses the
/// AABB bottom as the feet.
fn transform(memory: &mut Memory, level_field: usize) -> Option<([f32; 3], Option<[f32; 2]>)> {
    let state = memory.read::<usize>(level_field + LEVEL_TO_STATE_VECTOR)?;
    let aabb = memory.read::<usize>(level_field + LEVEL_TO_AABB)?;
    // Pooled components are packed at their own alignment: a 36-byte state vector is only
    // 4-aligned.
    if !state.is_multiple_of(4)
        || !aabb.is_multiple_of(4)
        || !memory.readable(state, STATE_VECTOR_SIZE)
        || !memory.readable(aabb, AABB_SIZE)
    {
        return None;
    }
    let mut position = memory.read::<[f32; 3]>(state)?;
    let [min, max] = memory.read::<[[f32; 3]; 2]>(aabb)?;
    let finite = |values: &[f32]| {
        values
            .iter()
            .all(|value| value.is_finite() && value.abs() < MAX_COORDINATE)
    };
    // The box holds the position, and its bottom lies at most a tall mob's height below.
    if !finite(&position)
        || !finite(&min)
        || !finite(&max)
        || (0..3).any(|axis| min[axis] > max[axis])
        || !(position[1] - MAX_FEET_DROP..=position[1] + 0.01).contains(&min[1])
    {
        return None;
    }
    position[1] = min[1];
    let rotation = memory
        .read::<usize>(level_field + LEVEL_TO_ROTATION)
        .filter(|&rotation| rotation.is_multiple_of(4) && memory.readable(rotation, 16))
        .and_then(|rotation| memory.read::<[f32; 2]>(rotation))
        .filter(|rotation| finite(rotation));
    Some((position, rotation))
}

/// An MSVC `std::string`, exact up to `max` bytes; whether it was cut.
fn text(memory: &mut Memory, address: usize, max: usize) -> Option<(String, bool)> {
    let (buffer, size, capacity) = memory.read::<([u8; 16], usize, usize)>(address)?;
    if capacity < size || capacity > 1 << 30 {
        return None;
    }
    let data = if capacity >= 16 {
        usize::from_le_bytes(buffer[..8].try_into().ok()?)
    } else {
        address
    };
    let length = size.min(max);
    if !memory.readable(data, length) {
        return None;
    }
    // SAFETY: checked readable above.
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
    Some((String::from_utf8_lossy(bytes).into_owned(), length < size))
}

fn identifier(memory: &mut Memory, component: usize) -> Option<String> {
    let valid = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'<' && byte != b'>')
    };
    if let Some((canonical, false)) = text(memory, component + IDENTIFIER_CANONICAL, MAX_NAME)
        && valid(&canonical)
        && canonical.contains(':')
    {
        return Some(canonical);
    }
    let (namespace, _) = text(memory, component + IDENTIFIER_NAMESPACE, MAX_NAME)?;
    let (name, _) = text(memory, component + IDENTIFIER_NAME, MAX_NAME)?;
    let joined = format!("{namespace}:{name}");
    (valid(&namespace) && valid(&name)).then_some(joined)
}

/// The `SynchedActorDataComponent` layout, as (stride, offset of the item vector): one under
/// which every sampled live component holds a plausible item vector. The layout found last is
/// tried first, then LeviLamina's, then the others by stride: the vector within the first
/// [`SYNCHED_ITEMS_SPAN`] bytes of a component and inside it. With two live components in the
/// sample, the second one's vector pins the stride; a lone component reads the same under any.
fn synched_layout(memory: &mut Memory, pool: &Pool) -> Option<(usize, usize)> {
    let fits = |memory: &mut Memory, (stride, items): (usize, usize)| {
        let mut sampled = 0;
        (0..pool.count.min(SYNCHED_SAMPLE)).all(|index| {
            if pool
                .entity(memory, index)
                .is_none_or(|entity| !live(entity))
            {
                return true;
            }
            sampled += 1;
            pool.component(memory, index, stride)
                .is_some_and(|component| item_vector(memory, component + items))
        }) && sampled > 0
    };
    let last = SYNCHED_LAYOUT.load(Ordering::Acquire);
    let searched = (VECTOR_SIZE..=MAX_SYNCHED_STRIDE)
        .step_by(8)
        .flat_map(|stride| {
            (0..=stride - VECTOR_SIZE)
                .step_by(8)
                .take_while(|&items| items < SYNCHED_ITEMS_SPAN)
                .map(move |items| (stride, items))
        });
    let found = (last != 0)
        .then_some((last >> 8, last & 0xFF))
        .into_iter()
        .chain([(SYNCHED_SIZE, 0)])
        .chain(searched)
        .find(|&layout| fits(memory, layout))?;
    let key = found.0 << 8 | found.1;
    if SYNCHED_LAYOUT.swap(key, Ordering::AcqRel) != key {
        log!(
            "world_state: SynchedActorDataComponent stride {}, item vector at +{}",
            found.0,
            found.1
        );
    }
    Some(found)
}

/// Whether `address` holds a `SynchedActorData` item vector: items indexed by their id, each
/// with a known `DataItemType`.
fn item_vector(memory: &mut Memory, address: usize) -> bool {
    let Some((first, count)) = memory.vector(address, 8, MAX_ITEMS) else {
        return false;
    };
    let mut items = 0;
    let plausible = (0..count.min(SYNCHED_CHECK_ITEMS)).all(|index| {
        let Some(item) = memory.read::<usize>(first + index * 8) else {
            return false;
        };
        if item == 0 {
            return true;
        }
        items += 1;
        memory.object(item, ITEM_VALUE)
            && memory
                .read::<u8>(item + ITEM_TYPE)
                .is_some_and(|kind| kind < ITEM_TYPE_UNKNOWN)
            && memory.read::<u16>(item + ITEM_ID) == Some(index as u16)
    });
    plausible && items > 0
}

fn name_tag(memory: &mut Memory, classes: &Classes, items: usize) -> Option<String> {
    let (first, count) = memory.vector(items, 8, MAX_ITEMS)?;
    (0..count).find_map(|index| {
        let item = memory.read::<usize>(first + index * 8)?;
        if item == 0
            || !image_vtable(memory, classes, item)
            || !memory.object(item, ITEM_VALUE + 32)
            || memory.read::<u8>(item + ITEM_TYPE) != Some(ITEM_TYPE_STRING)
            || memory.read::<u16>(item + ITEM_ID) != Some(ITEM_ID_NAME)
        {
            return None;
        }
        text(memory, item + ITEM_VALUE, MAX_NAME).map(|(name, _)| name)
    })
}

/// The attribute `name` (current and maximum) from the attribute map: a flat map of ids and
/// `AttributeInstance`s.
fn attribute(
    memory: &mut Memory,
    classes: &Classes,
    component: usize,
    name: &str,
) -> Option<[f32; 2]> {
    let (_, keys) = memory.vector(component, 4, MAX_ATTRIBUTES)?;
    let [first, last] = memory.read::<[usize; 2]>(component + 24)?;
    if keys == 0 || first == 0 || last < first || (last - first) % keys != 0 {
        return None;
    }
    let stride = (last - first) / keys;
    if !(INSTANCE_CURRENT + 12..=1024).contains(&stride) || !stride.is_multiple_of(8) {
        return None;
    }
    (0..keys).find_map(|index| {
        let instance = first + index * stride;
        if !image_vtable(memory, classes, instance)
            || !memory.object(instance, INSTANCE_CURRENT + 12)
        {
            return None;
        }
        let attribute = memory.read::<usize>(instance + INSTANCE_ATTRIBUTE)?;
        let (found, _) = text(memory, attribute + ATTRIBUTE_NAME, 64)?;
        if found != name {
            return None;
        }
        let [min, max, current] = memory.read::<[f32; 3]>(instance + INSTANCE_CURRENT)?;
        let plausible = [min, max, current].iter().all(|value| value.is_finite())
            && (0.0..=1.0e6).contains(&max)
            && (min..=max).contains(&current);
        plausible.then_some([current, max])
    })
}

/// The player named `name`, or the only player.
fn find_player<'a>(actors: &'a [Actor], name: Option<&str>) -> Result<&'a Actor, String> {
    let players: Vec<&Actor> = actors.iter().filter(|actor| actor.native.player).collect();
    if let Some(name) = name {
        return players
            .into_iter()
            .find(|actor| actor.native.name_tag.as_deref() == Some(name))
            .ok_or_else(|| format!("no player named {name:?} is on the local server"));
    }
    match players.as_slice() {
        [only] => Ok(only),
        [] => Err("no player is on the local server".into()),
        many => {
            let names: Vec<&str> = many
                .iter()
                .filter_map(|actor| actor.native.name_tag.as_deref())
                .collect();
            Err(format!(
                "{} players are on the local server ({}); name one",
                many.len(),
                names.join(", ")
            ))
        }
    }
}

/// The player's state: what every entity read gives, plus hunger, saturation, the selected
/// slot and effects, each `None` with a note when its checks fail.
fn read_player(
    memory: &mut Memory,
    classes: &Classes,
    world: &World,
    actors: &[Actor],
    name: Option<&str>,
) -> Result<NativePlayer, String> {
    let actor = find_player(actors, name)?;
    let mut notes = Vec::new();
    let attributes = world
        .pools
        .get(ATTRIBUTES)
        .and_then(|pool| pool.find(memory, actor.entity, ATTRIBUTES_SIZE));
    let mut read_attribute = |memory: &mut Memory, name: &str| {
        let value = attributes.and_then(|component| attribute(memory, classes, component, name));
        if value.is_none() {
            notes.push(format!(
                "{name} is not in the player's AttributesComponent or failed its checks"
            ));
        }
        value
    };
    let hunger = read_attribute(memory, HUNGER);
    let saturation = read_attribute(memory, SATURATION);
    let selected_slot = selected_slot(memory, classes, actor.address)
        .inspect_err(|reason| notes.push(format!("selected slot: {reason}")))
        .ok();
    let effects = match world.pools.get(MOB_EFFECTS) {
        None => {
            notes.push("effects: the MobEffectsComponent pool was not found".to_owned());
            None
        }
        Some(pool) => match pool.find(memory, actor.entity, MOB_EFFECTS_SIZE) {
            // No component: the player has no effects stored.
            None => Some(Vec::new()),
            Some(component) => {
                let found = effects(memory, component);
                if found.is_none() {
                    notes.push(
                        "effects: the MobEffectsComponent failed its checks (an effect away \
                         from its id's slot, or an amplifier or nested vector out of range)"
                            .to_owned(),
                    );
                }
                found
            }
        },
    };
    Ok(NativePlayer {
        tick: 0,
        entity: actor.native.clone(),
        hunger,
        saturation,
        selected_slot,
        effects,
        players: actors.iter().filter(|actor| actor.native.player).count() as u32,
        notes,
        read_us: 0,
    })
}

/// `PlayerInventory::mSelected` through `Player::mInventory`: tried where it was found last,
/// then where LeviLamina's headers put it, then at every word of the player object.
fn selected_slot(memory: &mut Memory, classes: &Classes, actor: usize) -> Result<u8, String> {
    let expected = ACTOR_LEVEL.load(Ordering::Acquire) + LEVEL_TO_INVENTORY;
    let known = INVENTORY_AT.load(Ordering::Acquire);
    let offsets = [known, expected]
        .into_iter()
        .filter(|&offset| offset != 0)
        .chain((ACTOR_LEVEL_FROM..PLAYER_SPAN).step_by(8));
    for offset in offsets {
        let Some(inventory) = memory.read::<usize>(actor + offset) else {
            continue;
        };
        if let Some(slot) = player_inventory(memory, classes, inventory) {
            if INVENTORY_AT.swap(offset, Ordering::AcqRel) != offset {
                log!(
                    "world_state: Player::mInventory at {offset:#x} (LeviLamina's is {expected:#x})"
                );
            }
            return Ok(slot);
        }
    }
    Err("no pointer in the player object leads to an object with PlayerInventory's shape".into())
}

/// The selected slot of the `PlayerInventory` at `inventory`, if it has that class's shape.
fn player_inventory(memory: &mut Memory, classes: &Classes, inventory: usize) -> Option<u8> {
    if !memory.object(inventory, INVENTORY_SIZE)
        || !image_vtable(memory, classes, inventory)
        || !image_vtable(memory, classes, inventory + 8)
        || !image_vtable(memory, classes, inventory + INVENTORY_ITEM)
        || memory.read::<i8>(inventory + INVENTORY_CONTAINER) != Some(0)
    {
        return None;
    }
    let owned = memory.read::<usize>(inventory + INVENTORY_OWNED)?;
    if !image_vtable(memory, classes, owned) {
        return None;
    }
    let slot = memory.read::<i32>(inventory + INVENTORY_SELECTED)?;
    u8::try_from(slot).ok().filter(|&slot| slot < HOTBAR_SLOTS)
}

/// The effects in a `MobEffectsComponent`, or None when one fails its checks. The game keeps
/// each effect at the index of its id and leaves the slots between as empty instances (id 0,
/// `MobEffectInstance::NO_EFFECT`), so an effect must sit at its own id and empty slots are
/// skipped.
fn effects(memory: &mut Memory, component: usize) -> Option<Vec<NativeEffect>> {
    let (first, count) = memory.vector(component, EFFECT_SIZE, MAX_EFFECTS)?;
    let mut found = Vec::new();
    for index in 0..count {
        let effect = first + index * EFFECT_SIZE;
        let id = memory.read::<u32>(effect)?;
        let duration = memory.read::<i32>(effect + EFFECT_DURATION)?;
        let amplifier = memory.read::<i32>(effect + EFFECT_AMPLIFIER)?;
        memory.vector(effect + EFFECT_NESTED, EFFECT_SIZE, MAX_EFFECTS)?;
        if id == 0 {
            continue;
        }
        if id as usize != index || !(0..256).contains(&amplifier) {
            return None;
        }
        found.push(NativeEffect {
            id,
            amplifier,
            duration,
        });
    }
    Some(found)
}

/// The answer to an entity query: the centre player, then matching entities nearest first.
fn snapshot(
    actors: &[Actor],
    skipped: u32,
    query: &Query,
    tick: u64,
    start: Instant,
) -> EntitySnapshot {
    let centre = actors
        .iter()
        .filter(|actor| actor.native.player)
        .find(|actor| {
            query
                .player
                .as_deref()
                .is_none_or(|name| actor.native.name_tag.as_deref() == Some(name))
        })
        .map(|actor| actor.native.clone());
    let distance = |entity: &NativeEntity| {
        let centre = centre.as_ref()?;
        if entity.dimension != centre.dimension {
            return None;
        }
        let squared: f32 = (0..3)
            .map(|axis| (entity.position[axis] - centre.position[axis]).powi(2))
            .sum();
        Some(squared.sqrt())
    };
    let mut entities: Vec<NativeEntity> = actors
        .iter()
        .map(|actor| NativeEntity {
            distance: distance(&actor.native),
            ..actor.native.clone()
        })
        .filter(|entity| match query.id {
            Some(id) => entity.id == id,
            None => {
                centre.as_ref().is_none_or(|centre| centre.id != entity.id)
                    && query
                        .entity_type
                        .as_deref()
                        .is_none_or(|kind| entity.type_id.as_deref() == Some(kind))
                    && query.radius.is_none_or(|radius| {
                        entity
                            .distance
                            .is_some_and(|distance| f64::from(distance) <= radius)
                    })
            }
        })
        .collect();
    entities.sort_by(|a, b| {
        a.distance
            .unwrap_or(f32::INFINITY)
            .total_cmp(&b.distance.unwrap_or(f32::INFINITY))
    });
    let total = entities.len() as u32;
    let limit = query
        .limit
        .map_or(DEFAULT_LIMIT, |limit| limit as usize)
        .min(MAX_LIMIT);
    entities.truncate(limit);
    EntitySnapshot {
        tick,
        centre,
        entities,
        total,
        skipped,
        read_us: start.elapsed().as_micros() as u64,
    }
}

fn properties(
    memory: &mut Memory,
    classes: &Classes,
    world: &World,
    actors: &Result<(Vec<Actor>, u32), String>,
    target: &PropertyTarget,
    collection: Option<&str>,
) -> Result<PropertySnapshot, String> {
    let mut snapshot = PropertySnapshot {
        tick: 0,
        entity: None,
        collections: Vec::new(),
        truncated: false,
        read_us: 0,
    };
    let map = match target {
        PropertyTarget::World => world_properties(memory, classes, world.level)?,
        PropertyTarget::Player { name } => {
            let actor = actors
                .as_ref()
                .map_err(Clone::clone)?
                .0
                .iter()
                .filter(|actor| actor.native.player)
                .find(|actor| {
                    name.as_deref()
                        .is_none_or(|name| actor.native.name_tag.as_deref() == Some(name))
                })
                .ok_or_else(|| match name {
                    Some(name) => format!("no player named {name:?} is on the local server"),
                    None => "no player is on the local server".to_owned(),
                })?;
            snapshot.entity = Some(actor.native.id);
            entity_properties(memory, world, actor.entity)?
        }
        PropertyTarget::Entity { id } => {
            let actor = actors
                .as_ref()
                .map_err(Clone::clone)?
                .0
                .iter()
                .find(|actor| actor.native.id == *id)
                .ok_or_else(|| format!("no entity with id {id} is on the local server"))?;
            snapshot.entity = Some(*id);
            entity_properties(memory, world, actor.entity)?
        }
    };
    if let Some(map) = map {
        snapshot.collections = collections(memory, map, collection, &mut snapshot.truncated)?;
    }
    Ok(snapshot)
}

/// The world's `DynamicProperties`, or None before the world stored any.
fn world_properties(
    memory: &mut Memory,
    classes: &Classes,
    level: usize,
) -> Result<Option<usize>, String> {
    let own = (LEVEL_OWN_FROM..LEVEL_SPAN - LEVEL_PROPERTIES_MANAGER - 8)
        .step_by(8)
        .find(|&offset| {
            pack_managers(memory, classes, level + offset)
                && memory
                    .read::<usize>(level + offset + LEVEL_MOB_EVENTS)
                    .is_some_and(|word| mob_events(memory, classes, word))
        })
        .ok_or(
            "layout: ServerLevel's ResourcePackManager references and mMobEvents were not found",
        )?;
    let manager = memory
        .read::<usize>(level + own + LEVEL_PROPERTIES_MANAGER)
        .ok_or("layout: ServerLevel::mDynamicPropertiesManager is not readable")?;
    if !LOGGED_WORLD_PATH.swap(true, Ordering::AcqRel) {
        let head = memory.read::<[u64; 6]>(manager);
        log!(
            "world_state: ServerLevel own members at {own:#x}; properties manager {manager:#x}: {head:x?}"
        );
    }
    if manager == 0 {
        return Ok(None);
    }
    if !memory.object(manager, MANAGER_PROPERTIES + 8) {
        return Err("layout: ServerLevel::mDynamicPropertiesManager is not readable".into());
    }
    let properties = memory
        .read::<usize>(manager + MANAGER_PROPERTIES)
        .ok_or("layout: DynamicPropertiesManager::mLevelDynamicProperties is not readable")?;
    Ok((properties != 0).then_some(properties))
}

/// An entity's `DynamicPropertiesComponent`, or None when it has none.
fn entity_properties(
    memory: &mut Memory,
    world: &World,
    entity: u32,
) -> Result<Option<usize>, String> {
    let pool = world.pools.get(PROPERTIES).ok_or(
        "no entity has dynamic properties yet (the game made no DynamicPropertiesComponent pool)",
    )?;
    Ok(pool.index(memory, PROPERTIES_SIZE).get(&entity).copied())
}

/// Whether the two words at `at` point at objects of one class, as
/// `ServerLevel::mServerResourcePackManager` and `mClientResourcePackManager` do.
fn pack_managers(memory: &mut Memory, classes: &Classes, at: usize) -> bool {
    let Some([server, client]) = memory.read::<[usize; 2]>(at) else {
        return false;
    };
    image_vtable(memory, classes, server)
        && image_vtable(memory, classes, client)
        && memory.read::<usize>(server) == memory.read::<usize>(client)
}

/// Whether `object` is a `MobEvents`: an object with a vtable whose events carry every name
/// in [`MOB_EVENT_NAMES`].
fn mob_events(memory: &mut Memory, classes: &Classes, object: usize) -> bool {
    if !image_vtable(memory, classes, object) || !memory.object(object, MOB_EVENTS_SPAN) {
        return false;
    }
    let mut seen = [false; MOB_EVENT_NAMES.len()];
    for offset in (8..MOB_EVENTS_SPAN).step_by(8) {
        if let Some((name, false)) = text(memory, object + offset, MAX_NAME)
            && let Some(index) = MOB_EVENT_NAMES.iter().position(|known| *known == name)
        {
            seen[index] = true;
        }
    }
    seen.iter().all(|&seen| seen)
}

/// The nodes of an MSVC `std::unordered_map`, checked as a hash table and a closed list.
fn map_nodes(
    memory: &mut Memory,
    map: usize,
    max: usize,
    truncated: &mut bool,
) -> Result<Vec<usize>, String> {
    let bad = || "layout: a dynamic property map is not an unordered_map".to_owned();
    let [head, size] = memory.read::<[usize; 2]>(map + MAP_HEAD).ok_or_else(bad)?;
    let [first, last] = memory
        .read::<[usize; 2]>(map + MAP_VECTOR)
        .ok_or_else(bad)?;
    let [mask, buckets] = memory.read::<[usize; 2]>(map + MAP_MASK).ok_or_else(bad)?;
    if !buckets.is_power_of_two()
        || buckets < 8
        || mask != buckets - 1
        || last.wrapping_sub(first) != buckets * 16
        || !memory.object(head, NODE_VALUE)
        || size > 1 << 24
    {
        return Err(bad());
    }
    let mut nodes = Vec::with_capacity(size.min(max));
    let mut previous = head;
    let mut node = memory.read::<usize>(head).ok_or_else(bad)?;
    while node != head {
        if nodes.len() == max {
            *truncated = true;
            return Ok(nodes);
        }
        if nodes.len() > size
            || !memory.object(node, NODE_VALUE)
            || memory.read::<usize>(node + 8) != Some(previous)
        {
            return Err(bad());
        }
        nodes.push(node);
        previous = node;
        node = memory.read::<usize>(node).ok_or_else(bad)?;
    }
    if nodes.len() != size {
        return Err(bad());
    }
    Ok(nodes)
}

fn collections(
    memory: &mut Memory,
    map: usize,
    only: Option<&str>,
    truncated: &mut bool,
) -> Result<Vec<PropertyCollection>, String> {
    let mut budget = MAX_TEXT_TOTAL;
    let mut collections = Vec::new();
    for node in map_nodes(memory, map, MAX_COLLECTIONS, truncated)? {
        if !memory.object(node, COLLECTION_MAP + 64) {
            return Err("layout: a property collection is not readable".into());
        }
        let (name, _) = text(memory, node + NODE_VALUE, MAX_NAME)
            .ok_or("layout: a property collection's name is not a string")?;
        if only.is_some_and(|only| only != name) {
            continue;
        }
        let byte_count = memory
            .read::<u64>(node + COLLECTION_BYTES)
            .unwrap_or_default();
        let mut properties = Vec::new();
        for property in map_nodes(memory, node + COLLECTION_MAP, MAX_PROPERTIES, truncated)? {
            if !memory.object(property, PROPERTY_INDEX + 8) {
                return Err("layout: a dynamic property is not readable".into());
            }
            let (key, _) = text(memory, property + NODE_VALUE, MAX_NAME)
                .ok_or("layout: a dynamic property's key is not a string")?;
            let at = property + PROPERTY_VALUE;
            let value = match memory.read::<u8>(property + PROPERTY_INDEX) {
                Some(0) => memory
                    .read::<f64>(at)
                    .filter(|value| value.is_finite())
                    .map(PropertyValue::Double),
                Some(1) => memory
                    .read::<f32>(at)
                    .filter(|value| value.is_finite())
                    .map(PropertyValue::Float),
                Some(2) => match memory.read::<u8>(at) {
                    Some(0) => Some(PropertyValue::Bool(false)),
                    Some(1) => Some(PropertyValue::Bool(true)),
                    _ => return Err("layout: a boolean dynamic property is not 0 or 1".into()),
                },
                Some(3) => {
                    let (value, cut) = text(memory, at, MAX_VALUE.min(budget))
                        .ok_or("layout: a string dynamic property is not a string")?;
                    budget -= value.len();
                    *truncated |= cut;
                    Some(PropertyValue::String(value))
                }
                Some(4) => memory
                    .read::<[f32; 3]>(at)
                    .filter(|vector| vector.iter().all(|value| value.is_finite()))
                    .map(PropertyValue::Vector),
                _ => return Err("layout: a dynamic property's type index is out of range".into()),
            };
            match value {
                Some(value) => properties.push(Property { key, value }),
                // A NaN or infinite number has no JSON form.
                None => *truncated = true,
            }
        }
        properties.sort_by(|a, b| a.key.cmp(&b.key));
        collections.push(PropertyCollection {
            name,
            byte_count,
            properties,
        });
    }
    collections.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(collections)
}

/// Queue `job` for the next server tick and wait for its answer.
fn ask(job: Job) -> Result<Answer, String> {
    if CLASSES.get().is_none() {
        let detail = capability::all()
            .into_iter()
            .find(|capability| capability.name == NAME)
            .map(|capability| capability.detail)
            .unwrap_or_default();
        return Err(format!("world_state is not installed: {detail}"));
    }
    let id = NEXT_JOB.fetch_add(1, Ordering::AcqRel) + 1;
    {
        let mut jobs = JOBS.lock().map_err(|_| "the job queue is poisoned")?;
        jobs.push((id, job));
        QUEUED.store(jobs.len(), Ordering::Release);
    }
    let deadline = Instant::now() + WAIT;
    let mut answers = ANSWERS.lock().map_err(|_| "the answers are poisoned")?;
    loop {
        if let Some(at) = answers.iter().position(|(answered, _)| *answered == id) {
            return Ok(answers.swap_remove(at).1);
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        answers = ANSWERED
            .wait_timeout(answers, deadline - now)
            .map_err(|_| "the answers are poisoned")?
            .0;
    }
    drop(answers);
    if let Ok(mut jobs) = JOBS.lock() {
        jobs.retain(|(queued, _)| *queued != id);
        QUEUED.store(jobs.len(), Ordering::Release);
    }
    Err(NOT_TICKING.into())
}

pub fn entities(query: Query) -> Result<EntitySnapshot, String> {
    match ask(Job::Entities(query))? {
        Answer::Entities(answer) => answer,
        _ => Err("mismatched answer".into()),
    }
}

pub fn player(name: Option<String>) -> Result<NativePlayer, String> {
    match ask(Job::Player(name))? {
        Answer::Player(answer) => answer,
        _ => Err("mismatched answer".into()),
    }
}

pub fn dynamic_properties(
    target: PropertyTarget,
    collection: Option<String>,
) -> Result<PropertySnapshot, String> {
    match ask(Job::Properties { target, collection })? {
        Answer::Properties(answer) => answer,
        _ => Err("mismatched answer".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(count: usize, capacity: usize, pages: usize) -> Pool {
        Pool {
            packed: 0,
            count,
            capacity,
            payload: 0,
            pages,
            page: FIRST_PAGE,
        }
    }

    #[test]
    fn small_pools_leave_the_page_size_open() {
        assert_eq!(page_sizes(&[shape(0, 0, 0), shape(50, 64, 1)]), PAGE_SIZES);
    }

    #[test]
    fn a_pool_past_one_small_page_decides_the_page_size() {
        // 200 entries: two pages of 128, or one of 1024.
        assert_eq!(page_sizes(&[shape(50, 64, 1), shape(200, 211, 2)]), [128]);
        assert_eq!(page_sizes(&[shape(50, 64, 1), shape(200, 211, 1)]), [1024]);
        // Entries removed since keep their pages and packed's capacity.
        assert_eq!(page_sizes(&[shape(10, 316, 3)]), [128]);
        // Reserved room has its pages too.
        assert_eq!(page_sizes(&[shape(10, 2000, 16)]), [128]);
        // Two pages under room reserved past 1024 pass for either size: actors decide.
        assert_eq!(page_sizes(&[shape(10, 2000, 2)]), PAGE_SIZES);
    }

    #[test]
    fn pools_that_disagree_fit_no_page_size() {
        assert!(page_sizes(&[shape(200, 211, 2), shape(200, 211, 1)]).is_empty());
        assert!(page_sizes(&[shape(300, 316, 5)]).is_empty());
    }

    #[test]
    fn actors_past_the_first_page_decide_an_open_page_size() {
        const REGISTRY: usize = 0x1234_5678;
        // 200 actors in one page of 1024; the pool's shape also fits pages of 128.
        let actors: Vec<Vec<u64>> = (0..200u64)
            .map(|entity| {
                let mut actor = vec![0u64; ACTOR_LEVEL_TO / 8];
                actor[ACTOR_REGISTRY / 8] = REGISTRY as u64;
                actor[ACTOR_ENTITY / 8] = entity;
                actor
            })
            .collect();
        let mut first = vec![0u64; 1024];
        for (slot, actor) in first.iter_mut().zip(&actors) {
            *slot = actor.as_ptr() as u64;
        }
        let second = vec![0u64; 1024];
        let packed: Vec<u32> = (0..200).collect();
        let pages = [first.as_ptr() as usize, second.as_ptr() as usize];
        let owner = Pool {
            packed: packed.as_ptr() as usize,
            payload: pages.as_ptr() as usize,
            ..shape(200, 2000, 2)
        };
        let mut memory = Memory::default();
        assert_eq!(
            calibrate_page(&mut memory, REGISTRY, &owner, [&owner]),
            Ok(1024)
        );
        assert_eq!(PAGE.load(Ordering::Acquire), 1024);
    }

    #[test]
    fn the_payload_is_found_where_its_actors_match_their_entity_contexts() {
        const REGISTRY: usize = 0x1234_5678;
        let actors: Vec<Vec<u64>> = (0..4u64)
            .map(|entity| {
                let mut actor = vec![0u64; ACTOR_LEVEL_TO / 8];
                actor[ACTOR_REGISTRY / 8] = REGISTRY as u64;
                actor[ACTOR_ENTITY / 8] = entity;
                actor
            })
            .collect();
        let mut page = vec![0u64; FIRST_PAGE];
        for (slot, actor) in page.iter_mut().zip(&actors) {
            *slot = actor.as_ptr() as u64;
        }
        let packed: Vec<u32> = (0..4).collect();
        let pages = [page.as_ptr() as u64];
        // A pool whose `payload` is not where EnTT puts it.
        let at = 112;
        let mut owner = vec![0u64; POOL_PAYLOAD_SPAN / 8];
        let first = packed.as_ptr() as u64;
        owner[POOL_PACKED / 8..POOL_PACKED / 8 + 3].copy_from_slice(&[
            first,
            first + 16,
            first + 16,
        ]);
        let first = pages.as_ptr() as u64;
        owner[at / 8..at / 8 + 3].copy_from_slice(&[first, first + 8, first + 8]);
        let mut memory = Memory::default();
        assert_eq!(
            calibrate_payload(&mut memory, REGISTRY, owner.as_ptr() as usize),
            Ok(at)
        );
        assert_eq!(PAYLOAD.load(Ordering::Acquire), at);
    }

    #[test]
    fn components_past_the_first_page_come_from_the_next_page() {
        let first: Vec<u64> = (0..128).collect();
        let second: Vec<u64> = (1000..1128).collect();
        let pages = [first.as_ptr() as usize, second.as_ptr() as usize];
        let pool = Pool {
            payload: pages.as_ptr() as usize,
            ..shape(200, 211, 2)
        }
        .with_page(128);
        let mut memory = Memory::default();
        let read = |memory: &mut Memory, index| {
            pool.component(memory, index, 8)
                .and_then(|address| memory.read::<u64>(address))
        };
        assert_eq!(read(&mut memory, 5), Some(5));
        assert_eq!(read(&mut memory, 130), Some(1002));
        assert_eq!(read(&mut memory, 256), None);
    }

    fn lone(id: i64, type_id: Option<&str>) -> Result<(Vec<Actor>, u32), String> {
        let native = NativeEntity {
            id,
            type_id: type_id.map(str::to_owned),
            name_tag: None,
            player: type_id == Some("minecraft:player"),
            dimension: Some(0),
            position: [0.0, 64.0, 0.0],
            rotation: None,
            health: None,
            distance: None,
        };
        Ok((
            vec![Actor {
                entity: 1,
                address: 0,
                native,
            }],
            0,
        ))
    }

    fn state() -> CapabilityState {
        capability::all()
            .into_iter()
            .find(|capability| capability.name == NAME)
            .map(|capability| capability.state)
            .expect("world_state reported")
    }

    #[test]
    fn a_lone_actor_confirms_the_layout_only_as_a_settled_player() {
        const LEVEL: Option<usize> = Some(0x1234_5678);
        // An actor that does not pass the checks yet reads as no player, however long it stays.
        for tick in (0..=200).step_by(20) {
            confirm(LEVEL, &lone(7, None), tick);
            assert_eq!(state(), CapabilityState::Pending);
        }
        // A player that keeps changing is not settled either.
        for (id, tick) in [(8, 220), (9, 300), (10, 380)] {
            confirm(LEVEL, &lone(id, Some("minecraft:player")), tick);
            assert_eq!(state(), CapabilityState::Pending);
        }
        // The same player for 60 onTick calls confirms it.
        confirm(LEVEL, &lone(10, Some("minecraft:player")), 420);
        assert_eq!(state(), CapabilityState::Pending);
        confirm(LEVEL, &lone(10, Some("minecraft:player")), 440);
        assert_eq!(state(), CapabilityState::Active);
        assert!(CONFIRMED.load(Ordering::Acquire));
    }

    /// Classes whose image is a one-slot vtable that points at itself.
    fn image() -> (Box<[usize; 1]>, Classes) {
        let mut vtable = Box::new([0usize; 1]);
        let start = vtable.as_ptr() as usize;
        vtable[0] = start;
        let classes = Classes {
            server_level: 0,
            image: start..start + 8,
        };
        (vtable, classes)
    }

    /// An MSVC `std::string` holding `text`, too long for the inline buffer.
    fn msvc_string(text: &'static str) -> [usize; 4] {
        assert!(text.len() >= 16);
        [text.as_ptr() as usize, 0, text.len(), text.len()]
    }

    #[test]
    fn hashes_are_entt_hashed_strings() {
        assert_eq!(hashed(b""), 0x811C_9DC5);
        assert_eq!(hashed(b"a"), 0xE40C_292C);
    }

    #[test]
    fn a_pool_descriptor_names_its_component_by_alias_and_hash() {
        let info = |hash: u32| {
            [
                (hash as usize) << 32 | 7,
                OWNER.as_ptr() as usize,
                OWNER.len(),
            ]
        };
        let mut memory = Memory::default();
        let good = info(hashed(OWNER.as_bytes()));
        let at = [good.as_ptr() as usize];
        assert_eq!(type_name(&mut memory, at.as_ptr() as usize), Some(OWNER));
        let bad = info(hashed(b"ActorOwner"));
        let at = [bad.as_ptr() as usize];
        assert_eq!(type_name(&mut memory, at.as_ptr() as usize), None);
    }

    #[test]
    fn mob_events_are_known_by_their_events_names() {
        let (vtable, classes) = image();
        let mut object = vec![0usize; MOB_EVENTS_SPAN / 8];
        object[0] = vtable.as_ptr() as usize;
        // Three events of 72 bytes after the vtable, as LeviLamina's header has them.
        for (index, name) in MOB_EVENT_NAMES.iter().enumerate() {
            let at = 1 + index * 9;
            object[at..at + 4].copy_from_slice(&msvc_string(name));
        }
        let mut memory = Memory::default();
        assert!(mob_events(&mut memory, &classes, object.as_ptr() as usize));
        object[10..14].fill(0);
        assert!(!mob_events(&mut memory, &classes, object.as_ptr() as usize));
    }

    /// A pool of `count` live components `stride` bytes apart, each with an item vector of two
    /// items at `items`; the vectors keep what they point at alive.
    struct Synched {
        _items: Vec<[u64; 2]>,
        _pointers: Vec<usize>,
        _page: Vec<u64>,
        _packed: Vec<u32>,
        _pages: Vec<usize>,
        pool: Pool,
    }

    fn synched(stride: usize, items: usize, count: usize) -> Synched {
        // `DataItem`s with ids 0 and 1.
        let item_list: Vec<[u64; 2]> = (0..2u64).map(|id| [0, id << 16]).collect();
        let pointers: Vec<usize> = item_list
            .iter()
            .map(|item| item.as_ptr() as usize)
            .collect();
        let mut page = vec![0u64; (count * stride).div_ceil(8) + 4];
        for index in 0..count {
            let at = (index * stride + items) / 8;
            let first = pointers.as_ptr() as u64;
            page[at..at + 3].copy_from_slice(&[first, first + 16, first + 16]);
        }
        let packed: Vec<u32> = (0..count as u32).collect();
        let pages = vec![page.as_ptr() as usize];
        let pool = Pool {
            packed: packed.as_ptr() as usize,
            payload: pages.as_ptr() as usize,
            ..shape(count, count, 1)
        };
        Synched {
            _items: item_list,
            _pointers: pointers,
            _page: page,
            _packed: packed,
            _pages: pages,
            pool,
        }
    }

    #[test]
    fn the_synched_layout_is_found_where_the_item_vectors_lie() {
        let mut memory = Memory::default();
        let levilamina = synched(SYNCHED_SIZE, 0, 3);
        assert_eq!(
            synched_layout(&mut memory, &levilamina.pool),
            Some((SYNCHED_SIZE, 0))
        );
        // Any other layout: the vector after other fields, components farther apart.
        let other = synched(96, 16, 3);
        assert_eq!(synched_layout(&mut memory, &other.pool), Some((96, 16)));
        let none = synched(96, 16, 0);
        assert_eq!(synched_layout(&mut memory, &none.pool), None);
    }

    fn player_actor(id: i64, name: &str) -> Actor {
        Actor {
            entity: id as u32,
            address: 0,
            native: NativeEntity {
                id,
                type_id: Some("minecraft:player".into()),
                name_tag: Some(name.into()),
                player: true,
                dimension: Some(0),
                position: [0.0, 64.0, 0.0],
                rotation: None,
                health: None,
                distance: None,
            },
        }
    }

    #[test]
    fn the_player_is_the_named_one_or_the_only_one() {
        let one = [player_actor(1, "Alex")];
        assert_eq!(find_player(&one, None).map(|actor| actor.native.id), Ok(1));
        let two = [player_actor(1, "Alex"), player_actor(2, "Bob")];
        assert_eq!(
            find_player(&two, Some("Bob")).map(|actor| actor.native.id),
            Ok(2)
        );
        let error = find_player(&two, None).err().unwrap();
        assert!(
            error.contains("2 players") && error.contains("Alex, Bob"),
            "{error}"
        );
        assert!(find_player(&two, Some("Steve")).is_err());
        assert!(find_player(&[], None).is_err());
    }

    #[test]
    fn the_selected_slot_is_found_behind_a_player_inventory_pointer() {
        let (vtable, classes) = image();
        let vtable = vtable.as_ptr() as usize;
        let owned = [vtable];
        // A `PlayerInventory` with slot 4 selected, as LeviLamina's header lays it out.
        let mut inventory = vec![0usize; INVENTORY_SIZE / 8];
        inventory[0] = vtable;
        inventory[1] = vtable;
        inventory[INVENTORY_SELECTED / 8] = 4;
        inventory[INVENTORY_ITEM / 8] = vtable;
        inventory[INVENTORY_OWNED / 8] = owned.as_ptr() as usize;
        // Something with one vtable comes first in the player and is passed over.
        let mut decoy = vec![0usize; INVENTORY_SIZE / 8];
        decoy[0] = vtable;
        decoy[INVENTORY_SELECTED / 8] = 4;
        let mut actor = vec![0usize; PLAYER_SPAN / 8];
        actor[0x300 / 8] = decoy.as_ptr() as usize;
        actor[0x700 / 8] = inventory.as_ptr() as usize;
        let mut memory = Memory::default();
        let address = actor.as_ptr() as usize;
        assert_eq!(selected_slot(&mut memory, &classes, address), Ok(4));
        assert_eq!(INVENTORY_AT.load(Ordering::Acquire), 0x700);
        // A slot past the hotbar or another container fails the shape.
        inventory[INVENTORY_SELECTED / 8] = 9;
        assert!(selected_slot(&mut memory, &classes, address).is_err());
        inventory[INVENTORY_SELECTED / 8] = 2;
        inventory[INVENTORY_CONTAINER / 8] = 119;
        assert!(selected_slot(&mut memory, &classes, address).is_err());
    }

    /// A `MobEffectsComponent` vector as the game keeps it: `slots` 168-byte
    /// `MobEffectInstance`s, empty (id 0) but for these (index, id, duration, amplifier).
    fn effect_list(slots: usize, effects: &[(usize, u32, i32, i32)]) -> Vec<u64> {
        let mut list = vec![0u64; slots * EFFECT_SIZE / 8];
        for &(index, id, duration, amplifier) in effects {
            let at = index * EFFECT_SIZE / 8;
            list[at] = u64::from(id) | u64::from(duration as u32) << 32;
            list[at + EFFECT_AMPLIFIER / 8] = amplifier as u32 as u64;
        }
        list
    }

    fn read_effects(memory: &mut Memory, list: &[u64]) -> Option<Vec<NativeEffect>> {
        let first = list.as_ptr() as usize;
        let end = first + list.len() * 8;
        let component = [first, end, end];
        effects(memory, component.as_ptr() as usize)
    }

    #[test]
    fn effects_are_read_from_their_id_slots_in_the_mob_effects_vector() {
        let mut memory = Memory::default();
        // Speed II and Hunger as the live game held them: slots 0 and 2 to 16 are empty.
        let list = effect_list(18, &[(1, 1, 1191, 1), (17, 17, 595, 0)]);
        assert_eq!(
            read_effects(&mut memory, &list),
            Some(vec![
                NativeEffect {
                    id: 1,
                    amplifier: 1,
                    duration: 1191
                },
                NativeEffect {
                    id: 17,
                    amplifier: 0,
                    duration: 595
                },
            ])
        );
        // Every effect gone leaves only empty slots, or no vector at all.
        assert_eq!(
            read_effects(&mut memory, &effect_list(18, &[])),
            Some(Vec::new())
        );
        let empty = [0usize; 3];
        assert_eq!(
            effects(&mut memory, empty.as_ptr() as usize),
            Some(Vec::new())
        );
        // An amplifier out of range, or an effect away from its id's slot, fails the read.
        assert_eq!(
            read_effects(&mut memory, &effect_list(2, &[(1, 1, 600, 1000)])),
            None
        );
        assert_eq!(
            read_effects(&mut memory, &effect_list(3, &[(1, 2, 600, 0)])),
            None
        );
    }
}

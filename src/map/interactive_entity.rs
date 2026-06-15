//! Interactive entities: a parallel, sparse per-tile store of reference-type
//! gameplay objects (chargers today) that actors can use.
//!
//! Unlike [`CellType`](crate::map::world_map::CellType) — a dense value baked
//! into every hypermap cell — interactive entities are *sparse* (a handful per
//! chunk) and *stateful* (they mutate at runtime: charge level, occupancy, an
//! `is_used` flag). They are kept in a separate "submap" so the dense tile grid
//! stays a plain value array.
//!
//! Three layers, smallest to largest:
//!
//! - [`InteractiveEntity`] — a serializable enum of concrete entity kinds
//!   (only [`ChargerEntity`] today). The shared interface lives in the
//!   [`InteractiveEntityBehavior`] trait, implemented on the enum so callers can
//!   ask for type / coordinates / props / `is_used` without matching.
//! - [`HypertileList<T>`] — a generic list of items sharing one hypertile, with
//!   [`InteractiveEntityHypertileList`] (a list of [`InteractiveEntityEntry`])
//!   as the concrete specialization. One hypertile can hold **more than one**
//!   entity.
//! - [`InteractiveEntityMap`] — the `Resource`: a sparse map from
//!   [`EntityCoordinates`] to that tile's list. `entities_at` returns everything
//!   on a given hypertile.
//!
//! **Duplication is intentional.** An entity's `(type, coordinates)` is stored in
//! the entity itself, in its [`InteractiveEntityEntry`], and (coordinates only)
//! as the map key. Entities never move, so this never drifts — the only rule is:
//! add the entry to every index on insert and drop it from every index on
//! removal. [`InteractiveEntityMap::insert`] / [`InteractiveEntityMap::remove_all_at`]
//! enforce that; do not hand-edit the inner map.

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::prelude::*;
use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::map::hypermap::{
    world_to_chunk_local, ChunkCoord, Hypermap, HypermapChunkHandle, LocalCoord,
    HYPERMAP_CHUNK_SIZE, HYPERMAP_FLOOR_COUNT,
};
use crate::map::hypermap_pathfind::passability_walkable;
use crate::map::hypermap_world::rendered_chunks_around;
use crate::map::world_map::{CellType, ChargerFacing};

/// Property key whose presence marks an entity as "in use" (occupied/active).
/// Stored as a typed field on each entity, *not* in the free-form props map, but
/// the constant documents the well-known concept across kinds.
pub const PROP_IS_USED: &str = "is_used";

/// Default energy capacity for a freshly placed [`ChargerEntity`].
pub const CHARGER_DEFAULT_CAPACITY: f32 = 100.0;

/// World location of an interactive entity. Hypermap tiles are addressed by
/// `(x, y)` plus a vertical `floor` (`0..HYPERMAP_FLOOR_COUNT`), so an entity's
/// coordinates carry all three. Used directly as the [`InteractiveEntityMap`] key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityCoordinates {
    pub x: i32,
    pub y: i32,
    pub floor: i32,
}

impl EntityCoordinates {
    pub const fn new(x: i32, y: i32, floor: i32) -> Self {
        Self { x, y, floor }
    }

    /// Ground-floor (`0`) coordinates from a world tile.
    pub const fn ground(x: i32, y: i32) -> Self {
        Self::new(x, y, 0)
    }
}

/// Discriminant for [`InteractiveEntity`] kinds. Cheap to copy and store next to
/// an entity in an [`InteractiveEntityEntry`] for filtering without inspecting
/// the entity payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntityType {
    Charger,
    PartsDepot,
}

/// Shared interface every interactive entity exposes.
///
/// Implemented on the [`InteractiveEntity`] enum (dispatching to the active
/// variant) and on each concrete kind, so code can treat any entity uniformly:
/// query its identity, read/write free-form string props, and toggle the
/// well-known `is_used` flag.
pub trait InteractiveEntityBehavior {
    /// Kind discriminant.
    fn entity_type(&self) -> EntityType;

    /// World location (never changes after placement).
    fn coordinates(&self) -> EntityCoordinates;

    /// Free-form properties as a `String -> String` map. **Empty** when the
    /// entity has no custom properties set. Returns an owned copy.
    fn props(&self) -> HashMap<String, String>;

    /// The special "in use" flag (occupied / active).
    fn is_used(&self) -> bool;

    /// Sets the special "in use" flag.
    fn set_used(&mut self, used: bool);

    /// Inserts or overwrites a free-form property.
    fn change_prop(&mut self, key: &str, value: &str);

    /// Reads a free-form property, or `None` if unset.
    fn get_prop(&self, key: &str) -> Option<String>;
}

/// A charging station instance: the runtime, stateful counterpart of a
/// [`CellType::Charger`](crate::map::world_map::CellType) tile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChargerEntity {
    coordinates: EntityCoordinates,
    /// Wall edge the charger backs onto (mirrors the tile's facing).
    facing: ChargerFacing,
    /// Current stored energy, `0.0..=capacity`.
    charge_level: f32,
    /// Maximum stored energy.
    capacity: f32,
    /// Actor currently docked, if any. Runtime-only — Bevy [`Entity`] ids are not
    /// stable across sessions, so this is never serialized (resets to `None` on load).
    #[serde(skip)]
    occupant: Option<Entity>,
    /// The special "in use" flag.
    is_used: bool,
    /// Free-form properties.
    props: HashMap<String, String>,
}

impl ChargerEntity {
    /// New, empty charger at `coordinates` backing onto `facing`.
    pub fn new(coordinates: EntityCoordinates, facing: ChargerFacing) -> Self {
        Self {
            coordinates,
            facing,
            charge_level: 0.0,
            capacity: CHARGER_DEFAULT_CAPACITY,
            occupant: None,
            is_used: false,
            props: HashMap::new(),
        }
    }

    pub fn facing(&self) -> ChargerFacing {
        self.facing
    }

    pub fn charge_level(&self) -> f32 {
        self.charge_level
    }

    pub fn capacity(&self) -> f32 {
        self.capacity
    }

    /// Sets the charge level, clamped to `0.0..=capacity`.
    pub fn set_charge_level(&mut self, value: f32) {
        self.charge_level = value.clamp(0.0, self.capacity);
    }

    pub fn occupant(&self) -> Option<Entity> {
        self.occupant
    }

    /// Docks (or undocks with `None`) an actor; also updates [`is_used`](Self::is_used).
    pub fn set_occupant(&mut self, occupant: Option<Entity>) {
        self.occupant = occupant;
        self.is_used = occupant.is_some();
    }
}

impl InteractiveEntityBehavior for ChargerEntity {
    fn entity_type(&self) -> EntityType {
        EntityType::Charger
    }

    fn coordinates(&self) -> EntityCoordinates {
        self.coordinates
    }

    fn props(&self) -> HashMap<String, String> {
        self.props.clone()
    }

    fn is_used(&self) -> bool {
        self.is_used
    }

    fn set_used(&mut self, used: bool) {
        self.is_used = used;
    }

    fn change_prop(&mut self, key: &str, value: &str) {
        self.props.insert(key.to_string(), value.to_string());
    }

    fn get_prop(&self, key: &str) -> Option<String> {
        self.props.get(key).cloned()
    }
}

/// A parts depot instance: the runtime, stateful counterpart of a
/// [`CellType::PartsDepot`](crate::map::world_map::CellType) tile.
/// Interaction is immediate (no queue or docking wait). Behavior is wired later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartsDepotEntity {
    coordinates: EntityCoordinates,
    /// Wall edge the depot backs onto (mirrors the tile's facing).
    facing: ChargerFacing,
    /// The special "in use" flag.
    is_used: bool,
    /// Free-form properties.
    props: HashMap<String, String>,
}

impl PartsDepotEntity {
    pub fn new(coordinates: EntityCoordinates, facing: ChargerFacing) -> Self {
        Self {
            coordinates,
            facing,
            is_used: false,
            props: HashMap::new(),
        }
    }

    pub fn facing(&self) -> ChargerFacing {
        self.facing
    }
}

impl InteractiveEntityBehavior for PartsDepotEntity {
    fn entity_type(&self) -> EntityType {
        EntityType::PartsDepot
    }

    fn coordinates(&self) -> EntityCoordinates {
        self.coordinates
    }

    fn props(&self) -> HashMap<String, String> {
        self.props.clone()
    }

    fn is_used(&self) -> bool {
        self.is_used
    }

    fn set_used(&mut self, used: bool) {
        self.is_used = used;
    }

    fn change_prop(&mut self, key: &str, value: &str) {
        self.props.insert(key.to_string(), value.to_string());
    }

    fn get_prop(&self, key: &str) -> Option<String> {
        self.props.get(key).cloned()
    }
}

/// Serializable enum over every interactive entity kind. The
/// [`InteractiveEntityBehavior`] impl dispatches to the active variant, so
/// callers rarely need to match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InteractiveEntity {
    Charger(ChargerEntity),
    PartsDepot(PartsDepotEntity),
}

impl InteractiveEntity {
    /// Borrow as a [`ChargerEntity`] if that is the active kind.
    pub fn as_charger(&self) -> Option<&ChargerEntity> {
        match self {
            InteractiveEntity::Charger(c) => Some(c),
            _ => None,
        }
    }

    /// Mutably borrow as a [`ChargerEntity`] if that is the active kind.
    pub fn as_charger_mut(&mut self) -> Option<&mut ChargerEntity> {
        match self {
            InteractiveEntity::Charger(c) => Some(c),
            _ => None,
        }
    }

    /// Borrow as a [`PartsDepotEntity`] if that is the active kind.
    pub fn as_parts_depot(&self) -> Option<&PartsDepotEntity> {
        match self {
            InteractiveEntity::PartsDepot(d) => Some(d),
            _ => None,
        }
    }

    /// Mutably borrow as a [`PartsDepotEntity`] if that is the active kind.
    pub fn as_parts_depot_mut(&mut self) -> Option<&mut PartsDepotEntity> {
        match self {
            InteractiveEntity::PartsDepot(d) => Some(d),
            _ => None,
        }
    }
}

impl InteractiveEntityBehavior for InteractiveEntity {
    fn entity_type(&self) -> EntityType {
        match self {
            InteractiveEntity::Charger(c) => c.entity_type(),
            InteractiveEntity::PartsDepot(d) => d.entity_type(),
        }
    }

    fn coordinates(&self) -> EntityCoordinates {
        match self {
            InteractiveEntity::Charger(c) => c.coordinates(),
            InteractiveEntity::PartsDepot(d) => d.coordinates(),
        }
    }

    fn props(&self) -> HashMap<String, String> {
        match self {
            InteractiveEntity::Charger(c) => c.props(),
            InteractiveEntity::PartsDepot(d) => d.props(),
        }
    }

    fn is_used(&self) -> bool {
        match self {
            InteractiveEntity::Charger(c) => c.is_used(),
            InteractiveEntity::PartsDepot(d) => d.is_used(),
        }
    }

    fn set_used(&mut self, used: bool) {
        match self {
            InteractiveEntity::Charger(c) => c.set_used(used),
            InteractiveEntity::PartsDepot(d) => d.set_used(used),
        }
    }

    fn change_prop(&mut self, key: &str, value: &str) {
        match self {
            InteractiveEntity::Charger(c) => c.change_prop(key, value),
            InteractiveEntity::PartsDepot(d) => d.change_prop(key, value),
        }
    }

    fn get_prop(&self, key: &str) -> Option<String> {
        match self {
            InteractiveEntity::Charger(c) => c.get_prop(key),
            InteractiveEntity::PartsDepot(d) => d.get_prop(key),
        }
    }
}

/// One stored interactive entity plus its redundant `(type, coordinates)` tags
/// (the "special type" — `EntityType`, `EntityCoordinates`, `InteractiveEntity`).
/// The tags let queries filter by kind or location without unwrapping the
/// payload; they always agree with the entity (entities never move).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractiveEntityEntry {
    pub entity_type: EntityType,
    pub coordinates: EntityCoordinates,
    pub entity: InteractiveEntity,
}

impl InteractiveEntityEntry {
    /// Builds an entry from an entity, deriving the redundant tags from it.
    pub fn new(entity: InteractiveEntity) -> Self {
        Self {
            entity_type: entity.entity_type(),
            coordinates: entity.coordinates(),
            entity,
        }
    }
}

/// Generic ordered list of items sharing a single hypertile. A hypertile can
/// hold more than one item, so this is the value stored per tile in an
/// [`InteractiveEntityMap`]. Generic so other reference-type submaps can reuse it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HypertileList<T> {
    items: Vec<T>,
}

impl<T> Default for HypertileList<T> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

impl<T> HypertileList<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn push(&mut self, item: T) {
        self.items.push(item);
    }

    /// All items on this hypertile (empty slice if none).
    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.items.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.items.iter_mut()
    }

    /// Removes and returns every item matching `predicate`, preserving order of the rest.
    pub fn remove_matching(&mut self, mut predicate: impl FnMut(&T) -> bool) -> Vec<T> {
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.items.len() {
            if predicate(&self.items[i]) {
                removed.push(self.items.remove(i));
            } else {
                i += 1;
            }
        }
        removed
    }
}

/// Concrete per-hypertile list of interactive entities.
pub type InteractiveEntityHypertileList = HypertileList<InteractiveEntityEntry>;

/// Runtime queue state for one interactive-entity tile. `wanting` tracks actors
/// that selected this station as a target; `waiting` tracks actors that entered
/// the near-station queue and are waiting for dock turn.
#[derive(Debug, Clone, Default)]
pub struct ActorQueueState {
    wanting: VecDeque<Entity>,
    waiting: VecDeque<Entity>,
}

/// Sparse map from a hypertile to the interactive entities standing on it.
///
/// Stored as a `HashMap` (not a dense [`Hypermap`](crate::map::hypermap::Hypermap))
/// because entities are rare relative to the 128×128×10 cells per chunk — a dense
/// store would allocate a `Vec` per cell. Inserted as a Bevy `Resource` by
/// [`InteractiveEntityPlugin`].
///
/// Serializes as a flat list of [`InteractiveEntityEntry`] (each carries its own
/// coordinates), which both keeps the output compact and sidesteps the
/// "map keys must be strings" limitation many serde formats place on struct keys.
#[derive(Debug, Default, Resource)]
pub struct InteractiveEntityMap {
    tiles: HashMap<EntityCoordinates, InteractiveEntityHypertileList>,
    queues: HashMap<EntityCoordinates, ActorQueueState>,
    /// Reverse index: how many `(station, queue)` memberships each actor holds
    /// across all stations. Lets [`is_in_any_queue`](Self::is_in_any_queue) — a
    /// per-bot, per-frame query in the path follower's stuck check — be an O(1)
    /// lookup instead of scanning every station's queues. Maintained only at the
    /// (cold) queue-mutation sites; an entry exists iff its count is `> 0`.
    queued_actors: HashMap<Entity, u32>,
    /// Liveness watchdog: seconds since each queued actor last re-asserted its
    /// membership. A pursuing bot refreshes this to `0` every brain tick
    /// (`refresh_queue`); a despawned or no-longer-pursuing bot stops refreshing,
    /// its idle time climbs, and `collect_stale_queued` evicts it once it crosses
    /// the TTL — so a dead/abandoned bot can never block a charger's dock queue
    /// forever. Keyed identically to `queued_actors` (an entry exists iff the
    /// actor holds ≥ 1 membership).
    queue_idle: HashMap<Entity, f32>,
}

impl InteractiveEntityMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when no entity is stored anywhere.
    pub fn is_empty(&self) -> bool {
        self.tiles.values().all(|list| list.is_empty())
    }

    /// Total entity count across every hypertile.
    pub fn len(&self) -> usize {
        self.tiles.values().map(|list| list.len()).sum()
    }

    /// Number of hypertiles that hold at least one entity.
    pub fn occupied_tiles(&self) -> usize {
        self.tiles.values().filter(|list| !list.is_empty()).count()
    }

    /// Inserts an entity, indexing it under its own coordinates. The entity's
    /// `(type, coordinates)` become the entry tags — the single place that keeps
    /// the redundant copies in agreement.
    pub fn insert(&mut self, entity: InteractiveEntity) {
        self.insert_entry(InteractiveEntityEntry::new(entity));
    }

    /// Inserts a pre-built entry, indexing it under its `coordinates` tag.
    pub fn insert_entry(&mut self, entry: InteractiveEntityEntry) {
        self.tiles.entry(entry.coordinates).or_default().push(entry);
    }

    /// The list of entities on `coords`, if any are present.
    pub fn list_at(&self, coords: EntityCoordinates) -> Option<&InteractiveEntityHypertileList> {
        self.tiles.get(&coords).filter(|list| !list.is_empty())
    }

    /// Mutable list of entities on `coords`, if any are present.
    pub fn list_at_mut(
        &mut self,
        coords: EntityCoordinates,
    ) -> Option<&mut InteractiveEntityHypertileList> {
        self.tiles.get_mut(&coords).filter(|list| !list.is_empty())
    }

    /// Every entity standing on `coords` (empty slice if none) — the "all
    /// interactive elements on this hypertile" query.
    pub fn entities_at(&self, coords: EntityCoordinates) -> &[InteractiveEntityEntry] {
        self.tiles.get(&coords).map_or(&[], |list| list.items())
    }

    /// Removes and returns every entity on `coords`, dropping the (now empty) tile.
    pub fn remove_all_at(&mut self, coords: EntityCoordinates) -> Vec<InteractiveEntityEntry> {
        self.tiles
            .remove(&coords)
            .map(|list| list.items.into_iter().collect())
            .unwrap_or_default()
    }

    /// Removes every entity of `kind` on `coords`, returning them.
    pub fn remove_of_type_at(
        &mut self,
        coords: EntityCoordinates,
        kind: EntityType,
    ) -> Vec<InteractiveEntityEntry> {
        let removed = match self.tiles.get_mut(&coords) {
            Some(list) => list.remove_matching(|entry| entry.entity_type == kind),
            None => return Vec::new(),
        };
        if self.tiles.get(&coords).is_some_and(|list| list.is_empty()) {
            self.tiles.remove(&coords);
        }
        removed
    }

    /// Drops every entity in the map.
    pub fn clear(&mut self) {
        self.tiles.clear();
        self.queues.clear();
        self.queued_actors.clear();
        self.queue_idle.clear();
    }

    /// Iterates every entity entry across all hypertiles. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = &InteractiveEntityEntry> {
        self.tiles.values().flat_map(|list| list.iter())
    }

    /// Mutably iterates every entity entry across all hypertiles. Coordinates
    /// must not be changed (entities never move); to relocate, remove and re-insert.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut InteractiveEntityEntry> {
        self.tiles.values_mut().flat_map(|list| list.iter_mut())
    }

    // --- Locators -----------------------------------------------------------
    //
    // Three flavors of "find nearby entities", each with a different notion of
    // "near". All optionally filter by `kind` (`None` = any kind) and borrow the
    // matching entries. Because the store is sparse, every variant iterates the
    // whole map and filters — cheap relative to the dense tile grid, and there is
    // no per-tile index to maintain.

    /// **Radius-local.** Entities within `radius` tiles (Euclidean) of `center`,
    /// on the *same floor* as `center`. A `radius` of `0` matches only entities on
    /// exactly `center`'s tile. Distance is compared squared, so no floats.
    pub fn find_within_radius(
        &self,
        center: EntityCoordinates,
        radius: i32,
        kind: Option<EntityType>,
    ) -> Vec<&InteractiveEntityEntry> {
        let radius_sq = (radius as i64) * (radius as i64);
        self.iter()
            .filter(|entry| entry.coordinates.floor == center.floor)
            .filter(|entry| kind.is_none_or(|k| entry.entity_type == k))
            .filter(|entry| {
                let dx = (entry.coordinates.x - center.x) as i64;
                let dy = (entry.coordinates.y - center.y) as i64;
                dx * dx + dy * dy <= radius_sq
            })
            .collect()
    }

    /// **Hypermap-local.** Entities sitting on the chunks the renderer would keep
    /// meshed around `center` — the camera's chunk plus the prefetch neighbor on
    /// each axis, via [`rendered_chunks_around`]. This deliberately reuses the
    /// renderer's footprint so "what the camera covers" and "what this query
    /// returns" never diverge. Chunk selection is XY-only (a chunk spans every
    /// floor), so this does **not** filter by floor; combine with
    /// [`find_within_radius`](Self::find_within_radius) or a manual floor check if
    /// you need a single level.
    pub fn find_in_rendered_chunks(
        &self,
        center: EntityCoordinates,
        kind: Option<EntityType>,
    ) -> Vec<&InteractiveEntityEntry> {
        let chunks = rendered_chunks_around(center.x, center.y);
        self.iter()
            .filter(|entry| kind.is_none_or(|k| entry.entity_type == k))
            .filter(|entry| {
                let (coord, _) = world_to_chunk_local(entry.coordinates.x, entry.coordinates.y);
                chunks.contains(&coord)
            })
            .collect()
    }

    /// **Accessible.** Entities reachable from `start` within `max_steps` 4-neighbor
    /// moves over the `passability` map (`> 0.0` walkable; see
    /// [`passability_walkable`]), restricted to `floor`. An entity counts as
    /// accessible if its own tile or any 4-neighbor of it lies in the reachable set
    /// — interactive entities such as chargers back onto a wall, so their tile is
    /// often itself blocked while an actor stands on the adjacent walkable tile.
    ///
    /// `passability` is the single-floor static-passability hypermap for `floor`
    /// (the caller supplies the layer matching the level being searched).
    pub fn find_accessible_within(
        &self,
        passability: &Hypermap<f32>,
        start: (i32, i32),
        floor: i32,
        max_steps: u32,
        kind: Option<EntityType>,
    ) -> Vec<&InteractiveEntityEntry> {
        let reachable = reachable_tiles_within(passability, start, max_steps);
        self.iter()
            .filter(|entry| entry.coordinates.floor == floor)
            .filter(|entry| kind.is_none_or(|k| entry.entity_type == k))
            .filter(|entry| {
                tile_or_neighbor_reachable(&reachable, (entry.coordinates.x, entry.coordinates.y))
            })
            .collect()
    }

    /// Records one new queue membership for `actor` in the reverse index. Joining
    /// a queue counts as a fresh liveness signal, so the idle timer resets.
    fn index_add(&mut self, actor: Entity) {
        *self.queued_actors.entry(actor).or_insert(0) += 1;
        self.queue_idle.insert(actor, 0.0);
    }

    /// Drops one queue membership for `actor`, removing the entry (and its idle
    /// timer) at zero.
    fn index_remove(&mut self, actor: Entity) {
        if let Some(count) = self.queued_actors.get_mut(&actor) {
            *count -= 1;
            if *count == 0 {
                self.queued_actors.remove(&actor);
                self.queue_idle.remove(&actor);
            }
        }
    }

    /// Re-asserts that `actor` is still actively pursuing its queued station this
    /// tick, resetting its liveness idle timer. No-op for an actor that holds no
    /// membership. Called every brain tick while a charge action holds a slot.
    pub fn refresh_queue(&mut self, actor: Entity) {
        if self.queued_actors.contains_key(&actor) {
            self.queue_idle.insert(actor, 0.0);
        }
    }

    /// Ages every queued actor's idle timer by `dt` and appends to `out` the
    /// actors that have not re-asserted their membership within `ttl` seconds.
    /// The caller is expected to evict each (and reset its brain) — eviction drops
    /// the idle entry, so a returned actor is reported once per stale episode.
    /// `out` is caller-owned to keep the steady state allocation-free (it is empty
    /// every frame in the common case).
    pub fn collect_stale_queued(&mut self, dt: f32, ttl: f32, out: &mut Vec<Entity>) {
        for (actor, idle) in self.queue_idle.iter_mut() {
            *idle += dt;
            if *idle >= ttl {
                out.push(*actor);
            }
        }
    }

    /// Adds `actor` to the wanting queue for `coords` (no duplicates).
    pub fn add_wanting(&mut self, coords: EntityCoordinates, actor: Entity) {
        let queue = self.queues.entry(coords).or_default();
        let added = if !queue.wanting.contains(&actor) {
            queue.wanting.push_back(actor);
            true
        } else {
            false
        };
        if added {
            self.index_add(actor);
        }
    }

    /// Removes `actor` from the wanting queue for `coords`.
    pub fn remove_wanting(&mut self, coords: EntityCoordinates, actor: Entity) {
        self.remove_from_queue(coords, actor, false);
    }

    /// Adds `actor` to the waiting queue for `coords` (no duplicates), and
    /// removes it from wanting on the same tile.
    pub fn add_waiting(&mut self, coords: EntityCoordinates, actor: Entity) {
        let queue = self.queues.entry(coords).or_default();
        let added_waiting = if !queue.waiting.contains(&actor) {
            queue.waiting.push_back(actor);
            true
        } else {
            false
        };
        let before = queue.wanting.len();
        queue.wanting.retain(|queued| *queued != actor);
        let removed_wanting = queue.wanting.len() != before;
        if added_waiting {
            self.index_add(actor);
        }
        if removed_wanting {
            self.index_remove(actor);
        }
    }

    /// Removes `actor` from the waiting queue for `coords`.
    pub fn remove_waiting(&mut self, coords: EntityCoordinates, actor: Entity) {
        self.remove_from_queue(coords, actor, true);
    }

    /// Removes `actor` from both queues for `coords`.
    pub fn remove_actor_from_queues(&mut self, coords: EntityCoordinates, actor: Entity) {
        let mut removed = 0u32;
        if let Some(queue) = self.queues.get_mut(&coords) {
            let before = queue.wanting.len() + queue.waiting.len();
            queue.wanting.retain(|queued| *queued != actor);
            queue.waiting.retain(|queued| *queued != actor);
            removed = (before - (queue.wanting.len() + queue.waiting.len())) as u32;
            if queue.wanting.is_empty() && queue.waiting.is_empty() {
                self.queues.remove(&coords);
            }
        }
        for _ in 0..removed {
            self.index_remove(actor);
        }
    }

    /// Drops all queue state for `coords` (used when a station is rebuilt/removed).
    pub fn clear_queues_at(&mut self, coords: EntityCoordinates) {
        if let Some(queue) = self.queues.remove(&coords) {
            let members: Vec<Entity> =
                queue.wanting.iter().chain(queue.waiting.iter()).copied().collect();
            for actor in members {
                self.index_remove(actor);
            }
        }
    }

    /// Removes `actor` from **every** station's wanting and waiting queues and
    /// releases it as the occupant of any charger it holds. Used when a bot goes
    /// non-operational (broken / depleted) so it stops occupying a charger or
    /// hogging queue slots it can no longer act on. Returns `true` if anything
    /// was changed (the actor was found in at least one queue or charger).
    pub fn evict_actor_everywhere(&mut self, actor: Entity) -> bool {
        let mut changed = false;
        self.queues.retain(|_, queue| {
            let before = queue.wanting.len() + queue.waiting.len();
            queue.wanting.retain(|queued| *queued != actor);
            queue.waiting.retain(|queued| *queued != actor);
            changed |= queue.wanting.len() + queue.waiting.len() != before;
            !(queue.wanting.is_empty() && queue.waiting.is_empty())
        });
        // The actor is gone from every queue, so drop its whole index entry.
        self.queued_actors.remove(&actor);
        self.queue_idle.remove(&actor);
        for entry in self.iter_mut() {
            if let Some(charger) = entry.entity.as_charger_mut() {
                if charger.occupant() == Some(actor) {
                    charger.set_occupant(None);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Number of actors currently waiting near station `coords`.
    pub fn waiting_len(&self, coords: EntityCoordinates) -> usize {
        self.queues.get(&coords).map_or(0, |q| q.waiting.len())
    }

    /// `true` when `actor` is first in waiting queue for `coords`.
    pub fn is_waiting_front(&self, coords: EntityCoordinates, actor: Entity) -> bool {
        self.queues
            .get(&coords)
            .and_then(|q| q.waiting.front())
            .is_some_and(|front| *front == actor)
    }

    /// `true` if `actor` is in any wanting or waiting queue. O(1) via the
    /// `queued_actors` reverse index — safe to call per bot, per frame.
    pub fn is_in_any_queue(&self, actor: Entity) -> bool {
        self.queued_actors.contains_key(&actor)
    }

    /// Snapshot the wanting queue in order.
    pub fn wanting_queue(&self, coords: EntityCoordinates) -> Vec<Entity> {
        self.queues
            .get(&coords)
            .map_or_else(Vec::new, |q| q.wanting.iter().copied().collect())
    }

    /// Snapshot the waiting queue in order.
    pub fn waiting_queue(&self, coords: EntityCoordinates) -> Vec<Entity> {
        self.queues
            .get(&coords)
            .map_or_else(Vec::new, |q| q.waiting.iter().copied().collect())
    }

    fn remove_from_queue(&mut self, coords: EntityCoordinates, actor: Entity, waiting: bool) {
        let mut removed = false;
        if let Some(queue) = self.queues.get_mut(&coords) {
            let target = if waiting { &mut queue.waiting } else { &mut queue.wanting };
            let before = target.len();
            target.retain(|queued| *queued != actor);
            removed = target.len() != before;
            if queue.wanting.is_empty() && queue.waiting.is_empty() {
                self.queues.remove(&coords);
            }
        }
        if removed {
            self.index_remove(actor);
        }
    }
}

/// Rebuilds charger entities for the given world chunks from authored cell data.
///
/// For each chunk in `chunks`, existing [`EntityType::Charger`] entries in that
/// chunk are removed, then re-inserted from `map` by scanning every local cell
/// on every floor for [`CellType::Charger`].
///
/// This keeps the sparse interactive-entity index in sync with map generation
/// and edits while preserving non-charger interactive entities elsewhere.
pub fn sync_chargers_for_chunks(
    map: &Hypermap<CellType>,
    entities: &mut InteractiveEntityMap,
    chunks: impl IntoIterator<Item = ChunkCoord>,
) {
    let chunk_set: HashSet<ChunkCoord> = chunks.into_iter().collect();
    if chunk_set.is_empty() {
        return;
    }

    let stale: Vec<EntityCoordinates> = entities
        .iter()
        .filter(|entry| entry.entity_type == EntityType::Charger)
        .map(|entry| entry.coordinates)
        .filter(|coords| {
            let (chunk, _) = world_to_chunk_local(coords.x, coords.y);
            chunk_set.contains(&chunk)
        })
        .collect();
    for coords in stale {
        entities.remove_of_type_at(coords, EntityType::Charger);
        entities.clear_queues_at(coords);
    }

    for coord in chunk_set {
        let _ = map.with_chunk_read(coord, |chunk| {
            for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
                for y in 0..HYPERMAP_CHUNK_SIZE {
                    for x in 0..HYPERMAP_CHUNK_SIZE {
                        let local = LocalCoord::new(x, y);
                        let CellType::Charger(facing) = *chunk.get_local_floor(local, floor) else {
                            continue;
                        };
                        let wx = coord.x * HYPERMAP_CHUNK_SIZE + x;
                        let wy = coord.y * HYPERMAP_CHUNK_SIZE + y;
                        entities.insert(InteractiveEntity::Charger(ChargerEntity::new(
                            EntityCoordinates::new(wx, wy, floor),
                            facing,
                        )));
                    }
                }
            }
        });
    }
}

/// Rebuilds parts-depot entities for the given world chunks from authored cell data.
///
/// Mirrors [`sync_chargers_for_chunks`] but for [`EntityType::PartsDepot`].
/// Wired by the behavior layer when it is added; no queues are managed.
pub fn sync_parts_depots_for_chunks(
    map: &Hypermap<CellType>,
    entities: &mut InteractiveEntityMap,
    chunks: impl IntoIterator<Item = ChunkCoord>,
) {
    let chunk_set: HashSet<ChunkCoord> = chunks.into_iter().collect();
    if chunk_set.is_empty() {
        return;
    }

    let stale: Vec<EntityCoordinates> = entities
        .iter()
        .filter(|entry| entry.entity_type == EntityType::PartsDepot)
        .map(|entry| entry.coordinates)
        .filter(|coords| {
            let (chunk, _) = world_to_chunk_local(coords.x, coords.y);
            chunk_set.contains(&chunk)
        })
        .collect();
    for coords in stale {
        entities.remove_of_type_at(coords, EntityType::PartsDepot);
    }

    for coord in chunk_set {
        let _ = map.with_chunk_read(coord, |chunk| {
            for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
                for y in 0..HYPERMAP_CHUNK_SIZE {
                    for x in 0..HYPERMAP_CHUNK_SIZE {
                        let local = LocalCoord::new(x, y);
                        let CellType::PartsDepot(facing) = *chunk.get_local_floor(local, floor)
                        else {
                            continue;
                        };
                        let wx = coord.x * HYPERMAP_CHUNK_SIZE + x;
                        let wy = coord.y * HYPERMAP_CHUNK_SIZE + y;
                        entities.insert(InteractiveEntity::PartsDepot(PartsDepotEntity::new(
                            EntityCoordinates::new(wx, wy, floor),
                            facing,
                        )));
                    }
                }
            }
        });
    }
}

/// Reads passability cells while holding the current chunk's `Arc` handle locally
/// between accesses. A spatially coherent scan (the BFS below) therefore takes the
/// map-wide `chunks` lock only when it crosses a chunk boundary — not on every
/// cell as bare [`Hypermap::get`] would. The per-chunk lock is still taken per
/// read, held only long enough to copy one cell, so every lock scope stays tight.
struct ChunkReadCache<'a> {
    map: &'a Hypermap<f32>,
    /// `(chunk coord, handle if loaded)`. A cached `None` handle remembers a miss
    /// so repeated reads in an unloaded chunk don't re-lock the map either.
    cached: Option<(ChunkCoord, Option<HypermapChunkHandle<f32>>)>,
}

impl<'a> ChunkReadCache<'a> {
    fn new(map: &'a Hypermap<f32>) -> Self {
        Self { map, cached: None }
    }

    fn walkable(&mut self, x: i32, y: i32) -> bool {
        let (coord, local) = world_to_chunk_local(x, y);
        if !matches!(&self.cached, Some((c, _)) if *c == coord) {
            self.cached = Some((coord, self.map.get_chunk(coord)));
        }
        let Some((_, Some(handle))) = &self.cached else {
            return false; // unloaded chunk reads as the void default (not walkable)
        };
        let guard = handle.read().expect("chunk lock poisoned");
        passability_walkable(*guard.get_local_floor(local, 0))
    }
}

/// Bounded BFS: every walkable tile reachable from `start` in at most `max_steps`
/// 4-neighbor moves. Returns the empty set if `start` itself is not walkable.
/// Distance-bounded (not expansion-bounded like
/// [`explore_walkable_tiles_limited`](crate::map::hypermap_pathfind::explore_walkable_tiles_limited)),
/// which is what "reachable within n steps" means here. Cell reads go through a
/// [`ChunkReadCache`] to keep hypermap lock traffic local to chunk crossings.
fn reachable_tiles_within(
    passability: &Hypermap<f32>,
    start: (i32, i32),
    max_steps: u32,
) -> HashSet<(i32, i32)> {
    let mut reader = ChunkReadCache::new(passability);
    let mut visited = HashSet::new();
    if !reader.walkable(start.0, start.1) {
        return visited;
    }
    visited.insert(start);
    let mut frontier = VecDeque::from([(start, 0u32)]);
    while let Some((pos, dist)) = frontier.pop_front() {
        if dist == max_steps {
            continue;
        }
        let neighbors = [
            (pos.0 + 1, pos.1),
            (pos.0 - 1, pos.1),
            (pos.0, pos.1 + 1),
            (pos.0, pos.1 - 1),
        ];
        for n in neighbors {
            if !visited.contains(&n) && reader.walkable(n.0, n.1) {
                visited.insert(n);
                frontier.push_back((n, dist + 1));
            }
        }
    }
    visited
}

/// `true` if `tile` or any of its 4 neighbors is in `reachable`.
fn tile_or_neighbor_reachable(reachable: &HashSet<(i32, i32)>, tile: (i32, i32)) -> bool {
    reachable.contains(&tile)
        || [(1, 0), (-1, 0), (0, 1), (0, -1)]
            .iter()
            .any(|(dx, dy)| reachable.contains(&(tile.0 + dx, tile.1 + dy)))
}

impl Serialize for InteractiveEntityMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let entries: Vec<&InteractiveEntityEntry> = self.iter().collect();
        entries.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InteractiveEntityMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let entries = Vec::<InteractiveEntityEntry>::deserialize(deserializer)?;
        let mut map = Self::new();
        for entry in entries {
            map.insert_entry(entry);
        }
        Ok(map)
    }
}

/// Registers the [`InteractiveEntityMap`] resource.
pub struct InteractiveEntityPlugin;

impl Plugin for InteractiveEntityPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InteractiveEntityMap>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn charger_at(x: i32, y: i32, facing: ChargerFacing) -> InteractiveEntity {
        InteractiveEntity::Charger(ChargerEntity::new(EntityCoordinates::ground(x, y), facing))
    }

    #[test]
    fn behavior_dispatches_through_enum() {
        let mut e = charger_at(3, 4, ChargerFacing::North);
        assert_eq!(e.entity_type(), EntityType::Charger);
        assert_eq!(e.coordinates(), EntityCoordinates::ground(3, 4));
        assert!(e.props().is_empty(), "no props by default");
        assert!(!e.is_used());

        e.change_prop("label", "dock-A");
        assert_eq!(e.get_prop("label").as_deref(), Some("dock-A"));
        assert_eq!(e.get_prop("missing"), None);
        assert_eq!(e.props().len(), 1);

        e.set_used(true);
        assert!(e.is_used());
    }

    #[test]
    fn charger_occupancy_drives_is_used() {
        let mut charger = ChargerEntity::new(EntityCoordinates::ground(0, 0), ChargerFacing::East);
        assert!(!charger.is_used());
        charger.set_occupant(Some(Entity::PLACEHOLDER));
        assert!(charger.is_used(), "docking an occupant marks the charger used");
        charger.set_occupant(None);
        assert!(!charger.is_used());
    }

    #[test]
    fn charge_level_clamps_to_capacity() {
        let mut charger = ChargerEntity::new(EntityCoordinates::ground(0, 0), ChargerFacing::West);
        charger.set_charge_level(1_000.0);
        assert_eq!(charger.charge_level(), CHARGER_DEFAULT_CAPACITY);
        charger.set_charge_level(-5.0);
        assert_eq!(charger.charge_level(), 0.0);
    }

    #[test]
    fn map_indexes_multiple_entities_per_tile() {
        let mut map = InteractiveEntityMap::new();
        let tile = EntityCoordinates::ground(10, 20);
        map.insert(charger_at(10, 20, ChargerFacing::North));
        map.insert(charger_at(10, 20, ChargerFacing::South));
        map.insert(charger_at(11, 20, ChargerFacing::East));

        assert_eq!(map.len(), 3);
        assert_eq!(map.occupied_tiles(), 2);
        assert_eq!(map.entities_at(tile).len(), 2);
        assert_eq!(map.entities_at(EntityCoordinates::ground(99, 99)).len(), 0);
    }

    #[test]
    fn remove_drops_empty_tiles() {
        let mut map = InteractiveEntityMap::new();
        let tile = EntityCoordinates::ground(5, 5);
        map.insert(charger_at(5, 5, ChargerFacing::North));
        assert_eq!(map.remove_all_at(tile).len(), 1);
        assert!(map.list_at(tile).is_none());
        assert!(map.is_empty());
    }

    #[test]
    fn remove_of_type_clears_only_matches() {
        let mut map = InteractiveEntityMap::new();
        let tile = EntityCoordinates::ground(1, 1);
        map.insert(charger_at(1, 1, ChargerFacing::North));
        map.insert(charger_at(1, 1, ChargerFacing::East));

        let removed = map.remove_of_type_at(tile, EntityType::Charger);
        assert_eq!(removed.len(), 2);
        assert!(map.list_at(tile).is_none(), "tile dropped once empty");
    }

    #[test]
    fn map_round_trips_through_yaml() {
        let mut map = InteractiveEntityMap::new();
        let mut charger = ChargerEntity::new(EntityCoordinates::ground(2, 3), ChargerFacing::South);
        charger.set_charge_level(42.0);
        charger.change_prop("label", "dock-7");
        map.insert(InteractiveEntity::Charger(charger));

        let yaml = serde_yaml::to_string(&map).expect("serialize");
        let restored: InteractiveEntityMap = serde_yaml::from_str(&yaml).expect("deserialize");

        assert_eq!(restored.len(), 1);
        let entry = &restored.entities_at(EntityCoordinates::ground(2, 3))[0];
        assert_eq!(entry.entity_type, EntityType::Charger);
        let charger = entry.entity.as_charger().expect("charger");
        assert_eq!(charger.charge_level(), 42.0);
        assert_eq!(charger.facing(), ChargerFacing::South);
        assert_eq!(charger.get_prop("label").as_deref(), Some("dock-7"));
        assert_eq!(charger.occupant(), None, "runtime occupant not persisted");
    }

    #[test]
    fn radius_locator_is_circular_and_floor_scoped() {
        let mut map = InteractiveEntityMap::new();
        map.insert(charger_at(0, 0, ChargerFacing::North)); // center
        map.insert(charger_at(2, 0, ChargerFacing::North)); // dist 2
        map.insert(charger_at(2, 2, ChargerFacing::North)); // dist ~2.83 > 2
        // Same x/y as center but a different floor must be excluded.
        map.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::new(0, 0, 1),
            ChargerFacing::North,
        )));

        let found = map.find_within_radius(EntityCoordinates::ground(0, 0), 2, None);
        assert_eq!(found.len(), 2, "only the two within radius 2 on floor 0");

        let only_center = map.find_within_radius(EntityCoordinates::ground(0, 0), 0, None);
        assert_eq!(only_center.len(), 1);
    }

    #[test]
    fn rendered_chunk_locator_matches_render_footprint() {
        let mut map = InteractiveEntityMap::new();
        // Around world (0,0) the renderer keeps chunks (0,0), (-1,0), (0,-1).
        map.insert(charger_at(1, 1, ChargerFacing::North)); // chunk (0,0)  — in
        map.insert(charger_at(-1, 1, ChargerFacing::North)); // chunk (-1,0) — in
        map.insert(charger_at(300, 300, ChargerFacing::North)); // far chunk  — out

        let found = map.find_in_rendered_chunks(EntityCoordinates::ground(0, 0), None);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn accessible_locator_uses_pathfinding_and_adjacency() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..9 {
            passability.set(x, 0, 1.0); // straight walkable corridor along y = 0
        }
        let mut map = InteractiveEntityMap::new();
        map.insert(charger_at(2, 0, ChargerFacing::North)); // dist 2 — reachable directly
        map.insert(charger_at(4, 0, ChargerFacing::North)); // dist 4, neighbor (3,0) reachable
        map.insert(charger_at(7, 0, ChargerFacing::North)); // too far, no reachable neighbor

        let found = map.find_accessible_within(&passability, (0, 0), 0, 3, None);
        assert_eq!(found.len(), 2, "direct hit plus adjacency, far one excluded");
    }

    #[test]
    fn wanting_and_waiting_queues_are_ordered_and_unique() {
        let mut map = InteractiveEntityMap::new();
        let coords = EntityCoordinates::ground(6, 6);
        let a = Entity::from_bits(10);
        let b = Entity::from_bits(11);

        map.add_wanting(coords, a);
        map.add_wanting(coords, a);
        map.add_wanting(coords, b);
        assert_eq!(map.wanting_queue(coords), vec![a, b], "wanting queue keeps insertion order");

        map.add_waiting(coords, b);
        assert_eq!(map.wanting_queue(coords), vec![a], "moving to waiting removes from wanting");
        assert_eq!(map.waiting_queue(coords), vec![b]);
        assert!(map.is_waiting_front(coords, b));

        map.add_waiting(coords, a);
        assert_eq!(map.waiting_queue(coords), vec![b, a]);
        assert_eq!(map.waiting_len(coords), 2);
        assert!(!map.is_waiting_front(coords, a));

        map.remove_waiting(coords, b);
        assert_eq!(map.waiting_queue(coords), vec![a]);
        assert!(map.is_waiting_front(coords, a));
    }

    #[test]
    fn evict_everywhere_clears_queues_and_releases_charger() {
        let mut map = InteractiveEntityMap::new();
        let station_a = EntityCoordinates::ground(3, 3);
        let station_b = EntityCoordinates::ground(7, 7);
        let bot = Entity::from_bits(100);
        let other = Entity::from_bits(101);

        // Bot occupies charger A and is queued (waiting at A, wanting at B);
        // another bot shares those queues and must be left untouched.
        let mut charger = ChargerEntity::new(station_a, ChargerFacing::North);
        charger.set_occupant(Some(bot));
        map.insert(InteractiveEntity::Charger(charger));
        map.add_waiting(station_a, bot);
        map.add_waiting(station_a, other);
        map.add_wanting(station_b, bot);

        assert!(map.evict_actor_everywhere(bot));

        assert_eq!(map.waiting_queue(station_a), vec![other], "only the evicted bot leaves");
        assert_eq!(map.wanting_queue(station_b), Vec::<Entity>::new(), "wanting slot freed and pruned");
        let charger = map.entities_at(station_a)[0].entity.as_charger().unwrap();
        assert_eq!(charger.occupant(), None, "charger released");
        assert!(!charger.is_used());

        assert!(!map.evict_actor_everywhere(bot), "second eviction is a no-op");
    }

    #[test]
    fn is_in_any_queue_tracks_membership_across_operations() {
        let mut map = InteractiveEntityMap::new();
        let a = EntityCoordinates::ground(1, 1);
        let b = EntityCoordinates::ground(9, 9);
        let bot = Entity::from_bits(7);

        assert!(!map.is_in_any_queue(bot), "untracked bot is in no queue");

        // Wanting at one station, then promoted to waiting (same station) — the
        // promotion moves the membership, it does not double-count.
        map.add_wanting(a, bot);
        assert!(map.is_in_any_queue(bot));
        map.add_waiting(a, bot);
        assert!(map.is_in_any_queue(bot));
        map.remove_waiting(a, bot);
        assert!(!map.is_in_any_queue(bot), "leaving the only queue clears membership");

        // Membership across two distinct stations: must survive until *both* drop.
        map.add_wanting(a, bot);
        map.add_waiting(b, bot);
        assert!(map.is_in_any_queue(bot));
        map.remove_actor_from_queues(a, bot);
        assert!(map.is_in_any_queue(bot), "still waiting at the other station");
        map.clear_queues_at(b);
        assert!(!map.is_in_any_queue(bot), "clearing the last station clears membership");

        // Eviction wipes every membership at once.
        map.add_wanting(a, bot);
        map.add_waiting(b, bot);
        assert!(map.is_in_any_queue(bot));
        map.evict_actor_everywhere(bot);
        assert!(!map.is_in_any_queue(bot), "eviction drops the index entry");
    }

    #[test]
    fn stale_queue_membership_is_collected_after_ttl() {
        let mut map = InteractiveEntityMap::new();
        let station = EntityCoordinates::ground(4, 4);
        let pursuing = Entity::from_bits(1);
        let abandoned = Entity::from_bits(2);
        let ttl = 2.0;

        map.add_waiting(station, pursuing);
        map.add_waiting(station, abandoned);

        let mut stale = Vec::new();
        // One bot keeps re-asserting its slot; the other goes silent.
        for _ in 0..200 {
            map.refresh_queue(pursuing);
            stale.clear();
            map.collect_stale_queued(0.016, ttl, &mut stale);
            if !stale.is_empty() {
                break;
            }
        }
        assert_eq!(stale, vec![abandoned], "only the silent bot ages out");

        // Evicting the stale bot drops its idle timer; the pursuer survives.
        map.evict_actor_everywhere(abandoned);
        assert!(map.is_in_any_queue(pursuing));
        assert!(!map.is_in_any_queue(abandoned));

        // After eviction the pursuer (still refreshed) never goes stale.
        let mut stale2 = Vec::new();
        map.refresh_queue(pursuing);
        map.collect_stale_queued(0.016, ttl, &mut stale2);
        assert!(stale2.is_empty(), "refreshed survivor is not collected");
    }

    #[test]
    fn queue_cleanup_drops_empty_entries() {
        let mut map = InteractiveEntityMap::new();
        let coords = EntityCoordinates::ground(8, 2);
        let actor = Entity::from_bits(42);

        map.add_wanting(coords, actor);
        map.remove_wanting(coords, actor);
        assert_eq!(map.wanting_queue(coords), Vec::<Entity>::new());

        map.add_waiting(coords, actor);
        map.remove_actor_from_queues(coords, actor);
        assert_eq!(map.waiting_queue(coords), Vec::<Entity>::new());
    }
}

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

use std::collections::HashMap;

use bevy::prelude::*;
use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::map::world_map::ChargerFacing;

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

/// Serializable enum over every interactive entity kind. The
/// [`InteractiveEntityBehavior`] impl dispatches to the active variant, so
/// callers rarely need to match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InteractiveEntity {
    Charger(ChargerEntity),
}

impl InteractiveEntity {
    /// Borrow as a [`ChargerEntity`] if that is the active kind.
    pub fn as_charger(&self) -> Option<&ChargerEntity> {
        match self {
            InteractiveEntity::Charger(c) => Some(c),
        }
    }

    /// Mutably borrow as a [`ChargerEntity`] if that is the active kind.
    pub fn as_charger_mut(&mut self) -> Option<&mut ChargerEntity> {
        match self {
            InteractiveEntity::Charger(c) => Some(c),
        }
    }
}

impl InteractiveEntityBehavior for InteractiveEntity {
    fn entity_type(&self) -> EntityType {
        match self {
            InteractiveEntity::Charger(c) => c.entity_type(),
        }
    }

    fn coordinates(&self) -> EntityCoordinates {
        match self {
            InteractiveEntity::Charger(c) => c.coordinates(),
        }
    }

    fn props(&self) -> HashMap<String, String> {
        match self {
            InteractiveEntity::Charger(c) => c.props(),
        }
    }

    fn is_used(&self) -> bool {
        match self {
            InteractiveEntity::Charger(c) => c.is_used(),
        }
    }

    fn set_used(&mut self, used: bool) {
        match self {
            InteractiveEntity::Charger(c) => c.set_used(used),
        }
    }

    fn change_prop(&mut self, key: &str, value: &str) {
        match self {
            InteractiveEntity::Charger(c) => c.change_prop(key, value),
        }
    }

    fn get_prop(&self, key: &str) -> Option<String> {
        match self {
            InteractiveEntity::Charger(c) => c.get_prop(key),
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

/// Sparse map from a hypertile to the interactive entities standing on it.
///
/// Stored as a `HashMap` (not a dense [`Hypermap`](crate::map::hypermap::Hypermap))
/// because entities are rare relative to the 128×128×10 cells per chunk — a dense
/// store would allocate a `Vec` per cell. Inserted as a Bevy `Resource` by
/// [`InteractiveEntityPlugin`].
///
/// Serializes as a flat list of [`InteractiveEntityEntry`] (each carries its own
/// coordinates), which both keeps the JSON compact and sidesteps `serde_json`'s
/// "map keys must be strings" limitation on the struct key.
#[derive(Debug, Default, Resource)]
pub struct InteractiveEntityMap {
    tiles: HashMap<EntityCoordinates, InteractiveEntityHypertileList>,
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
    fn map_round_trips_through_json() {
        let mut map = InteractiveEntityMap::new();
        let mut charger = ChargerEntity::new(EntityCoordinates::ground(2, 3), ChargerFacing::South);
        charger.set_charge_level(42.0);
        charger.change_prop("label", "dock-7");
        map.insert(InteractiveEntity::Charger(charger));

        let json = serde_json::to_string(&map).expect("serialize");
        let restored: InteractiveEntityMap = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.len(), 1);
        let entry = &restored.entities_at(EntityCoordinates::ground(2, 3))[0];
        assert_eq!(entry.entity_type, EntityType::Charger);
        let charger = entry.entity.as_charger().expect("charger");
        assert_eq!(charger.charge_level(), 42.0);
        assert_eq!(charger.facing(), ChargerFacing::South);
        assert_eq!(charger.get_prop("label").as_deref(), Some("dock-7"));
        assert_eq!(charger.occupant(), None, "runtime occupant not persisted");
    }
}

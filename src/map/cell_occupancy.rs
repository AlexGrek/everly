//! Per-cell actor occupancy: which entities currently stand on each world tile.
//!
//! [`CellOccupancy`] stores, for **every** hypermap cell that holds at least one
//! actor, the full list of entities whose [main tile](crate::actor::actor_main_tile)
//! is that cell. It is the reverse of "where is this bot?" — given a tile, get
//! everyone on it — and is the lookup a collision (or any tile-scoped query) needs
//! to turn a blocked subtile back into the *entities* responsible for it.
//!
//! Updated by [`track_cell_occupancy`] once per frame: each actor's current main
//! tile is compared against its last recorded cell, and only a genuine cell change
//! mutates the map (insert / move). Despawned actors are dropped via
//! [`RemovedComponents`]. The map is **sparse** — a cell with no actors holds no
//! entry — and allocation-free in steady state (no bot crossed a cell boundary ⇒
//! pure hash lookups, no list growth).

use bevy::platform::collections::HashMap;
use bevy::prelude::*;

use crate::actor::{actor_main_tile, ActorObject};
use crate::menu::main_menu::GameState;

/// Live map of `world tile -> entities whose main tile is that tile`, plus a
/// reverse `entity -> its cell` index for O(1) change detection and removal.
///
/// Read it with [`entities_in`](Self::entities_in) (everyone on a tile) and
/// [`cell_of`](Self::cell_of) (a specific actor's current tile). The map is
/// maintained by [`track_cell_occupancy`]; consumers only read it.
#[derive(Resource, Default)]
pub struct CellOccupancy {
    /// Tile → the entities currently standing on it. Empty cells hold no entry.
    cells: HashMap<IVec2, Vec<Entity>>,
    /// Entity → the cell it is recorded in. Source of truth for change detection.
    entity_cell: HashMap<Entity, IVec2>,
}

impl CellOccupancy {
    /// All entities whose main tile is `tile`. Empty slice when the cell is empty.
    #[inline]
    pub fn entities_in(&self, tile: IVec2) -> &[Entity] {
        match self.cells.get(&tile) {
            Some(list) => list.as_slice(),
            None => &[],
        }
    }

    /// The cell `entity` is currently recorded in, if it is tracked.
    #[inline]
    pub fn cell_of(&self, entity: Entity) -> Option<IVec2> {
        self.entity_cell.get(&entity).copied()
    }

    /// Number of tracked actors (one per live actor).
    #[inline]
    pub fn tracked_len(&self) -> usize {
        self.entity_cell.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entity_cell.is_empty()
    }

    /// Records `entity` as occupying `tile`, moving it from any previous cell.
    /// Returns `true` when the cell actually changed (or the entity is new), so
    /// callers can react only to genuine transitions.
    pub fn set_cell(&mut self, entity: Entity, tile: IVec2) -> bool {
        match self.entity_cell.get(&entity).copied() {
            Some(prev) if prev == tile => false,
            Some(prev) => {
                self.detach(entity, prev);
                self.attach(entity, tile);
                self.entity_cell.insert(entity, tile);
                true
            }
            None => {
                self.attach(entity, tile);
                self.entity_cell.insert(entity, tile);
                true
            }
        }
    }

    /// Drops `entity` from the map entirely (despawn / no longer an actor).
    pub fn remove(&mut self, entity: Entity) {
        if let Some(prev) = self.entity_cell.remove(&entity) {
            self.detach(entity, prev);
        }
    }

    /// Adds `entity` to `tile`'s list (creating the cell entry if needed).
    fn attach(&mut self, entity: Entity, tile: IVec2) {
        self.cells.entry(tile).or_default().push(entity);
    }

    /// Removes `entity` from `tile`'s list, pruning the entry when it empties so
    /// the map stays sparse.
    fn detach(&mut self, entity: Entity, tile: IVec2) {
        if let Some(list) = self.cells.get_mut(&tile) {
            if let Some(i) = list.iter().position(|&e| e == entity) {
                list.swap_remove(i);
            }
            if list.is_empty() {
                self.cells.remove(&tile);
            }
        }
    }
}

/// Maintains [`CellOccupancy`] from actor positions: drops despawned actors, then
/// records each live actor's current main tile (mutating the map only on a real
/// cell change).
///
/// Runs in `Update`, which executes after the frame's `FixedUpdate` movement
/// ticks, so `center` reflects the completed movement step (including off-screen
/// [`advance_unchecked`](crate::actor::movement) travel).
fn track_cell_occupancy(
    mut occupancy: ResMut<CellOccupancy>,
    actors: Query<(Entity, &ActorObject)>,
    mut removed: RemovedComponents<ActorObject>,
) {
    for entity in removed.read() {
        occupancy.remove(entity);
    }
    for (entity, obj) in &actors {
        let tile = actor_main_tile(obj.inner.state().center);
        occupancy.set_cell(entity, tile);
    }
}

/// Registers [`CellOccupancy`] and the system that keeps it in sync with actors.
pub struct CellOccupancyPlugin;

impl Plugin for CellOccupancyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CellOccupancy>().add_systems(
            Update,
            track_cell_occupancy.run_if(in_state(GameState::InGame)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: u64) -> Entity {
        Entity::from_bits(id)
    }

    #[test]
    fn insert_then_query_lists_entities() {
        let mut occ = CellOccupancy::default();
        assert!(occ.set_cell(e(1), IVec2::new(2, 3)), "new entity is a change");
        assert!(occ.set_cell(e(2), IVec2::new(2, 3)), "second entity on same tile");
        let here = occ.entities_in(IVec2::new(2, 3));
        assert_eq!(here.len(), 2);
        assert!(here.contains(&e(1)) && here.contains(&e(2)));
        assert_eq!(occ.cell_of(e(1)), Some(IVec2::new(2, 3)));
        assert_eq!(occ.tracked_len(), 2);
    }

    #[test]
    fn same_cell_is_not_a_change() {
        let mut occ = CellOccupancy::default();
        assert!(occ.set_cell(e(1), IVec2::new(0, 0)));
        assert!(!occ.set_cell(e(1), IVec2::new(0, 0)), "no transition, no change");
        assert_eq!(occ.entities_in(IVec2::new(0, 0)), &[e(1)]);
    }

    #[test]
    fn move_relocates_and_prunes_empty_cell() {
        let mut occ = CellOccupancy::default();
        occ.set_cell(e(1), IVec2::new(0, 0));
        assert!(occ.set_cell(e(1), IVec2::new(5, 5)), "crossing tiles is a change");
        assert!(occ.entities_in(IVec2::new(0, 0)).is_empty(), "old cell emptied");
        assert_eq!(occ.entities_in(IVec2::new(5, 5)), &[e(1)]);
        assert_eq!(occ.cell_of(e(1)), Some(IVec2::new(5, 5)));
        // The vacated cell holds no entry (sparse).
        assert!(!occ.cells.contains_key(&IVec2::new(0, 0)));
    }

    #[test]
    fn move_off_shared_cell_keeps_others() {
        let mut occ = CellOccupancy::default();
        occ.set_cell(e(1), IVec2::new(1, 1));
        occ.set_cell(e(2), IVec2::new(1, 1));
        occ.set_cell(e(1), IVec2::new(2, 2));
        assert_eq!(occ.entities_in(IVec2::new(1, 1)), &[e(2)], "other occupant stays");
        assert_eq!(occ.entities_in(IVec2::new(2, 2)), &[e(1)]);
    }

    #[test]
    fn remove_drops_entity_and_prunes() {
        let mut occ = CellOccupancy::default();
        occ.set_cell(e(1), IVec2::new(3, 4));
        occ.remove(e(1));
        assert!(occ.entities_in(IVec2::new(3, 4)).is_empty());
        assert_eq!(occ.cell_of(e(1)), None);
        assert!(occ.is_empty());
        // Removing an untracked entity is a harmless no-op.
        occ.remove(e(99));
    }
}

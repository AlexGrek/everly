//! Per-cell actor occupancy: which entities currently stand on each world tile,
//! plus each tracked bot's kinematics (where it is, which way it's going, its
//! size).
//!
//! [`CellOccupancy`] stores, for **every** hypermap cell that holds at least one
//! actor, the full list of entities whose [main tile](crate::actor::actor_main_tile)
//! is that cell. It is the reverse of "where is this bot?" — given a tile, get
//! everyone on it — and is the lookup a collision (or any tile-scoped query) needs
//! to turn a blocked subtile back into the *entities* responsible for it
//! ([`resolve_blocker`](CellOccupancy::resolve_blocker)). For each entity it also
//! keeps a [`BotKinematics`] snapshot so a neighbour's motion can be reasoned about
//! without re-querying the ECS.
//!
//! Updated by [`track_cell_occupancy`] once per frame: each actor's current main
//! tile drives the (sparse) tile→entities map — only a genuine cell change mutates
//! it — while the kinematics value is refreshed in place every frame (no
//! allocation). Despawned actors are dropped via [`RemovedComponents`].

use bevy::platform::collections::HashMap;
use bevy::prelude::*;

use crate::actor::{actor_main_tile, ActorObject};
use crate::map::passability::SUBTILE_COUNT;
use crate::menu::main_menu::GameState;

/// Per-bot kinematics tracked alongside cell occupancy: enough to reason about a
/// neighbour's motion (where it is, which way it's going, how big it is) without
/// re-querying the ECS. Refreshed every frame by [`track_cell_occupancy`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BotKinematics {
    /// Main tile (the [`CellOccupancy`] key) — `actor_main_tile(center)`.
    pub tile: IVec2,
    /// Tile-space center.
    pub center: Vec2,
    /// Movement direction (unit, `Vec2::ZERO` = none); see
    /// [`ActorState::heading`](crate::actor::ActorState::heading).
    pub heading: Vec2,
    /// Footprint radius in subtiles.
    pub radius_subtiles: i32,
}

impl BotKinematics {
    /// `true` when this bot has a known movement direction.
    #[inline]
    pub fn is_moving(&self) -> bool {
        self.heading != Vec2::ZERO
    }
}

/// Live map of `world tile -> entities whose main tile is that tile`, plus a
/// per-entity [`BotKinematics`] index used for O(1) change detection / removal and
/// for resolving a blocked subtile back to the bot occupying it.
///
/// Read it with [`entities_in`](Self::entities_in) (everyone on a tile),
/// [`cell_of`](Self::cell_of) / [`kinematics_of`](Self::kinematics_of) (a specific
/// actor), and [`resolve_blocker`](Self::resolve_blocker) (subtile → owning bot).
/// The map is maintained by [`track_cell_occupancy`]; consumers only read it.
#[derive(Resource, Default)]
pub struct CellOccupancy {
    /// Tile → the entities currently standing on it. Empty cells hold no entry.
    cells: HashMap<IVec2, Vec<Entity>>,
    /// Entity → its kinematics (incl. the recorded cell). Source of truth for
    /// change detection and the motion read API.
    info: HashMap<Entity, BotKinematics>,
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
        self.info.get(&entity).map(|k| k.tile)
    }

    /// The full kinematics snapshot recorded for `entity`, if it is tracked.
    #[inline]
    pub fn kinematics_of(&self, entity: Entity) -> Option<BotKinematics> {
        self.info.get(&entity).copied()
    }

    /// Number of tracked actors (one per live actor).
    #[inline]
    pub fn tracked_len(&self) -> usize {
        self.info.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.info.is_empty()
    }

    /// Records `entity`'s current `kin`, moving it between cells when its main tile
    /// changed. Returns `true` when the **cell** actually changed (or the entity is
    /// new) — the kinematics value is always refreshed regardless.
    pub fn update(&mut self, entity: Entity, kin: BotKinematics) -> bool {
        match self.info.get(&entity).map(|k| k.tile) {
            Some(prev) if prev == kin.tile => {
                self.info.insert(entity, kin);
                false
            }
            Some(prev) => {
                self.detach(entity, prev);
                self.attach(entity, kin.tile);
                self.info.insert(entity, kin);
                true
            }
            None => {
                self.attach(entity, kin.tile);
                self.info.insert(entity, kin);
                true
            }
        }
    }

    /// Drops `entity` from the map entirely (despawn / no longer an actor).
    pub fn remove(&mut self, entity: Entity) {
        if let Some(kin) = self.info.remove(&entity) {
            self.detach(entity, kin.tile);
        }
    }

    /// The bot most likely occupying `world_subtile` — e.g. a collision's blocked
    /// subtile — excluding `exclude` (the querying bot). A bot's footprint can
    /// spill one tile past its main cell, so this scans the subtile's tile plus its
    /// eight neighbours and returns the **nearest-centered** candidate whose body
    /// can plausibly reach the probed subtile (`dist ≤ (radius + 1) subtiles`).
    /// `None` when nothing qualifies (no avoidance data, or only `exclude` nearby).
    /// Returns the owning entity together with its kinematics.
    ///
    /// Allocation-free: a fixed 3×3 tile scan over the sparse `cells` lists, picking
    /// the minimum-distance candidate as it goes. Ties (astronomically rare with
    /// float centers) resolve by iteration order and are not semantically meaningful.
    pub fn resolve_blocker(
        &self,
        world_subtile: IVec2,
        exclude: Entity,
    ) -> Option<(Entity, BotKinematics)> {
        let sc = SUBTILE_COUNT as i32;
        let tile = IVec2::new(world_subtile.x.div_euclid(sc), world_subtile.y.div_euclid(sc));
        // Probed subtile center in tile-space.
        let probe = (world_subtile.as_vec2() + Vec2::splat(0.5)) / sc as f32;
        let mut best: Option<(Entity, BotKinematics, f32)> = None;
        for dy in -1..=1 {
            for dx in -1..=1 {
                for &e in self.entities_in(tile + IVec2::new(dx, dy)) {
                    if e == exclude {
                        continue;
                    }
                    let Some(kin) = self.info.get(&e) else { continue };
                    // Reject candidates whose body cannot reach the probed subtile:
                    // footprint radius + 1 subtile of slack, expressed in tiles.
                    let reach = (kin.radius_subtiles as f32 + 1.0) / sc as f32;
                    let d2 = (kin.center - probe).length_squared();
                    if d2 > reach * reach {
                        continue;
                    }
                    if best.map_or(true, |(_, _, bd)| d2 < bd) {
                        best = Some((e, *kin, d2));
                    }
                }
            }
        }
        best.map(|(e, k, _)| (e, k))
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
/// records each live actor's current main tile + kinematics (mutating the tile map
/// only on a real cell change; refreshing kinematics in place every frame).
///
/// Runs in `Update`, which executes after the frame's `FixedUpdate` movement ticks,
/// so `center` reflects the completed step and `heading` reflects the value the
/// brain just published (including off-screen
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
        let s = obj.inner.state();
        occupancy.update(
            entity,
            BotKinematics {
                tile: actor_main_tile(s.center),
                center: s.center,
                heading: s.heading,
                radius_subtiles: s.radius_subtiles,
            },
        );
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

    /// A bot at tile-space `center` (radius 2 subtiles, no heading), keyed by its
    /// floored main tile.
    fn kin(center: Vec2) -> BotKinematics {
        BotKinematics {
            tile: actor_main_tile(center),
            center,
            heading: Vec2::ZERO,
            radius_subtiles: 2,
        }
    }

    #[test]
    fn insert_then_query_lists_entities() {
        let mut occ = CellOccupancy::default();
        assert!(occ.update(e(1), kin(Vec2::new(2.5, 3.5))), "new entity is a change");
        assert!(occ.update(e(2), kin(Vec2::new(2.2, 3.2))), "second entity on same tile");
        let here = occ.entities_in(IVec2::new(2, 3));
        assert_eq!(here.len(), 2);
        assert!(here.contains(&e(1)) && here.contains(&e(2)));
        assert_eq!(occ.cell_of(e(1)), Some(IVec2::new(2, 3)));
        assert_eq!(occ.tracked_len(), 2);
    }

    #[test]
    fn same_cell_refreshes_kinematics_without_a_cell_change() {
        let mut occ = CellOccupancy::default();
        assert!(occ.update(e(1), kin(Vec2::new(0.5, 0.5))));
        // Same tile, moved within it + gained a heading: not a cell change, but the
        // kinematics snapshot is updated.
        let mut k = kin(Vec2::new(0.9, 0.5));
        k.heading = Vec2::X;
        assert!(!occ.update(e(1), k), "no tile transition, no cell change");
        assert_eq!(occ.entities_in(IVec2::new(0, 0)), &[e(1)]);
        let got = occ.kinematics_of(e(1)).unwrap();
        assert_eq!(got.heading, Vec2::X);
        assert_eq!(got.center, Vec2::new(0.9, 0.5));
    }

    #[test]
    fn move_relocates_and_prunes_empty_cell() {
        let mut occ = CellOccupancy::default();
        occ.update(e(1), kin(Vec2::new(0.5, 0.5)));
        assert!(occ.update(e(1), kin(Vec2::new(5.5, 5.5))), "crossing tiles is a change");
        assert!(occ.entities_in(IVec2::new(0, 0)).is_empty(), "old cell emptied");
        assert_eq!(occ.entities_in(IVec2::new(5, 5)), &[e(1)]);
        assert_eq!(occ.cell_of(e(1)), Some(IVec2::new(5, 5)));
        assert!(!occ.cells.contains_key(&IVec2::new(0, 0)));
    }

    #[test]
    fn move_off_shared_cell_keeps_others() {
        let mut occ = CellOccupancy::default();
        occ.update(e(1), kin(Vec2::new(1.5, 1.5)));
        occ.update(e(2), kin(Vec2::new(1.2, 1.2)));
        occ.update(e(1), kin(Vec2::new(2.5, 2.5)));
        assert_eq!(occ.entities_in(IVec2::new(1, 1)), &[e(2)], "other occupant stays");
        assert_eq!(occ.entities_in(IVec2::new(2, 2)), &[e(1)]);
    }

    #[test]
    fn remove_drops_entity_and_prunes() {
        let mut occ = CellOccupancy::default();
        occ.update(e(1), kin(Vec2::new(3.5, 4.5)));
        occ.remove(e(1));
        assert!(occ.entities_in(IVec2::new(3, 4)).is_empty());
        assert_eq!(occ.cell_of(e(1)), None);
        assert!(occ.is_empty());
        occ.remove(e(99)); // harmless no-op
    }

    #[test]
    fn resolve_blocker_picks_nearest_and_excludes_self() {
        let mut occ = CellOccupancy::default();
        // Two bots near tile (5,5); the blocker subtile sits at the center of (5,5).
        let sc = SUBTILE_COUNT as i32;
        let blocker = IVec2::new(5 * sc + sc / 2, 5 * sc + sc / 2); // ≈ (5.5, 5.5)
        occ.update(e(1), kin(Vec2::new(5.5, 5.5))); // right on it
        occ.update(e(2), kin(Vec2::new(5.5, 6.4))); // a tile up, farther
        let (who, k) = occ.resolve_blocker(blocker, e(99)).unwrap();
        // Nearest is e(1).
        assert_eq!(who, e(1));
        assert_eq!(k.center, Vec2::new(5.5, 5.5));
        // Excluding the nearest never returns it again.
        assert!(occ.resolve_blocker(blocker, e(1)).map_or(true, |(w, _)| w != e(1)));
    }

    #[test]
    fn resolve_blocker_rejects_out_of_reach_and_handles_empty() {
        let mut occ = CellOccupancy::default();
        let sc = SUBTILE_COUNT as i32;
        let blocker = IVec2::new(5 * sc + sc / 2, 5 * sc + sc / 2);
        // Bot two tiles away: outside (radius+1) reach of the probed subtile.
        occ.update(e(1), kin(Vec2::new(7.5, 7.5)));
        assert!(occ.resolve_blocker(blocker, e(99)).is_none(), "too far to own the cell");
        assert!(CellOccupancy::default().resolve_blocker(blocker, e(99)).is_none());
    }

    #[test]
    fn resolve_blocker_finds_owner_in_neighbour_tile() {
        // A bot whose center is in tile (4,5) but whose footprint spills into a
        // blocker subtile that lives in tile (5,5).
        let mut occ = CellOccupancy::default();
        let sc = SUBTILE_COUNT as i32;
        // Bot center near the (4,5)/(5,5) boundary.
        occ.update(e(1), kin(Vec2::new(4.95, 5.5)));
        // Blocker subtile = first column of tile (5,5), same row band.
        let blocker = IVec2::new(5 * sc, 5 * sc + sc / 2);
        let got = occ.resolve_blocker(blocker, e(99));
        assert_eq!(got.map(|(_, k)| k.center), Some(Vec2::new(4.95, 5.5)));
    }
}

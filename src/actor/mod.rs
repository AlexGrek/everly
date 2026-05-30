//! Actor runtime: trait-based low-level logic and movement over the unified
//! subtile passability map.
//!
//! This module defines the generic [`Actor`] trait and the Bevy systems that
//! drive all registered actor entities each frame:
//!
//! 1. [`flush_actor_occupancy`] — flush write→read, clear write buffer.
//! 2. [`stamp_static_passability`] — stamp wall/void geometry into write.
//! 3. Actor think systems (e.g. `glitch_bot_think`) fill `move_buffer`.
//! 4. [`process_actors`] — for each actor: clear error, think, prepare, try_move.
//!
//! ## Coordinates
//! - Actor center is stored as float tile coordinates (`Vec2`).
//! - Movement deltas: float `tile_delta` for rendering, integer `subtile_shift`
//!   for the collision grid.
//! - `1 tile = SUBTILE_COUNT (5) subtiles`.
//!
//! ## Collision model
//! Each actor declares its [`Actor::blocked_flags`] — a bitmask of
//! [`FLAG_*`](crate::map::passability::FLAG_BLOCKED) values it considers
//! impassable. The unified passability map encodes both static geometry and
//! creature bodies as flag bits, so a single read determines passability for
//! any actor class.

pub mod black_bot;
pub mod actor_name;
pub mod actor_pick;
pub mod brain;
pub mod charge;
pub mod glitch_bot;
pub mod inspect;
pub mod snapshot;

use bevy::prelude::*;

use crate::map::hypermap::Hypermap;
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::passability::{
    DynamicPassabilityMap, SubtilePassability, TryUpdateFootprintError,
    FLAG_BLOCKED, FLAG_VOID, SUBTILE_COUNT,
};
#[cfg(test)]
use crate::map::passability::FLAG_CREATURE;
use crate::menu::main_menu::GameState;

// ---------------------------------------------------------------------------
// Paused resource
// ---------------------------------------------------------------------------

/// When `true`, all actor movement systems are suspended.
/// Toggle with the `Space` key while in-game.
#[derive(Resource, Default, PartialEq)]
pub struct Paused(pub bool);

/// Condition: returns `true` when the simulation is paused.
pub fn is_paused(paused: Res<Paused>) -> bool {
    paused.0
}

// ---------------------------------------------------------------------------
// ActorMoveBuffer
// ---------------------------------------------------------------------------

/// Mutable movement intent for the current frame.
///
/// Two displacement channels:
/// - [`tile_delta`](Self::tile_delta) — exact float displacement in tile-space
///   applied to `center` every frame for smooth rendering.
/// - [`subtile_shift`](Self::subtile_shift) — integer subtile steps used only
///   for the passability/collision grid; typically zero on most frames and
///   non-zero only when the accumulated float motion crosses a subtile edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActorMoveBuffer {
    /// Exact float center displacement in **tile-space** for this frame.
    /// Applied to `ActorState::center` unconditionally on accepted movement.
    pub tile_delta: Vec2,
    /// Integer subtile steps for the passability grid collision check.
    pub subtile_shift: IVec2,
    /// Relative rotation shift for this frame.
    pub rotation_shift: f32,
}

impl Default for ActorMoveBuffer {
    fn default() -> Self {
        Self {
            tile_delta: Vec2::ZERO,
            subtile_shift: IVec2::ZERO,
            rotation_shift: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// ActorMovementError
// ---------------------------------------------------------------------------

/// Last movement failure reason produced by [`Actor::try_move`].
#[derive(Debug, Clone, PartialEq)]
pub enum ActorMovementError {
    /// Candidate footprint intersects a dynamic obstacle (another actor's
    /// footprint — `FLAG_BLOCKED | FLAG_CREATURE` both set).
    BlockedByOccupancy { world_subtile_x: i32, world_subtile_y: i32 },
    /// Candidate footprint intersects a static obstacle (wall or void for
    /// ground walkers — `FLAG_BLOCKED` or `FLAG_VOID` without `FLAG_CREATURE`).
    BlockedByStatic { world_subtile_x: i32, world_subtile_y: i32 },
    /// Radius must be non-negative.
    InvalidRadius(i32),
}

// ---------------------------------------------------------------------------
// ActorState
// ---------------------------------------------------------------------------

/// Common mutable actor data handled by low-level movement.
///
/// Grid position is tracked in `last_accepted_center_subtile` **independently**
/// of `center` (float). The float center drifts between subtile boundaries via
/// `tile_delta` for smooth rendering; recomputing the grid position from the
/// float would eventually round into a wall and permanently deadlock the actor.
#[derive(Debug, Clone)]
pub struct ActorState {
    /// Center in tile-space (`x` and `y` in world tiles, float precision).
    pub center: Vec2,
    /// Radius of the circular occupancy shadow, measured in subtiles.
    pub radius_subtiles: i32,
    /// Actor yaw/rotation in radians.
    pub rotation: f32,
    /// Per-frame movement intent written by [`Actor::prepare_movement`].
    pub move_buffer: ActorMoveBuffer,
    /// Cleared at the beginning of each low-level processing step.
    pub last_movement_error: Option<ActorMovementError>,
    /// Integer subtile center of the most recent **accepted** occupancy
    /// update. `None` for brand-new actors that have never written a footprint.
    /// Used to compute self-overlap on the next frame without a `Vec<IVec2>`.
    pub last_accepted_center_subtile: Option<IVec2>,
    /// Radius of the most recent accepted footprint in subtiles. Tracked
    /// separately so an actor that resizes between frames still re-stamps the
    /// correct old circle on rejected moves.
    pub last_accepted_radius_subtiles: i32,
    /// Destination hint for off-screen→on-screen collision resolution.
    /// Set each frame by the actor's think system (e.g. the current path
    /// waypoint). When the actor re-enters a rendered chunk and its footprint
    /// overlaps static geometry, [`resolve_offscreen_collision`] tries this
    /// tile before falling back to a ring search.
    pub next_waypoint_hint: Option<Vec2>,
    /// Last committed main world tile for [`crate::map::field_interactions`].
    /// Updated after movement each frame; not serialized in actor snapshots.
    pub field_main_tile: Option<IVec2>,
    /// How dirty the actor is, `0.0` (clean) ..= `1.0` (filthy). Exchanged with
    /// the floor [`crate::map::dirt::DirtMap`] on each main-tile transition (see
    /// [`crate::map::field_interactions`]). Actors spawn clean and this is not
    /// serialized in snapshots — a loaded actor starts clean again.
    pub dirtiness: f32,
}

/// Nearest world tile to a tile-space [`Vec2`] center (`round`, not `floor`).
///
/// `ActorState::center` lives in **tile units** (1 unit = 1 m world tile). Field
/// interactions and “which tile am I in?” semantics use this; passability still
/// floors to subtiles via [`ActorState::center_subtile_i32`].
#[inline]
pub fn actor_main_tile(center: Vec2) -> IVec2 {
    IVec2::new(center.x.floor() as i32, center.y.floor() as i32)
}

impl ActorState {
    /// Tile-space center converted to integer tile coordinates (floor).
    ///
    /// For field deposits / main-tile tracking, prefer [`actor_main_tile`].
    #[inline]
    pub fn center_tile_i32(&self) -> IVec2 {
        IVec2::new(self.center.x.floor() as i32, self.center.y.floor() as i32)
    }

    /// Nearest world tile to [`Self::center`] — same as [`actor_main_tile`].
    #[inline]
    pub fn main_tile_i32(&self) -> IVec2 {
        actor_main_tile(self.center)
    }

    /// Center converted to integer subtile coordinates by flooring.
    /// Used as a fallback for the first frame only (before any footprint is
    /// accepted). All subsequent grid positions derive from
    /// `last_accepted_center_subtile + subtile_shift`.
    ///
    /// Floor (not round) gives the subtile that *contains* the position.
    /// Round would shift actors at half-integer tile positions (e.g. tile
    /// centres spawned at 0.5) into the wrong subtile.
    #[inline]
    pub fn center_subtile_i32(&self) -> IVec2 {
        let scale = SUBTILE_COUNT as f32;
        IVec2::new(
            (self.center.x * scale).floor() as i32,
            (self.center.y * scale).floor() as i32,
        )
    }
}

// ---------------------------------------------------------------------------
// Actor trait
// ---------------------------------------------------------------------------

/// Trait implemented by all actor logic objects.
///
/// The trait separates high-level asynchronous planning from deterministic
/// per-frame low-level logic.
pub trait Actor: Send + Sync + 'static {
    fn state(&self) -> &ActorState;
    fn state_mut(&mut self) -> &mut ActorState;

    /// Low-level deterministic thinking step called every frame before
    /// movement preparation.
    fn think_low_level(&mut self) {}

    /// Fill or adjust [`ActorState::move_buffer`] for this frame.
    /// Called after [`think_low_level`](Self::think_low_level).
    fn prepare_movement(&mut self) {}

    /// Passability flags that block this actor.
    ///
    /// The unified passability map stores static geometry and creature bodies
    /// as `u64` flag bitmasks. An actor is blocked when any bit in
    /// `blocked_flags()` is set in the candidate subtile's flags.
    ///
    /// | Actor class | `blocked_flags` | Description |
    /// |---|---|---|
    /// | Ground walker (default) | `FLAG_BLOCKED \| FLAG_VOID` | stopped by walls and void gaps |
    /// | Flyer | `FLAG_BLOCKED` | crosses void, stopped by walls |
    /// | Creature-aware only | `FLAG_BLOCKED \| FLAG_CREATURE` | only blocked by other units |
    ///
    /// Override this method to encode the actor's traversal rules.
    fn blocked_flags(&self) -> u64 {
        FLAG_BLOCKED | FLAG_VOID
    }

    /// Applies `move_buffer` to position and grid state **without** any
    /// collision check or footprint stamp.
    ///
    /// Used for off-screen actors: they travel freely through static geometry
    /// and do not participate in the dynamic occupancy map.
    fn advance_unchecked(&mut self) {
        let state = self.state_mut();
        let shift = state.move_buffer.subtile_shift;
        // Capture the grid position BEFORE center is updated, matching the
        // same ordering as try_move (which reads last_accepted_center_subtile
        // before touching center).
        let grid = state.last_accepted_center_subtile.unwrap_or_else(|| {
            let sc = SUBTILE_COUNT as f32;
            IVec2::new(
                (state.center.x * sc).floor() as i32,
                (state.center.y * sc).floor() as i32,
            )
        });
        state.center += state.move_buffer.tile_delta;
        state.rotation += state.move_buffer.rotation_shift;
        if shift != IVec2::ZERO {
            state.last_accepted_center_subtile = Some(grid + shift);
        }
        state.move_buffer = ActorMoveBuffer::default();
    }

    /// Applies the accepted movement transform.
    ///
    /// - `center` is advanced by the exact float `tile_delta` (smooth, never
    ///   quantized).
    /// - `rotation` is advanced by `rotation_shift`.
    /// - `last_accepted_*` are updated by [`try_move`](Self::try_move) before
    ///   this runs.
    fn move_actor(&mut self) -> Result<(), ActorMovementError> {
        let state = self.state_mut();
        if state.radius_subtiles < 0 {
            return Err(ActorMovementError::InvalidRadius(state.radius_subtiles));
        }
        state.center += state.move_buffer.tile_delta;
        state.rotation += state.move_buffer.rotation_shift;
        state.move_buffer = ActorMoveBuffer::default();
        Ok(())
    }

    /// Attempts to move; on collision records an error and keeps position.
    ///
    /// Static geometry is read from the persistent `static_cache` (updated
    /// only on map edits). Dynamic creature footprints are read from the
    /// passability **read** buffer. The actor's `blocked_flags()` determine
    /// which flag bits are impassable.
    ///
    /// Self-overlap (the actor's previous footprint) is always bypassed —
    /// the actor never collides with itself.
    ///
    /// Grid position invariant: the next candidate position is
    /// `last_accepted_center_subtile + subtile_shift`. The float `center` is
    /// never consulted for grid position — it drifts for smooth rendering
    /// only.
    fn try_move(
        &mut self,
        dynamic_passability: &DynamicPassabilityMap,
        static_cache: &Hypermap<SubtilePassability>,
    ) {
        let (radius, grid_pos, requested_shift, previous, actor_blocked) = {
            let s = self.state();
            let previous = s
                .last_accepted_center_subtile
                .map(|c| (c, s.last_accepted_radius_subtiles));
            let grid_pos = s
                .last_accepted_center_subtile
                .unwrap_or_else(|| s.center_subtile_i32());
            (
                s.radius_subtiles,
                grid_pos,
                s.move_buffer.subtile_shift,
                previous,
                self.blocked_flags(),
            )
        };
        let next_center_sub = grid_pos + requested_shift;

        let result = dynamic_passability.try_update_footprint(
            next_center_sub,
            radius,
            previous,
            actor_blocked,
            static_cache,
        );

        match result {
            Ok(()) => {
                {
                    let state = self.state_mut();
                    state.last_accepted_center_subtile = Some(next_center_sub);
                    state.last_accepted_radius_subtiles = radius;
                }
                if let Err(err) = self.move_actor() {
                    self.state_mut().last_movement_error = Some(err);
                }
            }
            Err(err) => {
                let state = self.state_mut();
                state.last_movement_error = Some(match err {
                    TryUpdateFootprintError::InvalidRadius(r) => {
                        ActorMovementError::InvalidRadius(r)
                    }
                    TryUpdateFootprintError::BlockedByOccupancy { world_subtile } => {
                        ActorMovementError::BlockedByOccupancy {
                            world_subtile_x: world_subtile.x,
                            world_subtile_y: world_subtile.y,
                        }
                    }
                    TryUpdateFootprintError::BlockedByStatic { world_subtile } => {
                        ActorMovementError::BlockedByStatic {
                            world_subtile_x: world_subtile.x,
                            world_subtile_y: world_subtile.y,
                        }
                    }
                });
                state.move_buffer = ActorMoveBuffer::default();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ECS wrapper
// ---------------------------------------------------------------------------

/// Marker on actor roots spawned from `levels/level_{name}/actors.yaml`.
#[derive(Component)]
pub struct LevelActor;

/// Marker attached to actors whose containing chunk is not currently rendered.
///
/// While present, the actor moves without collision detection and its mesh
/// transform is not updated. Removed by [`process_actors`] when the chunk
/// becomes rendered; at that point [`resolve_offscreen_collision`] places the
/// actor at a valid passable position.
#[derive(Component)]
pub struct OffScreenActor;

/// ECS wrapper for heterogeneous actor trait objects.
#[derive(Component)]
pub struct ActorObject {
    pub inner: Box<dyn Actor>,
}

impl ActorObject {
    pub fn new(actor: Box<dyn Actor>) -> Self {
        Self { inner: actor }
    }
}

// ---------------------------------------------------------------------------
// Plugin + systems
// ---------------------------------------------------------------------------

/// Plugin that runs low-level actor processing every in-game frame.
pub struct ActorPlugin;

impl Plugin for ActorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Paused>().add_systems(
            Update,
            (
                toggle_pause.run_if(in_state(GameState::InGame)),
                (flush_actor_occupancy, process_actors)
                    .chain()
                    .run_if(in_state(GameState::InGame))
                    .run_if(not(is_paused)),
            ),
        );
    }
}

fn toggle_pause(keys: Res<ButtonInput<KeyCode>>, mut paused: ResMut<Paused>) {
    if keys.just_pressed(KeyCode::Space) {
        paused.0 = !paused.0;
    }
}

/// Flush write→read; clear write buffer. Must be first in the actor pipeline.
fn flush_actor_occupancy(passability: Res<DynamicPassabilityMap>) {
    passability.flush();
}

/// Teleports an actor to a valid passable position after it re-enters a
/// rendered chunk from off-screen travel.
///
/// Tries, in order:
/// 1. Current subtile position (may be free if static geometry hasn't changed).
/// 2. [`ActorState::next_waypoint_hint`] (set by the actor's think system).
/// 3. Centers of tiles in an expanding ring of radius 1–5 around the actor.
///
/// If nothing is passable the actor is left in place without stamping a
/// footprint; it will retry next frame.
fn resolve_offscreen_collision(
    actor: &mut dyn Actor,
    dynamic: &DynamicPassabilityMap,
    static_cache: &Hypermap<SubtilePassability>,
) {
    let radius = actor.state().radius_subtiles;
    let blocked = actor.blocked_flags();
    let current_sub = actor
        .state()
        .last_accepted_center_subtile
        .unwrap_or_else(|| actor.state().center_subtile_i32());

    // 1. Try current position (previous=None — off-screen footprint was not stamped).
    if dynamic
        .try_update_footprint(current_sub, radius, None, blocked, static_cache)
        .is_ok()
    {
        let s = actor.state_mut();
        s.last_accepted_center_subtile = Some(current_sub);
        s.last_accepted_radius_subtiles = radius;
        return;
    }

    // 2. Try waypoint hint supplied by the think system.
    let waypoint_hint = actor.state().next_waypoint_hint;
    if let Some(wp) = waypoint_hint {
        let sc = SUBTILE_COUNT as f32;
        let candidate = IVec2::new((wp.x * sc).floor() as i32, (wp.y * sc).floor() as i32);
        if dynamic
            .try_update_footprint(candidate, radius, None, blocked, static_cache)
            .is_ok()
        {
            let s = actor.state_mut();
            s.center = wp;
            s.last_accepted_center_subtile = Some(candidate);
            s.last_accepted_radius_subtiles = radius;
            return;
        }
    }

    // 3. Search expanding tile rings (r = 1..=5), testing tile centers.
    let current_tile = {
        let c = actor.state().center;
        IVec2::new(c.x.floor() as i32, c.y.floor() as i32)
    };
    let sc_i = SUBTILE_COUNT as i32;
    'outer: for r in 1i32..=5 {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue; // interior — covered by a smaller ring
                }
                let tile = current_tile + IVec2::new(dx, dy);
                let candidate = IVec2::new(tile.x * sc_i + sc_i / 2, tile.y * sc_i + sc_i / 2);
                if dynamic
                    .try_update_footprint(candidate, radius, None, blocked, static_cache)
                    .is_ok()
                {
                    let tile_center = Vec2::new(tile.x as f32 + 0.5, tile.y as f32 + 0.5);
                    let s = actor.state_mut();
                    s.center = tile_center;
                    s.last_accepted_center_subtile = Some(candidate);
                    s.last_accepted_radius_subtiles = radius;
                    break 'outer;
                }
            }
        }
    }
    // If nothing found, leave position unchanged; footprint not stamped.
}

/// Drives the complete low-level actor pipeline for all registered actors.
///
/// On-screen actors (chunk in `chunk_roots`) use full collision detection via
/// [`Actor::try_move`]. Off-screen actors advance without collision via
/// [`Actor::advance_unchecked`] and carry an [`OffScreenActor`] marker.
/// On the frame a chunk becomes rendered, `resolve_offscreen_collision` places
/// the actor at the nearest valid passable position before normal movement resumes.
pub(crate) fn process_actors(
    mut actors: Query<(Entity, &mut ActorObject, Option<&OffScreenActor>)>,
    dynamic_passability: Res<DynamicPassabilityMap>,
    hypermap: Res<HypermapRuntime>,
    par_commands: ParallelCommands,
) {
    // Parallel-safe by construction: collision reads hit the **read** buffer,
    // which is immutable for the whole frame (flushed once up front), and writes
    // accumulate into the **write** buffer as commutative per-chunk `|=` ORs. The
    // result is therefore independent of actor processing order, so `par_iter_mut`
    // is deterministic. Each actor mutates only its own disjoint state; the only
    // shared writes are the order-independent occupancy ORs and deferred
    // `OffScreenActor` marker changes (one per distinct entity) via `ParallelCommands`.
    let static_cache = hypermap.static_subtile_cache.as_ref();
    let dynamic_passability = &*dynamic_passability;
    let hypermap = &*hypermap;

    actors
        .par_iter_mut()
        .for_each(|(entity, mut actor_obj, off_screen)| {
            let actor = actor_obj.inner.as_mut();
            actor.state_mut().last_movement_error = None;
            actor.think_low_level();
            actor.prepare_movement();

            let center = actor.state().center;
            let is_rendered =
                hypermap.is_world_pos_rendered(center.x.floor() as i32, center.y.floor() as i32);
            let was_off_screen = off_screen.is_some();

            if is_rendered {
                if was_off_screen {
                    // Transition: off-screen → on-screen. Place actor at valid position, then
                    // normal movement resumes next frame once the marker is removed.
                    par_commands.command_scope(|mut commands| {
                        commands.entity(entity).remove::<OffScreenActor>();
                    });
                    resolve_offscreen_collision(actor, dynamic_passability, static_cache);
                } else {
                    actor.try_move(dynamic_passability, static_cache);
                }
            } else {
                if !was_off_screen {
                    par_commands.command_scope(|mut commands| {
                        commands.entity(entity).insert(OffScreenActor);
                    });
                }
                actor.advance_unchecked();
            }
        });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::passability::{
        cell_subtile_flags, DynamicPassabilityMap, SubtilePassabilityMap, FLAG_BLOCKED, FLAG_VOID,
    };
    use crate::map::world_map::{CellType, WallMask};

    fn empty_static_cache() -> Hypermap<SubtilePassability> {
        Hypermap::new(SubtilePassability::EMPTY)
    }

    /// Builds a static cache with a single wall tile at world (wx, wy).
    fn static_cache_with_wall(wx: i32, wy: i32, mask: WallMask) -> Hypermap<SubtilePassability> {
        let cache = Hypermap::new(SubtilePassability::EMPTY);
        let cell = CellType::Wall(mask);
        let mut tile = SubtilePassability::EMPTY;
        for sy in 0..SUBTILE_COUNT {
            for sx in 0..SUBTILE_COUNT {
                let flags = cell_subtile_flags(cell, sx, sy);
                if flags != 0 {
                    tile.or_flags(sy, sx, flags);
                }
            }
        }
        cache.set(wx, wy, tile);
        cache
    }

    /// Builds a static cache with void at every tile.
    fn static_cache_all_void() -> Hypermap<SubtilePassability> {
        let mut tile = SubtilePassability::EMPTY;
        for sy in 0..SUBTILE_COUNT {
            for sx in 0..SUBTILE_COUNT {
                tile.or_flags(sy, sx, FLAG_VOID);
            }
        }
        Hypermap::new(tile)
    }

    struct DummyActor {
        state: ActorState,
    }

    impl DummyActor {
        fn new(center: Vec2, radius_subtiles: i32) -> Self {
            Self {
                state: ActorState {
                    center,
                    radius_subtiles,
                    rotation: 0.0,
                    move_buffer: ActorMoveBuffer::default(),
                    last_movement_error: None,
                    last_accepted_center_subtile: None,
                    last_accepted_radius_subtiles: radius_subtiles,
                    next_waypoint_hint: None,
                    field_main_tile: None,
                    dirtiness: 0.0,
                },
            }
        }
    }

    impl Actor for DummyActor {
        fn state(&self) -> &ActorState { &self.state }
        fn state_mut(&mut self) -> &mut ActorState { &mut self.state }
    }

    struct FlyingActor {
        state: ActorState,
    }

    impl Actor for FlyingActor {
        fn state(&self) -> &ActorState { &self.state }
        fn state_mut(&mut self) -> &mut ActorState { &mut self.state }
        fn blocked_flags(&self) -> u64 { FLAG_BLOCKED }
    }

    fn fresh_flying(center: Vec2, radius_subtiles: i32) -> FlyingActor {
        FlyingActor {
            state: ActorState {
                center,
                radius_subtiles,
                rotation: 0.0,
                move_buffer: ActorMoveBuffer::default(),
                last_movement_error: None,
                last_accepted_center_subtile: None,
                last_accepted_radius_subtiles: radius_subtiles,
                next_waypoint_hint: None,
                field_main_tile: None,
                dirtiness: 0.0,
            },
        }
    }

    #[test]
    fn try_move_updates_center_and_writes_shadow() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_static_cache();
        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 1);
        actor.state.move_buffer.tile_delta = Vec2::new(1.0, 0.0);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        actor.try_move(&map, &sc);
        map.flush();
        let view = SubtilePassabilityMap::new(&map);

        assert_eq!(actor.state.center, Vec2::new(11.0, 10.0));
        assert_eq!(
            actor.state.last_accepted_center_subtile,
            Some(IVec2::new(55, 50)),
        );
        assert_eq!(actor.state.last_accepted_radius_subtiles, 1);
        assert_ne!(view.flags_xy(0, 0, 55, 50) & FLAG_BLOCKED, 0);
        assert!(actor.state.last_movement_error.is_none());
    }

    #[test]
    fn try_move_reports_collision_and_keeps_center() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_static_cache();
        let view = SubtilePassabilityMap::new(&map);

        view.or_flags_xy(0, 0, 51, 50, FLAG_BLOCKED | FLAG_CREATURE);
        map.flush();

        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 0);
        actor.state.move_buffer.subtile_shift = IVec2::new(1, 0);
        actor.try_move(&map, &sc);

        assert_eq!(actor.state.center, Vec2::new(10.0, 10.0));
        assert!(matches!(
            actor.state.last_movement_error,
            Some(ActorMovementError::BlockedByOccupancy { .. })
        ));
    }

    #[test]
    fn try_move_default_actor_blocked_by_static_wall() {
        // Put a wall (all edges) at tile (11, 10) — subtile row 0 is blocked.
        let sc = static_cache_with_wall(11, 10, WallMask::from_bits(0x0F).unwrap());
        let map = DynamicPassabilityMap::new();

        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 0);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        actor.try_move(&map, &sc);

        assert_eq!(actor.state.center, Vec2::new(10.0, 10.0));
        assert!(matches!(
            actor.state.last_movement_error,
            Some(ActorMovementError::BlockedByStatic { .. })
        ));
    }

    #[test]
    fn try_move_flying_actor_crosses_void() {
        let sc = static_cache_all_void();
        let map = DynamicPassabilityMap::new();

        let mut actor = fresh_flying(Vec2::new(10.0, 10.0), 0);
        actor.state.move_buffer.tile_delta = Vec2::new(1.0, 0.0);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        actor.try_move(&map, &sc);

        assert_eq!(actor.state.center, Vec2::new(11.0, 10.0), "flying actor must cross void");
        assert!(actor.state.last_movement_error.is_none());
    }

    #[test]
    fn try_move_flying_actor_stops_at_wall() {
        let sc = static_cache_with_wall(11, 10, WallMask::from_bits(0x0F).unwrap());
        let map = DynamicPassabilityMap::new();

        let mut actor = fresh_flying(Vec2::new(10.0, 10.0), 0);
        actor.state.move_buffer.tile_delta = Vec2::new(1.0, 0.0);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        actor.try_move(&map, &sc);

        assert_eq!(actor.state.center, Vec2::new(10.0, 10.0), "flying actor must stop at wall");
        assert!(matches!(
            actor.state.last_movement_error,
            Some(ActorMovementError::BlockedByStatic { .. })
        ));
    }

    #[test]
    fn try_move_self_overlap_after_acceptance_allows_subsequent_step() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_static_cache();
        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 2);

        actor.state.move_buffer.tile_delta = Vec2::new(0.2, 0.0);
        actor.state.move_buffer.subtile_shift = IVec2::new(1, 0);
        actor.try_move(&map, &sc);
        assert!(actor.state.last_movement_error.is_none());
        map.flush();

        actor.state.move_buffer.tile_delta = Vec2::new(0.2, 0.0);
        actor.state.move_buffer.subtile_shift = IVec2::new(1, 0);
        actor.try_move(&map, &sc);
        assert!(actor.state.last_movement_error.is_none());
    }

    // --- Off-screen actor optimization ---

    #[test]
    fn advance_unchecked_moves_through_static_wall() {
        let sc = static_cache_with_wall(11, 10, WallMask::from_bits(0x0F).unwrap());
        let _map = DynamicPassabilityMap::new();
        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 0);
        actor.state.move_buffer.tile_delta = Vec2::new(1.0, 0.0);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        // advance_unchecked does not consult sc or map — wall is ignored.
        // Center (10,10) → initial subtile (50,50) + shift (5,0) = (55,50).
        actor.advance_unchecked();
        let _ = sc;
        assert_eq!(actor.state.center, Vec2::new(11.0, 10.0), "must pass through wall");
        assert!(actor.state.last_movement_error.is_none());
        assert_eq!(actor.state.last_accepted_center_subtile, Some(IVec2::new(55, 50)));
    }

    #[test]
    fn advance_unchecked_updates_grid_position() {
        let mut actor = DummyActor::new(Vec2::new(0.0, 0.0), 0);
        actor.state.last_accepted_center_subtile = Some(IVec2::new(10, 10));
        actor.state.move_buffer.tile_delta = Vec2::new(0.2, 0.4);
        actor.state.move_buffer.subtile_shift = IVec2::new(1, 2);
        actor.advance_unchecked();
        assert_eq!(actor.state.center, Vec2::new(0.2, 0.4));
        assert_eq!(actor.state.last_accepted_center_subtile, Some(IVec2::new(11, 12)));
        assert_eq!(actor.state.move_buffer, ActorMoveBuffer::default());
    }

    #[test]
    fn advance_unchecked_zero_shift_leaves_grid_unchanged() {
        let mut actor = DummyActor::new(Vec2::new(5.0, 5.0), 0);
        actor.state.last_accepted_center_subtile = Some(IVec2::new(25, 25));
        actor.state.move_buffer.tile_delta = Vec2::new(0.05, 0.0);
        actor.state.move_buffer.subtile_shift = IVec2::ZERO;
        actor.advance_unchecked();
        assert_eq!(actor.state.last_accepted_center_subtile, Some(IVec2::new(25, 25)));
    }

    #[test]
    fn resolve_offscreen_stays_at_clear_position() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_static_cache();
        let mut actor = DummyActor::new(Vec2::new(5.0, 5.0), 1);
        actor.state.last_accepted_center_subtile = Some(IVec2::new(25, 25));

        super::resolve_offscreen_collision(&mut actor, &map, &sc);
        map.flush();
        let view = SubtilePassabilityMap::new(&map);

        assert_eq!(actor.state.last_accepted_center_subtile, Some(IVec2::new(25, 25)),
            "clear position: must keep current subtile");
        assert_ne!(view.flags_xy(0, 0, 25, 25) & FLAG_BLOCKED, 0,
            "footprint must be stamped into write buffer");
    }

    #[test]
    fn resolve_offscreen_uses_waypoint_when_current_blocked() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_static_cache();

        // Block the current subtile with a creature footprint.
        map.write_footprint(&[IVec2::new(25, 25)]);
        map.flush();

        let mut actor = DummyActor::new(Vec2::new(5.0, 5.0), 0);
        actor.state.last_accepted_center_subtile = Some(IVec2::new(25, 25));
        actor.state.next_waypoint_hint = Some(Vec2::new(10.0, 5.0));

        super::resolve_offscreen_collision(&mut actor, &map, &sc);

        assert_eq!(actor.state.center, Vec2::new(10.0, 5.0),
            "must teleport to waypoint hint");
        assert_eq!(actor.state.last_accepted_center_subtile, Some(IVec2::new(50, 25)));
    }

    #[test]
    fn resolve_offscreen_searches_ring_when_no_waypoint() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_static_cache();

        // Block all subtiles of tile (0, 0) in the static cache.
        let mut blocked_tile = SubtilePassability::EMPTY;
        for sy in 0..SUBTILE_COUNT {
            for sx in 0..SUBTILE_COUNT {
                blocked_tile.or_flags(sy, sx, FLAG_BLOCKED);
            }
        }
        sc.set(0, 0, blocked_tile);

        let mut actor = DummyActor::new(Vec2::new(0.5, 0.5), 0);
        actor.state.last_accepted_center_subtile = Some(IVec2::new(2, 2));
        actor.state.next_waypoint_hint = None;

        super::resolve_offscreen_collision(&mut actor, &map, &sc);

        let final_sub = actor.state.last_accepted_center_subtile.unwrap();
        assert_ne!(final_sub, IVec2::new(2, 2),
            "must escape blocked tile via ring search");
    }

    #[test]
    fn resolve_offscreen_flyer_ignores_void() {
        let sc = static_cache_all_void();
        let map = DynamicPassabilityMap::new();

        let mut actor = fresh_flying(Vec2::new(5.0, 5.0), 0);
        actor.state.last_accepted_center_subtile = Some(IVec2::new(25, 25));

        super::resolve_offscreen_collision(&mut actor, &map, &sc);

        // Flyer only blocks on FLAG_BLOCKED; void alone must not prevent placement.
        assert_eq!(actor.state.last_accepted_center_subtile, Some(IVec2::new(25, 25)),
            "flyer must stay at void position");
    }
}

//! Actor runtime: trait-based low-level logic and movement over the unified
//! subtile passability map.
//!
//! This module defines the generic [`Actor`] trait and the Bevy systems that
//! drive all registered actor entities each frame:
//!
//! 1. [`flush_actor_occupancy`] — flush write→read, clear write buffer.
//! 2. [`stamp_static_passability`] — stamp wall/void geometry into write.
//! 3. Actor think systems (e.g. `black_bot_brain`) fill `move_buffer`.
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
pub mod dispatch;
pub mod inspect;
pub mod movement;
pub mod resurrect;
pub mod selection_overlay;
pub mod snapshot;

pub use movement::{ActorShadow, OccupancyArbiter};
pub(crate) use movement::process_actor_moves;

use bevy::prelude::*;

use crate::map::hypermap::Hypermap;
use crate::map::passability::{
    DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID, SUBTILE_COUNT,
};
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
    /// Footprint shadow + transient per-frame state for the arbitrated movement
    /// pipeline (see [`movement`]). Defaulted on construction; not serialized.
    pub shadow: ActorShadow,
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

/// Unit normal pointing from a blocking occupancy subtile toward `center`.
///
/// Used to compute elastic reflection when two actors collide. Returns
/// `Vec2::ZERO` when `center` is exactly on the blocker subtile center.
#[inline]
pub fn occupancy_collision_normal(center: Vec2, world_subtile_x: i32, world_subtile_y: i32) -> Vec2 {
    let sc = SUBTILE_COUNT as f32;
    let blocker_center = (Vec2::new(world_subtile_x as f32, world_subtile_y as f32) + Vec2::splat(0.5)) / sc;
    let delta = center - blocker_center;
    if delta.length_squared() <= 1e-12 {
        Vec2::ZERO
    } else {
        delta.normalize()
    }
}

/// Classifies a bot-on-bot occupancy collision relative to the bot's `heading`.
///
/// Returns `true` for a **head-on or side** contact (the bot is moving into the
/// blocker) and `false` for a **rear** bump (the blocker is behind the heading),
/// so callers can ignore bumps that come from behind. Degenerate inputs — the
/// bot is effectively stationary, or sits exactly on the blocker subtile — are
/// treated as front so a wedged bot never silently freezes.
///
/// [`occupancy_collision_normal`] points from the blocker toward the bot, so
/// moving *into* the blocker means `heading` opposes the normal (`dot <= 0`); a
/// positive dot means the blocker is behind the heading.
#[inline]
pub fn is_front_collision(center: Vec2, heading: Vec2, world_subtile_x: i32, world_subtile_y: i32) -> bool {
    let normal = occupancy_collision_normal(center, world_subtile_x, world_subtile_y);
    if normal.length_squared() <= 1e-8 || heading.length_squared() <= 1e-8 {
        return true;
    }
    heading.dot(normal) <= 0.0
}

/// Reflect `velocity` across `normal` (perfectly elastic, restitution = 1.0).
///
/// If `normal` is zero-length, falls back to an opposite-direction bounce.
#[inline]
pub fn reflect_velocity(velocity: Vec2, normal: Vec2) -> Vec2 {
    if velocity.length_squared() <= 1e-12 {
        return Vec2::ZERO;
    }
    if normal.length_squared() <= 1e-12 {
        return -velocity;
    }
    let n = normal.normalize();
    velocity - 2.0 * velocity.dot(n) * n
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

    /// Proposes this frame's move, validated against **static** geometry only,
    /// and records it in the actor's [`ActorShadow`] for the occupancy arbiter.
    ///
    /// Static geometry is read from the persistent `static_cache` (updated only
    /// on map edits); the actor's `blocked_flags()` decide which bits are
    /// impassable. Creature-on-creature conflicts are **not** checked here — the
    /// sequential resolution stage of [`movement::process_actor_moves`] resolves
    /// those afterward.
    ///
    /// The default tests the combined `(dx, dy)` footprint and cancels the whole
    /// step if any candidate cell is statically blocked. Classes that want
    /// wall-sliding (e.g. `BlackBot`) override this with an axis-decomposed
    /// static probe.
    ///
    /// Grid position invariant: the candidate center is
    /// `last_accepted_center_subtile + subtile_shift`; the float `center` is
    /// never consulted for grid position — it drifts for smooth rendering only.
    fn propose_move(&mut self, static_cache: &Hypermap<SubtilePassability>) {
        let (origin, previous, radius, blocked, want, tile_delta, rotation) = {
            let s = self.state();
            let origin = s
                .last_accepted_center_subtile
                .unwrap_or_else(|| s.center_subtile_i32());
            let previous = s
                .last_accepted_center_subtile
                .map(|c| (c, s.last_accepted_radius_subtiles));
            (
                origin,
                previous,
                s.radius_subtiles,
                self.blocked_flags(),
                s.move_buffer.subtile_shift,
                s.move_buffer.tile_delta,
                s.move_buffer.rotation_shift,
            )
        };
        let target = origin + want;
        let static_block = crate::map::passability::first_static_block(
            static_cache,
            target,
            radius,
            blocked,
            previous,
        );
        let (center, delta) = if static_block.is_none() {
            (target, tile_delta)
        } else {
            (origin, Vec2::ZERO)
        };

        let s = self.state_mut();
        s.shadow.world_previous = s.center;
        s.shadow.origin = origin;
        s.shadow.proposed_center = center;
        s.shadow.proposed_delta = delta;
        s.shadow.proposed_rotation = rotation;
        s.shadow.static_block = static_block;
        s.shadow.participates = true;
        s.move_buffer = ActorMoveBuffer::default();
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

/// Plugin that runs low-level actor processing every fixed (60 Hz) tick.
pub struct ActorPlugin;

impl Plugin for ActorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Paused>()
            .init_resource::<OccupancyArbiter>()
            // Input sampling stays on the render frame — `just_pressed` edges
            // can be missed by fixed ticks at high frame rates.
            .add_systems(Update, toggle_pause.run_if(in_state(GameState::InGame)))
            // The movement pipeline runs on the fixed 60 Hz schedule so bot
            // pace is independent of the render frame rate (see `GamePlugin`).
            .add_systems(
                FixedUpdate,
                (
                    flush_actor_occupancy,
                    process_actor_moves,
                )
                    .chain()
                    .run_if(in_state(GameState::InGame))
                    .run_if(not(is_paused)),
            );
    }
}

fn toggle_pause(keys: Res<ButtonInput<KeyCode>>, mut paused: ResMut<Paused>) {
    if keys.just_pressed(KeyCode::Space) {
        paused.0 = !paused.0;
    }
}

/// Flush write→read; clear write buffer. Must be first in the actor pipeline.
pub(crate) fn flush_actor_occupancy(passability: Res<DynamicPassabilityMap>) {
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
/// Each candidate is tested with
/// [`DynamicPassabilityMap::try_claim_reentry_footprint`], which checks the
/// **write** buffer in addition to static geometry and the read buffer and
/// stamps the claim into the write buffer on success. Called sequentially (see
/// [`process_actors`]), this keeps several actors re-entering on the same frame
/// from being placed on the same cell.
///
/// If nothing is passable the actor is left in place without stamping a
/// footprint; it will retry next frame.
pub(crate) fn resolve_offscreen_collision(
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

    // 1. Try current position. Re-entry placement probes the write buffer too,
    // so several actors re-entering on the same frame (resolved sequentially)
    // never claim the same cell.
    if dynamic
        .try_claim_reentry_footprint(current_sub, radius, blocked, static_cache)
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
            .try_claim_reentry_footprint(candidate, radius, blocked, static_cache)
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
                    .try_claim_reentry_footprint(candidate, radius, blocked, static_cache)
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
                    shadow: ActorShadow::default(),
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
                shadow: ActorShadow::default(),
            },
        }
    }

    #[test]
    fn propose_move_clear_sets_proposed_center_and_shadow() {
        // Static-clear proposal: advances the proposed center, no static block,
        // and records the compact footprint centers for the arbiter.
        let sc = empty_static_cache();
        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 1);
        actor.state.move_buffer.tile_delta = Vec2::new(1.0, 0.0);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        actor.propose_move(&sc);

        assert!(actor.state.shadow.participates);
        assert_eq!(actor.state.shadow.proposed_center, IVec2::new(55, 50));
        assert_eq!(actor.state.shadow.proposed_delta, Vec2::new(1.0, 0.0));
        assert_eq!(actor.state.shadow.static_block, None);
        // The back-off origin is the pre-move grid center.
        assert_eq!(actor.state.shadow.origin, IVec2::new(50, 50));
        // propose does not move `center` — apply (in the arbiter) does.
        assert_eq!(actor.state.center, Vec2::new(10.0, 10.0));
    }

    #[test]
    fn propose_move_default_blocked_by_static_wall_holds() {
        // Wall (all edges) at tile (11, 10): the proposed cell is statically
        // blocked, so the default combined proposal cancels the whole step.
        let sc = static_cache_with_wall(11, 10, WallMask::from_bits(0x0F).unwrap());
        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 0);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        actor.propose_move(&sc);

        assert!(actor.state.shadow.static_block.is_some());
        assert_eq!(actor.state.shadow.proposed_center, IVec2::new(50, 50));
        assert_eq!(actor.state.shadow.proposed_delta, Vec2::ZERO);
    }

    #[test]
    fn propose_move_flyer_crosses_void() {
        let sc = static_cache_all_void();
        let mut actor = fresh_flying(Vec2::new(10.0, 10.0), 0);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        actor.propose_move(&sc);

        // Flyer's blocked_flags is FLAG_BLOCKED only, so void is not a block.
        assert_eq!(actor.state.shadow.static_block, None);
        assert_eq!(actor.state.shadow.proposed_center, IVec2::new(55, 50));
    }

    #[test]
    fn propose_move_flyer_stops_at_wall() {
        let sc = static_cache_with_wall(11, 10, WallMask::from_bits(0x0F).unwrap());
        let mut actor = fresh_flying(Vec2::new(10.0, 10.0), 0);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        actor.propose_move(&sc);

        assert!(actor.state.shadow.static_block.is_some());
        assert_eq!(actor.state.shadow.proposed_center, IVec2::new(50, 50));
    }

    #[test]
    fn propose_move_previous_shadow_is_origin_footprint() {
        // The back-off origin is the last accepted grid center (here the
        // first-frame fallback floor(center*5) = (50,50)).
        let sc = empty_static_cache();
        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 1);
        actor.state.move_buffer.subtile_shift = IVec2::new(3, 0);
        actor.propose_move(&sc);
        assert_eq!(actor.state.shadow.origin, IVec2::new(50, 50));
        assert_eq!(actor.state.shadow.proposed_center, IVec2::new(53, 50));
    }

    #[test]
    fn occupancy_collision_normal_points_away_from_blocker() {
        let center = Vec2::new(10.0, 10.0);
        // Subtile immediately to the actor's +X side.
        let blocker_x = 10 * SUBTILE_COUNT as i32 + 1;
        let blocker_y = 10 * SUBTILE_COUNT as i32;
        let n = occupancy_collision_normal(center, blocker_x, blocker_y);
        assert!(n.x < 0.0, "normal must point left away from right-side blocker");
    }

    #[test]
    fn reflect_velocity_flips_normal_component() {
        let v = Vec2::new(1.0, 0.0);
        let n = Vec2::new(-1.0, 0.0);
        let bounced = reflect_velocity(v, n);
        assert!((bounced.x + 1.0).abs() < 1e-6);
        assert!(bounced.y.abs() < 1e-6);
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
    fn resolve_offscreen_same_frame_reentrants_dont_overlap() {
        // Two actors re-entering on the **same** frame from the same subtile must
        // be placed on different cells. They are resolved sequentially with NO
        // flush in between (mirroring `process_actors`' re-entry pass): the second
        // sees the first's claim only because re-entry placement probes the write
        // buffer. Under the old read-buffer-only path both would keep (27, 27).
        let map = DynamicPassabilityMap::new();
        let sc = empty_static_cache();

        let mut a = DummyActor::new(Vec2::new(5.5, 5.5), 0);
        a.state.last_accepted_center_subtile = Some(IVec2::new(27, 27));
        let mut b = DummyActor::new(Vec2::new(5.5, 5.5), 0);
        b.state.last_accepted_center_subtile = Some(IVec2::new(27, 27));

        super::resolve_offscreen_collision(&mut a, &map, &sc);
        super::resolve_offscreen_collision(&mut b, &map, &sc);

        let a_sub = a.state.last_accepted_center_subtile.unwrap();
        let b_sub = b.state.last_accepted_center_subtile.unwrap();
        assert_eq!(a_sub, IVec2::new(27, 27), "first re-entrant keeps the clear cell");
        assert_ne!(a_sub, b_sub, "same-frame re-entrants must not share a cell");

        // Both claims are stamped in the write buffer (visible after flush).
        map.flush();
        let view = SubtilePassabilityMap::new(&map);
        assert_ne!(view.flags_xy(0, 0, a_sub.x, a_sub.y) & FLAG_BLOCKED, 0);
        assert_ne!(view.flags_xy(0, 0, b_sub.x, b_sub.y) & FLAG_BLOCKED, 0);
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

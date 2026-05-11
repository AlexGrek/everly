//! Actor runtime: trait-based low-level logic and movement over subtile occupancy.
//!
//! This module introduces a generic [`Actor`] trait and a single Bevy system that
//! processes every actor entity each frame:
//!
//! 1. Clear the last movement error (low-level step reset).
//! 2. Run [`Actor::think_low_level`].
//! 3. Run [`Actor::prepare_movement`] to fill the per-frame movement buffer.
//! 4. Run [`Actor::try_move`] to resolve movement and stamp occupancy into the
//!    dynamic subtile map write buffer.
//! 5. Flush the dynamic map write buffer to read buffer after all actors.
//!
//! Coordinates:
//! - Actor center is stored as float tile coordinates (`Vec2`).
//! - Movement deltas are stored in integer subtile units.
//! - `1 tile = 5 subtiles` (see `SUBTILE_COUNT`).
//!
//! Shape:
//! - Actors use circular integer "shadows" with radius measured in subtiles.
//! - Shadow offsets are baked and cached per-radius, then reused.

pub mod glitch_bot;

use bevy::prelude::*;

use crate::map::passability::{
    ActorFootprint, DynamicPassabilityMap, TryUpdateFootprintError, SUBTILE_COUNT,
};
use crate::menu::main_menu::GameState;

/// Mutable movement intent for the current frame.
///
/// Values are relative to the actor's current center and rotation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActorMoveBuffer {
    /// Relative center shift in integer subtile units.
    pub subtile_shift: IVec2,
    /// Relative rotation shift for this frame.
    pub rotation_shift: f32,
}

impl Default for ActorMoveBuffer {
    fn default() -> Self {
        Self {
            subtile_shift: IVec2::ZERO,
            rotation_shift: 0.0,
        }
    }
}

/// Last movement failure reason produced by [`Actor::try_move`].
#[derive(Debug, Clone, PartialEq)]
pub enum ActorMovementError {
    /// Candidate movement footprint intersects a blocked subtile.
    BlockedByOccupancy { world_subtile_x: i32, world_subtile_y: i32 },
    /// Radius must be non-negative.
    InvalidRadius(i32),
}

/// Common mutable actor data handled by low-level movement.
#[derive(Debug, Clone)]
pub struct ActorState {
    /// Center in tile-space (`x` and `y` in world tiles, float precision).
    pub center: Vec2,
    /// Radius of the circular occupancy shadow, measured in subtiles.
    pub radius_subtiles: i32,
    /// Actor yaw/rotation in radians (or arbitrary game-space units).
    pub rotation: f32,
    /// Per-frame movement intent written by [`Actor::prepare_movement`].
    pub move_buffer: ActorMoveBuffer,
    /// Cleared at the beginning of each low-level processing step.
    pub last_movement_error: Option<ActorMovementError>,
    /// Persisted occupied world-subtiles from the previous movement frame.
    ///
    /// This is passed into passability updates so self-overlap does not count
    /// as collision.
    pub footprint: ActorFootprint,
}

impl ActorState {
    /// Tile-space center converted to integer tile coordinates (floor).
    #[inline]
    pub fn center_tile_i32(&self) -> IVec2 {
        IVec2::new(self.center.x.floor() as i32, self.center.y.floor() as i32)
    }

    /// Center converted to integer subtile coordinates by rounding.
    ///
    /// This representation is used by movement and occupancy stamping.
    #[inline]
    pub fn center_subtile_i32(&self) -> IVec2 {
        let scale = SUBTILE_COUNT as f32;
        IVec2::new(
            (self.center.x * scale).round() as i32,
            (self.center.y * scale).round() as i32,
        )
    }
}

/// Trait implemented by all actor logic objects.
///
/// The trait intentionally separates high-level asynchronous planning (outside
/// this module) from deterministic per-frame low-level logic.
pub trait Actor: Send + Sync + 'static {
    /// Shared mutable actor state.
    fn state(&self) -> &ActorState;
    fn state_mut(&mut self) -> &mut ActorState;

    /// Low-level deterministic thinking step.
    ///
    /// Called every frame before movement preparation. Implementations can read
    /// sensor data and update internal state.
    fn think_low_level(&mut self) {}

    /// Fill or adjust [`ActorState::move_buffer`] for this frame.
    ///
    /// Called after [`think_low_level`](Self::think_low_level).
    fn prepare_movement(&mut self) {}

    /// Applies movement transform and stores the accepted footprint.
    ///
    /// This low-level operation is shared for all actors:
    /// - center/rotation are advanced from `move_buffer`
    /// - `footprint` is replaced with the map-approved footprint.
    fn move_actor(&mut self, new_footprint: ActorFootprint) -> Result<(), ActorMovementError> {
        let state = self.state_mut();
        if state.radius_subtiles < 0 {
            return Err(ActorMovementError::InvalidRadius(state.radius_subtiles));
        }

        let tile_delta = state.move_buffer.subtile_shift.as_vec2() / SUBTILE_COUNT as f32;
        state.center += tile_delta;
        state.rotation += state.move_buffer.rotation_shift;
        state.footprint = new_footprint;
        state.move_buffer = ActorMoveBuffer::default();
        Ok(())
    }

    /// Attempts to move; on collision records an error and keeps position.
    ///
    /// Behavior:
    /// - Calls [`move_actor`](Self::move_actor) when movement is valid.
    /// - If any candidate subtile collides with already-blocked data from the
    ///   passability read buffer, movement is rejected.
    /// - On rejection, `last_movement_error` is filled and the actor remains at
    ///   previous center/rotation.
    /// - Current footprint is still stamped into the write buffer so occupancy
    ///   persists for the next frame.
    fn try_move(&mut self, passability: &DynamicPassabilityMap) {
        let (radius, current_center_sub, requested_shift, previous_footprint) = {
            let s = self.state();
            (
                s.radius_subtiles,
                s.center_subtile_i32(),
                s.move_buffer.subtile_shift,
                s.footprint.clone(),
            )
        };
        let next_center_sub = current_center_sub + requested_shift;
        match passability.try_update_footprint(next_center_sub, radius, &previous_footprint) {
            Ok(new_footprint) => {
                if let Err(err) = self.move_actor(new_footprint) {
                    self.state_mut().last_movement_error = Some(err);
                }
            }
            Err(err) => {
                let state = self.state_mut();
                state.last_movement_error = Some(match err {
                    TryUpdateFootprintError::InvalidRadius(r) => ActorMovementError::InvalidRadius(r),
                    TryUpdateFootprintError::BlockedByOccupancy { world_subtile } => {
                        ActorMovementError::BlockedByOccupancy {
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

/// Plugin that runs low-level actor processing every in-game frame.
pub struct ActorPlugin;

impl Plugin for ActorPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (process_actors, flush_actor_occupancy)
                .chain()
                .run_if(in_state(GameState::InGame)),
        );
    }
}

/// Drives the complete low-level actor pipeline for all registered actors.
pub fn process_actors(mut actors: Query<&mut ActorObject>, passability: Res<DynamicPassabilityMap>) {
    for mut actor in &mut actors {
        let actor = actor.inner.as_mut();
        actor.state_mut().last_movement_error = None;
        actor.think_low_level();
        actor.prepare_movement();
        actor.try_move(&passability);
    }
}

/// Publishes actor occupancy writes for next frame reads.
fn flush_actor_occupancy(passability: Res<DynamicPassabilityMap>) {
    passability.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::passability::{DynamicPassabilityMap, SubtilePassabilityMap};

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
                    footprint: Vec::new(),
                },
            }
        }
    }

    impl Actor for DummyActor {
        fn state(&self) -> &ActorState {
            &self.state
        }

        fn state_mut(&mut self) -> &mut ActorState {
            &mut self.state
        }
    }

    #[test]
    fn try_move_updates_center_and_writes_shadow() {
        let map = DynamicPassabilityMap::new();
        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 1);
        actor.state.move_buffer.subtile_shift = IVec2::new(5, 0);
        actor.try_move(&map);
        map.flush();
        let view = SubtilePassabilityMap::new(&map);

        assert_eq!(actor.state.center, Vec2::new(11.0, 10.0));
        assert!(!actor.state.footprint.is_empty());
        // Center subtile for (11,10) is (55,50); local center subtile should be blocked.
        assert!(!view.subtile_xy(0, 0, 55, 50));
        assert!(actor.state.last_movement_error.is_none());
    }

    #[test]
    fn try_move_reports_collision_and_keeps_center() {
        let map = DynamicPassabilityMap::new();
        let view = SubtilePassabilityMap::new(&map);
        let mut actor = DummyActor::new(Vec2::new(10.0, 10.0), 0);

        // Pre-block target subtile in read buffer.
        view.set_subtile_xy(0, 0, 51, 50, false);
        map.flush();

        actor.state.move_buffer.subtile_shift = IVec2::new(1, 0);
        actor.try_move(&map);

        assert_eq!(actor.state.center, Vec2::new(10.0, 10.0));
        assert!(matches!(
            actor.state.last_movement_error,
            Some(ActorMovementError::BlockedByOccupancy { .. })
        ));
    }
}

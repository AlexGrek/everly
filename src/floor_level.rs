//! Active map elevation (0–9). Drives vertical stacking of hypermap floors and camera height.

use bevy::prelude::*;

/// Nominal wall height per storey (m).
pub const HYPERMAP_WALL_HEIGHT: f32 = 3.0;
/// Vertical spacing between floor planes — a hair above [`HYPERMAP_WALL_HEIGHT`] so the next
/// storey’s floor mesh does not z-fight with wall tops.
pub const HYPERMAP_FLOOR_HEIGHT: f32 = HYPERMAP_WALL_HEIGHT + 0.03;
/// Inclusive highest floor index (`0..=HYPERMAP_FLOOR_MAX`).
pub const HYPERMAP_FLOOR_MAX: u8 = 9;
/// How quickly [`crate::camera::StrategyCamera::focus`] Y eases toward the active floor height (higher = snappier).
pub const CAMERA_FLOOR_Y_SMOOTH_PER_S: f32 = 6.0;

/// Currently viewed floor. Floors `level..=HYPERMAP_FLOOR_MAX` are rendered (see hypermap_world).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveFloorLevel(pub u8);

impl Default for ActiveFloorLevel {
    fn default() -> Self {
        Self(0)
    }
}

pub struct FloorLevelPlugin;

impl Plugin for FloorLevelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ActiveFloorLevel>();
    }
}

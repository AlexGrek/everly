//! Scene presentation: how the world is shown to the player.
//!
//! Owns the strategy camera (with its post-processing stack) and scene
//! lighting (the directional sun). Pure-rendering / view concerns live
//! here so gameplay modules can depend on them without circular reach.

pub mod camera;
pub mod sun;

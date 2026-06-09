//! Pickable mesh marker for actor hover / click in the HUD inspector.

use bevy::prelude::*;

/// Root entity carrying [`crate::actor::ActorObject`] and actor visuals.
#[derive(Component)]
pub struct ActorInspectable;

/// When `true`, this actor's game-log events are recorded with the FORCE flag
/// and stay visible even when the global log overlay is disabled.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct ActorForceLogs(pub bool);

/// Mesh child used for [`bevy::picking::prelude::Pickable`] hits.
#[derive(Component)]
pub struct ActorPickMesh;

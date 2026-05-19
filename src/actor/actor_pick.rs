//! Pickable mesh marker for actor hover / click in the HUD inspector.

use bevy::prelude::*;

/// Root entity carrying [`crate::actor::ActorObject`] and actor visuals.
#[derive(Component)]
pub struct ActorInspectable;

/// Mesh child used for [`bevy::picking::prelude::Pickable`] hits.
#[derive(Component)]
pub struct ActorPickMesh;

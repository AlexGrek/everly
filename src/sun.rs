//! Single world-space sun: a [`DirectionalLight`] tilted across the playfield.
//!
//! The scene leans on emissive materials + bloom for mood, so the sun is set
//! well below `RAW_SUNLIGHT` to avoid washing out the glow columns. Tweak
//! [`SUN_ILLUMINANCE`] to dim or brighten the whole world without touching
//! the rest of the lighting setup.

use bevy::light::light_consts::lux;
use bevy::prelude::*;

/// Marker for the world's primary sun entity.
#[derive(Component, Debug)]
pub struct Sun;

/// Direct sunlight strength, in lux. Just above Bevy's `OVERCAST_DAY` (1 000)
/// — a soft fill that defines surfaces without competing with the emissive
/// columns and bloom that carry the scene's mood.
pub const SUN_ILLUMINANCE: f32 = lux::OVERCAST_DAY * 1.5;

/// Slight golden-hour tint so the white ground reads as "lit" instead of "self-emissive".
const SUN_COLOR: Color = Color::srgb(1.0, 0.96, 0.88);

/// World position the sun "sits at"; only its *direction* (toward origin) matters.
const SUN_POSITION: Vec3 = Vec3::new(60.0, 110.0, 40.0);

pub struct SunPlugin;

impl Plugin for SunPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_sun);
    }
}

fn spawn_sun(mut commands: Commands) {
    commands.spawn((
        Name::new("Sun"),
        Sun,
        DirectionalLight {
            color: SUN_COLOR,
            illuminance: SUN_ILLUMINANCE,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_translation(SUN_POSITION).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

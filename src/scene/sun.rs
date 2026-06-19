//! Single world-space sun: a [`DirectionalLight`] tilted across the playfield.
//!
//! The scene leans on emissive materials + bloom for mood, so the sun is set
//! well below `RAW_SUNLIGHT` to avoid washing out the glow columns. Tweak
//! [`SUN_ILLUMINANCE`] to dim or brighten the whole world without touching
//! the rest of the lighting setup. Toggle via [`SunEnabled`] (Overlays panel).

use bevy::light::light_consts::lux;
use bevy::prelude::*;

use crate::menu::main_menu::GameState;

/// Marker for the world's primary sun entity.
#[derive(Component, Debug)]
pub struct Sun;

/// Overlays-panel toggle for the directional sun.
#[derive(Resource, Clone, Copy, PartialEq, Eq)]
pub struct SunEnabled(pub bool);

impl Default for SunEnabled {
    fn default() -> Self {
        Self(true)
    }
}

/// Direct sunlight strength, in lux. Below Bevy's `OVERCAST_DAY` (1 000) — a
/// dim fill that defines surfaces while letting the emissive columns, bloom,
/// and the point lights (lamps/chargers) carry the scene's mood.
pub const SUN_ILLUMINANCE: f32 = lux::OVERCAST_DAY * 0.75;

/// Slight golden-hour tint so the white ground reads as "lit" instead of "self-emissive".
const SUN_COLOR: Color = Color::srgb(1.0, 0.96, 0.88);

/// World position the sun "sits at"; only its *direction* (toward origin) matters.
const SUN_POSITION: Vec3 = Vec3::new(60.0, 110.0, 40.0);

pub struct SunPlugin;

impl Plugin for SunPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SunEnabled>()
            .add_systems(OnEnter(GameState::Loading), spawn_sun)
            .add_systems(
                Update,
                sync_sun_enabled
                    .run_if(in_state(GameState::InGame))
                    .run_if(resource_changed::<SunEnabled>),
            );
    }
}

fn spawn_sun(mut commands: Commands, enabled: Res<SunEnabled>) {
    commands.spawn((
        Name::new("Sun"),
        Sun,
        DirectionalLight {
            color: SUN_COLOR,
            illuminance: if enabled.0 { SUN_ILLUMINANCE } else { 0.0 },
            shadows_enabled: enabled.0,
            ..default()
        },
        Transform::from_translation(SUN_POSITION).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

fn sync_sun_enabled(enabled: Res<SunEnabled>, mut lights: Query<&mut DirectionalLight, With<Sun>>) {
    let on = enabled.0;
    for mut light in &mut lights {
        light.illuminance = if on { SUN_ILLUMINANCE } else { 0.0 };
        light.shadows_enabled = on;
    }
}

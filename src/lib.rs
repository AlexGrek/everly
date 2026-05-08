//! Everly — a 3D strategy-camera sandbox built with Bevy 0.18.
//!
//! Each subsystem lives in its own module behind a small `Plugin`. The
//! top-level [`GamePlugin`] is the single entry point that wires them
//! together, so `main.rs` stays tiny and the modules stay decoupled.

use bevy::light::GlobalAmbientLight;
use bevy::math::Vec2;
use bevy::pbr::DefaultOpaqueRendererMethod;
use bevy::prelude::*;
use bevy_water::{WaterPlugin, WaterQuality, WaterSettings};

pub mod boxes;
pub mod camera;
pub mod ground;
pub mod hypermap;
pub mod hypermap_world;
pub mod sun;
pub mod world_map;

/// Aggregates every gameplay subsystem of Everly.
pub struct GamePlugin;

impl Plugin for GamePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(Color::BLACK))
            // No fill light unless we add one on a camera; keeps the void emissive-only.
            .insert_resource(GlobalAmbientLight::NONE)
            // SSR runs after deferred lighting; opaque `StandardMaterial` must use the
            // deferred path (not forward) for reflections to appear.
            .insert_resource(DefaultOpaqueRendererMethod::deferred())
            .insert_resource(WaterSettings {
                spawn_tiles: None,
                height: world_map::WATER_SURFACE_Y,
                amplitude: 0.5,
                clarity: 0.38,
                water_quality: WaterQuality::High,
                wave_direction: Vec2::new(0.82, 0.28),
                ..default()
            })
            .add_plugins((
                WaterPlugin,
                camera::StrategyCameraPlugin,
                hypermap_world::HypermapWorldPlugin,
                sun::SunPlugin,
            ));
    }
}

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

pub mod actor;
pub mod edit;
pub mod hud;
pub mod map;
pub mod menu;
pub mod scene;

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
                height: map::world_map::WATER_SURFACE_Y,
                amplitude: 0.5,
                clarity: 0.38,
                water_quality: WaterQuality::High,
                wave_direction: Vec2::new(0.82, 0.28),
                ..default()
            })
            .add_plugins((
                WaterPlugin,
                MeshPickingPlugin,
                menu::main_menu::MainMenuPlugin,
                scene::camera::StrategyCameraPlugin,
                scene::camera_snapshot::CameraSnapshotPlugin,
                scene::sun::SunPlugin,
                hud::game_hud::GameHudPlugin,
                hud::actor_inspector::ActorInspectorPlugin,
                map::floor_level::FloorLevelPlugin,
                map::level::LevelPlugin,
                map::hypermap_world::HypermapWorldPlugin,
                map::chunk_overlay::ChunkOverlayPlugin,
                map::dirt::DirtMapPlugin,
                map::dirt_overlay::DirtOverlayPlugin,
                map::passability::PassabilityMapPlugin,
            ))
            .add_plugins((
                map::temperature::TemperatureMapPlugin,
                map::temperature_overlay::TemperatureOverlayPlugin,
                map::field_interactions::FieldInteractionsPlugin,
                actor::ActorPlugin,
                actor::snapshot::ActorSnapshotPlugin,
                actor::glitch_bot::GlitchBotPlugin,
                actor::black_bot::BlackBotPlugin,
                edit::map_edit::MapEditPlugin,
                edit::map_selection::MapSelectionPlugin,
            ))
            .add_systems(
                OnEnter(menu::main_menu::GameState::InGame),
                edit::map_edit::spawn_map_edit_palette.after(hud::game_hud::spawn_bottom_hud),
            );
    }
}

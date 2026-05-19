//! Temperature heatmap overlay — one RGBA texel per world tile (°C colormap).

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::PlaneMeshBuilder;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::map::chunk_overlay::TEMPERATURE_OVERLAY_Y;
use crate::map::hypermap::{ChunkCoord, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::temperature::{temperature_celsius_to_rgba, TemperatureMap};
use crate::map::tile_field::{TileFieldMap, TILE_FIELD_OVERLAY_RES};
use crate::menu::main_menu::GameState;

/// HUD / **F5** toggle for the temperature heatmap overlay (off by default).
#[derive(Resource, Clone, Copy)]
pub struct TemperatureOverlayEnabled(pub bool);

impl Default for TemperatureOverlayEnabled {
    fn default() -> Self {
        Self(false)
    }
}

#[derive(Resource, Default)]
pub struct TemperatureOverlayState {
    overlays: HashMap<ChunkCoord, (Entity, Handle<Image>, Handle<StandardMaterial>)>,
}

#[derive(Resource)]
struct TemperatureOverlayAssets {
    plane_mesh: Handle<Mesh>,
}

#[derive(Component)]
struct TemperatureOverlayPlane;

pub struct TemperatureOverlayPlugin;

impl Plugin for TemperatureOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(TemperatureOverlayEnabled::default())
            .init_resource::<TemperatureOverlayState>()
            .add_systems(
                OnEnter(GameState::InGame),
                (reset_temperature_overlay_on_enter, setup_temperature_overlay).chain(),
            )
            .add_systems(
                Update,
                (
                    toggle_temperature_overlay,
                    sync_temperature_overlays,
                    update_temperature_overlay_textures,
                )
                    .chain()
                    .after(crate::map::temperature::flush_temperature_map)
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

fn reset_temperature_overlay_on_enter(
    mut enabled: ResMut<TemperatureOverlayEnabled>,
    mut state: ResMut<TemperatureOverlayState>,
    mut commands: Commands,
    planes: Query<Entity, With<TemperatureOverlayPlane>>,
) {
    enabled.0 = false;
    for entity in planes.iter() {
        commands.entity(entity).despawn();
    }
    state.overlays.clear();
}

fn setup_temperature_overlay(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>) {
    let size = HYPERMAP_CHUNK_SIZE as f32;
    let plane_mesh = meshes.add(PlaneMeshBuilder::from_size(Vec2::splat(size)));
    commands.insert_resource(TemperatureOverlayAssets { plane_mesh });
}

fn toggle_temperature_overlay(
    keys: Res<ButtonInput<KeyCode>>,
    mut enabled: ResMut<TemperatureOverlayEnabled>,
) {
    if keys.just_pressed(KeyCode::F5) {
        enabled.0 = !enabled.0;
    }
}

fn new_tile_field_image() -> Image {
    let size = TILE_FIELD_OVERLAY_RES;
    Image::new(
        Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        TextureDimension::D2,
        vec![0u8; (size * size * 4) as usize],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
}

fn overlay_material(
    image_handle: Handle<Image>,
    materials: &mut Assets<StandardMaterial>,
) -> Handle<StandardMaterial> {
    materials.add(StandardMaterial {
        base_color_texture: Some(image_handle),
        base_color: Color::WHITE,
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        cull_mode: None,
        ..default()
    })
}

fn sync_temperature_overlays(
    mut commands: Commands,
    runtime: Res<HypermapRuntime>,
    enabled: Res<TemperatureOverlayEnabled>,
    temperature: Res<TemperatureMap>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    assets: Res<TemperatureOverlayAssets>,
    mut state: ResMut<TemperatureOverlayState>,
    planes: Query<Entity, With<TemperatureOverlayPlane>>,
) {
    let desired: std::collections::HashSet<ChunkCoord> = if enabled.0 {
        runtime.desired_chunk_coords().into_iter().collect()
    } else {
        std::collections::HashSet::new()
    };

    let to_remove: Vec<ChunkCoord> = state
        .overlays
        .keys()
        .filter(|c| !desired.contains(c))
        .copied()
        .collect();
    for coord in to_remove {
        if let Some((entity, _, _)) = state.overlays.remove(&coord) {
            if planes.get(entity).is_ok() {
                commands.entity(entity).despawn();
            }
        }
    }

    for &coord in &desired {
        if state.overlays.contains_key(&coord) {
            continue;
        }
        temperature.mark_dirty(coord);
        let image_handle = images.add(new_tile_field_image());
        let mat_handle = overlay_material(image_handle.clone(), &mut materials);
        let cx = coord.x as f32 * HYPERMAP_CHUNK_SIZE as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
        let cz = coord.y as f32 * HYPERMAP_CHUNK_SIZE as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
        let entity = commands
            .spawn((
                Name::new(format!("Temperature overlay {},{}", coord.x, coord.y)),
                Mesh3d(assets.plane_mesh.clone()),
                MeshMaterial3d(mat_handle.clone()),
                Transform::from_xyz(cx, TEMPERATURE_OVERLAY_Y, cz),
                NotShadowCaster,
                NotShadowReceiver,
                TemperatureOverlayPlane,
            ))
            .id();
        state.overlays.insert(coord, (entity, image_handle, mat_handle));
    }
}

fn update_temperature_overlay_textures(
    enabled: Res<TemperatureOverlayEnabled>,
    temperature: Res<TemperatureMap>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    state: Res<TemperatureOverlayState>,
) {
    if !enabled.0 {
        return;
    }
    let dirty = temperature.take_dirty_chunks();
    if dirty.is_empty() {
        return;
    }

    let coords: Vec<(ChunkCoord, Handle<Image>, Handle<StandardMaterial>)> = state
        .overlays
        .iter()
        .filter(|(c, _)| dirty.contains(c))
        .map(|(&c, (_, img, mat))| (c, img.clone(), mat.clone()))
        .collect();

    for (coord, img_handle, mat_handle) in coords {
        let Some(image) = images.get_mut(&img_handle) else { continue; };
        let Some(data) = image.data.as_mut() else { continue; };

        let chunk_existed = temperature.read_map().with_chunk_read(coord, |chunk| {
            TileFieldMap::paint_chunk_to_rgba(data, chunk, temperature_celsius_to_rgba);
        }).is_some();

        if !chunk_existed {
            data.fill(0);
        }
        materials.get_mut(&mat_handle);
    }
}

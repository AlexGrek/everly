//! Transparent dirt stain overlay — one RGBA plane per visible chunk.
//!
//! Sits slightly below the generic / occupancy overlays ([`DIRT_OVERLAY_Y`]).
//! Texel darkness scales with dirt amount from [`DirtMap`].

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::PlaneMeshBuilder;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::map::dirt::{DirtMap, DIRT_OVERLAY_RES, DIRT_SUBDIV};
use crate::map::chunk_overlay::DIRT_OVERLAY_Y;
use crate::map::hypermap::{ChunkCoord, LocalCoord, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_world::HypermapRuntime;
use crate::menu::main_menu::GameState;

#[derive(Resource, Default)]
pub struct DirtOverlayState {
    overlays: HashMap<ChunkCoord, (Entity, Handle<Image>, Handle<StandardMaterial>)>,
}

impl DirtOverlayState {
    pub fn image_for(&self, coord: ChunkCoord) -> Option<&Handle<Image>> {
        self.overlays.get(&coord).map(|(_, img, _)| img)
    }

    pub fn material_for(&self, coord: ChunkCoord) -> Option<&Handle<StandardMaterial>> {
        self.overlays.get(&coord).map(|(_, _, mat)| mat)
    }
}

#[derive(Resource)]
struct DirtOverlayAssets {
    plane_mesh: Handle<Mesh>,
}

#[derive(Component)]
struct DirtOverlayPlane;

pub struct DirtOverlayPlugin;

impl Plugin for DirtOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DirtOverlayState>()
            .add_systems(OnEnter(GameState::InGame), setup_dirt_overlay)
            .add_systems(
                Update,
                (sync_dirt_overlays, update_dirt_overlay_textures)
                    .chain()
                    .after(crate::map::dirt::flush_dirt_map)
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

fn setup_dirt_overlay(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>) {
    let size = HYPERMAP_CHUNK_SIZE as f32;
    let plane_mesh = meshes.add(PlaneMeshBuilder::from_size(Vec2::splat(size)));
    commands.insert_resource(DirtOverlayAssets { plane_mesh });
}

fn new_dirt_overlay_image() -> Image {
    let size = DIRT_OVERLAY_RES;
    Image::new(
        Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        TextureDimension::D2,
        vec![0u8; (size * size * 4) as usize],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
}

fn dirt_overlay_material(
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

fn spawn_dirt_overlay_plane(
    commands: &mut Commands,
    plane_mesh: Handle<Mesh>,
    mat: Handle<StandardMaterial>,
    coord: ChunkCoord,
) -> Entity {
    let cx = coord.x as f32 * HYPERMAP_CHUNK_SIZE as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
    let cz = coord.y as f32 * HYPERMAP_CHUNK_SIZE as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
    commands
        .spawn((
            Name::new(format!("Dirt overlay {},{}", coord.x, coord.y)),
            Mesh3d(plane_mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(cx, DIRT_OVERLAY_Y, cz),
            NotShadowCaster,
            NotShadowReceiver,
            DirtOverlayPlane,
        ))
        .id()
}

fn dirt_to_rgba(dirt: f32) -> [u8; 4] {
    if dirt <= 0.0 {
        return [0, 0, 0, 0];
    }
    let a = (dirt.clamp(0.0, 1.0) * 255.0).round() as u8;
    [0, 0, 0, a]
}

fn sync_dirt_overlays(
    mut commands: Commands,
    runtime: Res<HypermapRuntime>,
    dirt: Res<DirtMap>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    assets: Res<DirtOverlayAssets>,
    mut state: ResMut<DirtOverlayState>,
    planes: Query<Entity, With<DirtOverlayPlane>>,
) {
    let desired: std::collections::HashSet<ChunkCoord> =
        runtime.desired_chunk_coords().into_iter().collect();

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
        dirt.mark_dirty(coord);
        let image_handle = images.add(new_dirt_overlay_image());
        let mat_handle = dirt_overlay_material(image_handle.clone(), &mut materials);
        let entity = spawn_dirt_overlay_plane(
            &mut commands,
            assets.plane_mesh.clone(),
            mat_handle.clone(),
            coord,
        );
        state.overlays.insert(coord, (entity, image_handle, mat_handle));
    }
}

fn update_dirt_overlay_textures(
    dirt: Res<DirtMap>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    state: Res<DirtOverlayState>,
) {
    let dirty = dirt.take_dirty_chunks();
    if dirty.is_empty() {
        return;
    }

    let subdiv = DIRT_SUBDIV;
    let res = DIRT_OVERLAY_RES as usize;

    let coords: Vec<(ChunkCoord, Handle<Image>, Handle<StandardMaterial>)> = state
        .overlays
        .iter()
        .filter(|(c, _)| dirty.contains(c))
        .map(|(&c, (_, img, mat))| (c, img.clone(), mat.clone()))
        .collect();

    for (coord, img_handle, mat_handle) in coords {
        let Some(image) = images.get_mut(&img_handle) else { continue; };
        let Some(data) = image.data.as_mut() else { continue; };

        let chunk_existed = dirt.read_map().with_chunk_read(coord, |chunk| {
            for tile_ly in 0..HYPERMAP_CHUNK_SIZE as usize {
                for tile_lx in 0..HYPERMAP_CHUNK_SIZE as usize {
                    let local = LocalCoord::new(tile_lx as i32, tile_ly as i32);
                    let tile = chunk.get_local(local);
                    for sy in 0..subdiv {
                        for sx in 0..subdiv {
                            let dirt = tile.at(sy, sx);
                            let px = tile_lx * subdiv + sx;
                            let py = tile_ly * subdiv + sy;
                            let idx = (py * res + px) * 4;
                            let color = dirt_to_rgba(dirt);
                            data[idx..idx + 4].copy_from_slice(&color);
                        }
                    }
                }
            }
        }).is_some();

        if !chunk_existed {
            data.fill(0);
        }
        materials.get_mut(&mat_handle);
    }
}

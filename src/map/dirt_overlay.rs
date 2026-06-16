//! Transparent dirt stain overlay — one RGBA texel per world tile.

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::PlaneMeshBuilder;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::map::chunk_overlay::DIRT_OVERLAY_Y;
use crate::map::dirt::DirtMap;
use crate::map::hypermap::{ChunkCoord, LocalCoord, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::tile_field::TILE_FIELD_OVERLAY_RES;
use crate::menu::main_menu::GameState;

/// Dirt-overlay texels per world-tile edge. The dirt **field** is one scalar per
/// tile; this only upsamples the *rendered* texture so stains read as smooth fades
/// rather than 1 m blocks (see [`paint_dirt_chunk_supersampled`]). Purely visual.
const DIRT_OVERLAY_SUPERSAMPLE: u32 = 4;

/// Dirt overlay texture size per chunk edge (`TILE_FIELD_OVERLAY_RES × supersample`).
const DIRT_OVERLAY_TEXELS: u32 = TILE_FIELD_OVERLAY_RES * DIRT_OVERLAY_SUPERSAMPLE;

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

fn new_tile_field_image() -> Image {
    let size = DIRT_OVERLAY_TEXELS;
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

fn spawn_overlay_plane(
    commands: &mut Commands,
    plane_mesh: Handle<Mesh>,
    mat: Handle<StandardMaterial>,
    coord: ChunkCoord,
    name: &str,
    y: f32,
) -> Entity {
    let cx = coord.x as f32 * HYPERMAP_CHUNK_SIZE as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
    let cz = coord.y as f32 * HYPERMAP_CHUNK_SIZE as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
    commands
        .spawn((
            Name::new(format!("{name} {},{}", coord.x, coord.y)),
            Mesh3d(plane_mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(cx, y, cz),
            NotShadowCaster,
            NotShadowReceiver,
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

/// Bilinearly upsamples one chunk's tile-resolution dirt into a
/// `DIRT_OVERLAY_TEXELS²` RGBA image. Tile sample `(lx, ly)` is treated as living at
/// its tile centre; each output texel blends the four nearest tile samples. A 1-tile
/// padded border (read from `dirt` so it spans neighbouring chunks) keeps stains
/// seamless across chunk edges. Visual only — the dirt field stays per-tile.
fn paint_dirt_chunk_supersampled(data: &mut [u8], dirt: &DirtMap, coord: ChunkCoord) {
    let n = HYPERMAP_CHUNK_SIZE as usize;
    let stride = n + 2;
    let origin_x = coord.x * HYPERMAP_CHUNK_SIZE;
    let origin_y = coord.y * HYPERMAP_CHUNK_SIZE;

    // Padded tile grid: `pad[(ly+1) * stride + (lx+1)]` for lx, ly in 0..n, plus a
    // one-tile ring so bilinear taps at the chunk border read real neighbour values.
    let mut pad = vec![0.0f32; stride * stride];
    dirt.read_map().with_chunk_read(coord, |chunk| {
        for ly in 0..n {
            for lx in 0..n {
                let v = *chunk.get_local(LocalCoord::new(lx as i32, ly as i32));
                pad[(ly + 1) * stride + (lx + 1)] = v;
            }
        }
    });
    for p in 0..stride as i32 {
        let wx = origin_x + p - 1;
        pad[p as usize] = dirt.get_tile(wx, origin_y - 1);
        pad[(stride - 1) * stride + p as usize] = dirt.get_tile(wx, origin_y + n as i32);
        let wy = origin_y + p - 1;
        pad[p as usize * stride] = dirt.get_tile(origin_x - 1, wy);
        pad[p as usize * stride + (stride - 1)] = dirt.get_tile(origin_x + n as i32, wy);
    }

    let s = DIRT_OVERLAY_SUPERSAMPLE as f32;
    let res = DIRT_OVERLAY_TEXELS as usize;
    for py in 0..res {
        let fy = (py as f32 + 0.5) / s - 0.5;
        let ly0 = fy.floor();
        let ty = fy - ly0;
        let row0 = (ly0 as i32 + 1) as usize;
        let row1 = row0 + 1;
        for px in 0..res {
            let fx = (px as f32 + 0.5) / s - 0.5;
            let lx0 = fx.floor();
            let tx = fx - lx0;
            let col0 = (lx0 as i32 + 1) as usize;
            let col1 = col0 + 1;
            let v00 = pad[row0 * stride + col0];
            let v10 = pad[row0 * stride + col1];
            let v01 = pad[row1 * stride + col0];
            let v11 = pad[row1 * stride + col1];
            let top = v00 + (v10 - v00) * tx;
            let bot = v01 + (v11 - v01) * tx;
            let v = top + (bot - top) * ty;
            let idx = (py * res + px) * 4;
            data[idx..idx + 4].copy_from_slice(&dirt_to_rgba(v));
        }
    }
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
        let image_handle = images.add(new_tile_field_image());
        let mat_handle = overlay_material(image_handle.clone(), &mut materials);
        let entity = spawn_overlay_plane(
            &mut commands,
            assets.plane_mesh.clone(),
            mat_handle.clone(),
            coord,
            "Dirt overlay",
            DIRT_OVERLAY_Y,
        );
        commands.entity(entity).insert(DirtOverlayPlane);
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

    let coords: Vec<(ChunkCoord, Handle<Image>, Handle<StandardMaterial>)> = state
        .overlays
        .iter()
        .filter(|(c, _)| dirty.contains(c))
        .map(|(&c, (_, img, mat))| (c, img.clone(), mat.clone()))
        .collect();

    for (coord, img_handle, mat_handle) in coords {
        let Some(image) = images.get_mut(&img_handle) else { continue; };
        let Some(data) = image.data.as_mut() else { continue; };

        if dirt.read_map().has_chunk(coord) {
            paint_dirt_chunk_supersampled(data, &dirt, coord);
        } else {
            // No chunk ever materialized → all-transparent.
            data.fill(0);
        }
        materials.get_mut(&mat_handle);
    }
}

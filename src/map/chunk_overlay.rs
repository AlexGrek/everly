//! Per-chunk subtile-resolution overlay textures.
//!
//! # Overview
//!
//! The module manages two independent RGBA planes per visible chunk, both
//! floating above the floor and updated from the CPU each frame:
//!
//! | Layer | Y offset | Purpose |
//! |---|---|---|
//! | Generic (paths) | 0.001 m | BlackBot path + target markers (toggle: HUD "Path" / **F6**, off by default) |
//! | Occupancy | 0.002 m | Debug: colours each subtile by its passability flag |
//!
//! Both layers use `OVERLAY_RES × OVERLAY_RES = 640 × 640` texels
//! (`CHUNK_SIZE=128 × SUBTILE_COUNT=5`). One texel = one subtile = 0.2 m².
//! Format is `Rgba8UnormSrgb`; all texels start fully transparent.
//!
//! ## Writing to the generic layer
//!
//! The generic planes (and `ChunkOverlayState` entries) are only present for
//! visible chunks while `PathOverlayEnabled` is true (HUD "Path" button or F6).
//! 1. Add `Res<ChunkOverlayState>`, `ResMut<Assets<Image>>`, and
//!    `ResMut<Assets<StandardMaterial>>` to your system parameters (guard with
//!    `image_for` returning Some).
//! 2. For each chunk you want to paint, call `state.image_for(coord)` and
//!    `state.material_for(coord)` to get the handles.
//! 3. Write RGBA bytes into `image.data`, then touch the material handle.
//!    Touching the material is **required** by Bevy issue #20269 — without it,
//!    `MeshMaterial3d` silently skips the GPU texture re-upload.
//!
//! ```ignore
//! fn my_painter(
//!     state: Res<ChunkOverlayState>,
//!     mut images: ResMut<Assets<Image>>,
//!     mut materials: ResMut<Assets<StandardMaterial>>,
//! ) {
//!     let coord = ChunkCoord::new(0, 0);
//!     let (Some(img_h), Some(mat_h)) = (state.image_for(coord), state.material_for(coord))
//!     else { return; };
//!
//!     let Some(image) = images.get_mut(img_h) else { return; };
//!     if let Some(data) = image.data.as_mut() {
//!         // pixel (px, py): byte index = (py * OVERLAY_RES as usize + px) * 4
//!         let idx = (py * OVERLAY_RES as usize + px) * 4;
//!         data[idx]     = r;   // red   0–255
//!         data[idx + 1] = g;   // green 0–255
//!         data[idx + 2] = b;   // blue  0–255
//!         data[idx + 3] = a;   // alpha 0 = transparent, 255 = opaque
//!     }
//!     // Must touch the material after every write (Bevy issue #20269).
//!     materials.get_mut(mat_h);
//! }
//! ```
//!
//! ## Occupancy layer
//!
//! Reads the **read buffer** of `DynamicPassabilityMap` (last frame's snapshot
//! of static geometry + actor footprints) and colours each subtile:
//!
//! | Flags | Colour | Meaning |
//! |---|---|---|
//! | `FLAG_BLOCKED \| FLAG_CREATURE` | Red | Actor body |
//! | `FLAG_BLOCKED` (no creature) | Orange | Static wall / geometry |
//! | `FLAG_VOID` | Blue | Void floor (no ground) |
//! | `0` | Transparent | Passable |
//!
//! Updated at ~15 Hz to avoid excessive GPU upload pressure.
//! Toggled via the HUD "Occ" button or **F4** (off by default).
//!
//! One `with_chunk_read` per chunk acquires a single lock and iterates all
//! 128 × 128 tiles as pure array access — no per-subtile lock overhead.

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::PlaneMeshBuilder;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::map::hypermap::{ChunkCoord, LocalCoord, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::passability::{
    DynamicPassabilityMap, FLAG_BLOCKED, FLAG_CREATURE, FLAG_VOID, SUBTILE_COUNT,
};
use crate::menu::main_menu::GameState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Temperature overlay (see `temperature_overlay`) — lowest field layer.
pub const TEMPERATURE_OVERLAY_Y: f32 = 0.0004;
/// Dirt overlay (see `dirt_overlay`) sits just below the generic layer.
pub const DIRT_OVERLAY_Y: f32 = 0.0005;
/// Generic overlay sits directly on the floor surface.
pub const GENERIC_OVERLAY_Y: f32 = 0.001;
/// Occupancy overlay floats one step higher to avoid z-fighting with the generic layer.
pub const OCCUPANCY_OVERLAY_Y: f32 = 0.002;
/// Texels per axis: `CHUNK_SIZE × SUBTILE_COUNT = 640`.
pub const OVERLAY_RES: u32 = HYPERMAP_CHUNK_SIZE as u32 * SUBTILE_COUNT as u32;

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

/// Tracks the generic overlay plane + image handle for each visible chunk.
/// Call [`ChunkOverlayState::image_for`] to get a writable image handle.
#[derive(Resource, Default)]
pub struct ChunkOverlayState {
    overlays: HashMap<ChunkCoord, (Entity, Handle<Image>, Handle<StandardMaterial>)>,
}

impl ChunkOverlayState {
    /// Returns the [`Handle<Image>`] for the generic overlay of `coord`, or
    /// `None` if that chunk is not currently visible.
    pub fn image_for(&self, coord: ChunkCoord) -> Option<&Handle<Image>> {
        self.overlays.get(&coord).map(|(_, img, _)| img)
    }

    /// Returns the [`Handle<StandardMaterial>`] for the generic overlay of `coord`.
    ///
    /// **Must** be touched (via `materials.get_mut`) after each image write to
    /// work around <https://github.com/bevyengine/bevy/issues/20269>: `MeshMaterial3d`
    /// does not re-upload textures unless the material itself is also marked dirty.
    pub fn material_for(&self, coord: ChunkCoord) -> Option<&Handle<StandardMaterial>> {
        self.overlays.get(&coord).map(|(_, _, mat)| mat)
    }

    /// Iterates over all chunk coordinates that currently have a generic overlay.
    pub fn iter_coords(&self) -> impl Iterator<Item = ChunkCoord> + '_ {
        self.overlays.keys().copied()
    }
}

/// Set to `true` (and press **F4** at runtime) to enable the occupancy debug layer.
#[derive(Resource, Default)]
pub struct OccupancyOverlayEnabled(pub bool);

/// Set to `true` (and press **F6** at runtime) to enable BlackBot path/target rendering
/// on the generic overlay layer (cyan waypoints + purple targets). Off by default.
#[derive(Resource, Default)]
pub struct PathOverlayEnabled(pub bool);

#[derive(Resource)]
struct OccupancyOverlayState {
    overlays: HashMap<ChunkCoord, (Entity, Handle<Image>, Handle<StandardMaterial>)>,
    cadence: Timer,
}

#[derive(Resource)]
struct ChunkOverlayAssets {
    plane_mesh: Handle<Mesh>,
}

// ---------------------------------------------------------------------------
// Marker components
// ---------------------------------------------------------------------------

#[derive(Component)]
struct GenericOverlayPlane;

#[derive(Component)]
struct OccupancyOverlayPlane;

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct ChunkOverlayPlugin;

impl Plugin for ChunkOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChunkOverlayState>()
            .init_resource::<OccupancyOverlayEnabled>()
            .init_resource::<PathOverlayEnabled>()
            .add_systems(
                OnEnter(GameState::InGame),
                (reset_path_overlay_on_enter, setup_chunk_overlay).chain(),
            )
            .add_systems(
                Update,
                (
                    toggle_occupancy_overlay,
                    toggle_path_overlay,
                    sync_generic_overlays,
                    sync_occupancy_overlays,
                    update_occupancy_overlay_textures,
                )
                    .chain()
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

fn setup_chunk_overlay(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>) {
    let size = HYPERMAP_CHUNK_SIZE as f32;
    let plane_mesh = meshes.add(PlaneMeshBuilder::from_size(Vec2::splat(size)));
    commands.insert_resource(ChunkOverlayAssets { plane_mesh });
    commands.insert_resource(OccupancyOverlayState {
        overlays: HashMap::new(),
        cadence: Timer::from_seconds(1.0 / 15.0, TimerMode::Repeating),
    });
}

fn reset_path_overlay_on_enter(
    mut enabled: ResMut<PathOverlayEnabled>,
    mut state: ResMut<ChunkOverlayState>,
    mut commands: Commands,
    planes: Query<Entity, With<GenericOverlayPlane>>,
) {
    enabled.0 = false;
    for entity in planes.iter() {
        commands.entity(entity).despawn();
    }
    state.overlays.clear();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn new_overlay_image() -> Image {
    let size = OVERLAY_RES;
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
    y: f32,
    name: &str,
) -> Entity {
    let cx = coord.x as f32 * HYPERMAP_CHUNK_SIZE as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
    let cz = coord.y as f32 * HYPERMAP_CHUNK_SIZE as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
    commands
        .spawn((
            Name::new(format!("{} {},{}", name, coord.x, coord.y)),
            Mesh3d(plane_mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(cx, y, cz),
            NotShadowCaster,
            NotShadowReceiver,
        ))
        .id()
}

fn flags_to_rgba(flags: u64) -> [u8; 4] {
    if flags & FLAG_CREATURE != 0 {
        [220, 60, 60, 200]
    } else if flags & FLAG_BLOCKED != 0 {
        [210, 130, 50, 160]
    } else if flags & FLAG_VOID != 0 {
        [50, 90, 210, 110]
    } else {
        [0, 0, 0, 0]
    }
}

// ---------------------------------------------------------------------------
// Generic overlay: sync
// ---------------------------------------------------------------------------

fn sync_generic_overlays(
    mut commands: Commands,
    runtime: Res<HypermapRuntime>,
    enabled: Res<PathOverlayEnabled>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    assets: Res<ChunkOverlayAssets>,
    mut state: ResMut<ChunkOverlayState>,
    planes: Query<Entity, With<GenericOverlayPlane>>,
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
        let image_handle = images.add(new_overlay_image());
        let mat_handle = overlay_material(image_handle.clone(), &mut materials);
        let entity = spawn_overlay_plane(
            &mut commands,
            assets.plane_mesh.clone(),
            mat_handle.clone(),
            coord,
            GENERIC_OVERLAY_Y,
            "Generic overlay",
        );
        commands.entity(entity).insert(GenericOverlayPlane);
        state.overlays.insert(coord, (entity, image_handle, mat_handle));
    }
}

// ---------------------------------------------------------------------------
// Occupancy overlay: toggle + sync + writer
// ---------------------------------------------------------------------------

fn toggle_occupancy_overlay(
    keys: Res<ButtonInput<KeyCode>>,
    mut enabled: ResMut<OccupancyOverlayEnabled>,
) {
    if keys.just_pressed(KeyCode::F4) {
        enabled.0 = !enabled.0;
    }
}

fn toggle_path_overlay(
    keys: Res<ButtonInput<KeyCode>>,
    mut enabled: ResMut<PathOverlayEnabled>,
) {
    if keys.just_pressed(KeyCode::F6) {
        enabled.0 = !enabled.0;
    }
}

fn sync_occupancy_overlays(
    mut commands: Commands,
    runtime: Res<HypermapRuntime>,
    enabled: Res<OccupancyOverlayEnabled>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    assets: Res<ChunkOverlayAssets>,
    mut occ: ResMut<OccupancyOverlayState>,
    planes: Query<Entity, With<OccupancyOverlayPlane>>,
) {
    let desired: std::collections::HashSet<ChunkCoord> = if enabled.0 {
        runtime.desired_chunk_coords().into_iter().collect()
    } else {
        std::collections::HashSet::new()
    };

    let to_remove: Vec<ChunkCoord> = occ
        .overlays
        .keys()
        .filter(|c| !desired.contains(c))
        .copied()
        .collect();
    for coord in to_remove {
        if let Some((entity, _, _)) = occ.overlays.remove(&coord) {
            if planes.get(entity).is_ok() {
                commands.entity(entity).despawn();
            }
        }
    }

    for &coord in &desired {
        if occ.overlays.contains_key(&coord) {
            continue;
        }
        let image_handle = images.add(new_overlay_image());
        let mat_handle = overlay_material(image_handle.clone(), &mut materials);
        let entity = spawn_overlay_plane(
            &mut commands,
            assets.plane_mesh.clone(),
            mat_handle.clone(),
            coord,
            OCCUPANCY_OVERLAY_Y,
            "Occupancy overlay",
        );
        commands.entity(entity).insert(OccupancyOverlayPlane);
        occ.overlays.insert(coord, (entity, image_handle, mat_handle));
    }
}

fn update_occupancy_overlay_textures(
    time: Res<Time>,
    enabled: Res<OccupancyOverlayEnabled>,
    dyn_pass: Res<DynamicPassabilityMap>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut occ: ResMut<OccupancyOverlayState>,
) {
    if !enabled.0 {
        return;
    }
    occ.cadence.tick(time.delta());
    if !occ.cadence.just_finished() {
        return;
    }

    let sc = SUBTILE_COUNT;
    let res = OVERLAY_RES as usize;

    let coords: Vec<(ChunkCoord, Handle<Image>, Handle<StandardMaterial>)> = occ
        .overlays
        .iter()
        .map(|(&c, (_, img, mat))| (c, img.clone(), mat.clone()))
        .collect();

    for (coord, img_handle, mat_handle) in coords {
        let Some(image) = images.get_mut(&img_handle) else { continue; };
        let Some(data) = image.data.as_mut() else { continue; };

        let chunk_existed = dyn_pass.inner().with_chunk_read(coord, |chunk| {
            for tile_ly in 0..HYPERMAP_CHUNK_SIZE as usize {
                for tile_lx in 0..HYPERMAP_CHUNK_SIZE as usize {
                    let local = LocalCoord::new(tile_lx as i32, tile_ly as i32);
                    let tile_data = chunk.get_local(local);
                    for sy in 0..sc {
                        for sx in 0..sc {
                            let flags = tile_data.flags_at(sy, sx);
                            let px = tile_lx * sc + sx;
                            let py = tile_ly * sc + sy;
                            let idx = (py * res + px) * 4;
                            let color = flags_to_rgba(flags);
                            data[idx..idx + 4].copy_from_slice(&color);
                        }
                    }
                }
            }
        }).is_some();

        // Passability chunk absent means no stamps ever landed here → all passable → transparent.
        if !chunk_existed {
            data.fill(0);
        }
        // Workaround for https://github.com/bevyengine/bevy/issues/20269.
        materials.get_mut(&mat_handle);
    }
}

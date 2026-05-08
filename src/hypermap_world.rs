//! Runtime renderer for Hypermap chunks around the strategy camera.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::PlaneMeshBuilder;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use bevy_water::water::material::{StandardWaterMaterial, WaterMaterial};
use bevy_water::{setup_water, WaterQuality, WaterSettings, WaterTile, WaterTiles, WaveDirection};
use futures_lite::future;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::camera::StrategyCamera;
use crate::hypermap::{world_to_chunk_local, ChunkCoord, Hypermap, HypermapChunk, LocalCoord, HYPERMAP_CHUNK_SIZE};
use crate::world_map::{CellType, WallSide, WorldMapFloor, WATER_SURFACE_Y};

const WALL_THICKNESS: f32 = 0.2;
const WALL_HEIGHT: f32 = 1.0;
const WALL_PROBABILITY: f32 = 0.22;
const WORLD_MAP_FILE_PATH: &str = "world_map.txt";
const CENTER_CHUNK: ChunkCoord = ChunkCoord { x: 0, y: 0 };
const DEAD_ZONE_SIZE: u8 = 20;

#[derive(Component)]
struct RenderedChunkRoot;

#[derive(Component)]
struct RenderedChunkWater;

#[derive(Resource)]
struct HypermapRuntime {
    map: Arc<Hypermap<CellType>>,
    desired_chunks: HashSet<ChunkCoord>,
    chunk_roots: HashMap<ChunkCoord, Entity>,
    water_tiles: HashMap<ChunkCoord, Entity>,
    pending_renders: HashMap<ChunkCoord, Task<PreparedChunkRender>>,
    despawn_queue: VecDeque<ChunkCoord>,
    last_center_chunk: Option<ChunkCoord>,
    active_side: HorizontalSide,
    water_root: Entity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HorizontalSide {
    West,
    East,
}

struct PreparedChunkRender {
    has_void: bool,
    cells: Vec<(u8, u8, CellType)>,
}

#[derive(Resource)]
struct HypermapRenderAssets {
    floor_mesh: Handle<Mesh>,
    wall_ns_mesh: Handle<Mesh>,
    wall_ew_mesh: Handle<Mesh>,
    water_mesh: Handle<Mesh>,
    road_material: Handle<StandardMaterial>,
    wall_material: Handle<StandardMaterial>,
    water_material: Handle<StandardWaterMaterial>,
}

pub struct HypermapWorldPlugin;

impl Plugin for HypermapWorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_hypermap_runtime)
            .add_systems(Startup, setup_hypermap_assets.after(setup_hypermap_runtime).after(setup_water))
            .add_systems(
                Update,
                (
                    update_visible_hypermap_chunks,
                    poll_chunk_render_tasks,
                    process_chunk_despawns,
                ),
            );
    }
}

fn setup_hypermap_runtime(mut commands: Commands) {
    let water_root = commands
        .spawn((Name::new("Hypermap water"), WaterTiles))
        .id();
    commands.insert_resource(HypermapRuntime {
        map: Arc::new(Hypermap::new(CellType::Road)),
        desired_chunks: HashSet::new(),
        chunk_roots: HashMap::new(),
        water_tiles: HashMap::new(),
        pending_renders: HashMap::new(),
        despawn_queue: VecDeque::new(),
        last_center_chunk: None,
        active_side: HorizontalSide::East,
        water_root,
    });
}

fn setup_hypermap_assets(
    mut commands: Commands,
    settings: Res<WaterSettings>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut water_materials: ResMut<Assets<StandardWaterMaterial>>,
) {
    let floor_mesh = meshes.add(Plane3d::default().mesh().size(1.0, 1.0));
    let wall_ns_mesh = meshes.add(Cuboid::new(1.0, WALL_HEIGHT, WALL_THICKNESS));
    let wall_ew_mesh = meshes.add(Cuboid::new(WALL_THICKNESS, WALL_HEIGHT, 1.0));
    let chunk_size = HYPERMAP_CHUNK_SIZE as f32;
    let water_mesh = meshes.add(PlaneMeshBuilder::from_size(Vec2::new(chunk_size, chunk_size)));

    let road_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.36, 0.36, 0.38),
        perceptual_roughness: 0.98,
        metallic: 0.0,
        ..default()
    });
    let wall_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.78, 0.79, 0.82),
        perceptual_roughness: 0.72,
        metallic: 0.02,
        ..default()
    });
    let normalized_dir = settings.wave_direction.normalize_or_zero();
    let coord_scale = Vec2::splat(chunk_size);
    let coord_offset = Vec2::splat(-chunk_size * 0.5);
    let water_material = water_materials.add(StandardWaterMaterial {
        base: StandardMaterial {
            base_color: settings.base_color,
            alpha_mode: settings.alpha_mode,
            perceptual_roughness: 0.22,
            ..default()
        },
        extension: WaterMaterial {
            amplitude: settings.amplitude,
            clarity: settings.clarity,
            deep_color: settings.deep_color,
            shallow_color: settings.shallow_color,
            edge_color: settings.edge_color,
            edge_scale: settings.edge_scale,
            coord_offset,
            coord_scale,
            wave_dir_a: normalized_dir,
            wave_dir_b: normalized_dir,
            wave_blend: 1.0,
            quality: settings.water_quality.into(),
        },
    });

    commands.insert_resource(HypermapRenderAssets {
        floor_mesh,
        wall_ns_mesh,
        wall_ew_mesh,
        water_mesh,
        road_material,
        wall_material,
        water_material,
    });
}

fn update_visible_hypermap_chunks(
    mut runtime: ResMut<HypermapRuntime>,
    cameras: Query<&StrategyCamera>,
) {
    let Ok(camera) = cameras.single() else {
        return;
    };
    let (center, local) =
        world_to_chunk_local(camera.focus.x.floor() as i32, camera.focus.z.floor() as i32);

    let center_changed = runtime.last_center_chunk != Some(center);
    if center_changed {
        runtime.last_center_chunk = Some(center);
    }

    let in_dead_zone = is_in_center_dead_zone(local);
    if !center_changed && in_dead_zone {
        return;
    }

    runtime.active_side = select_horizontal_side(local.x, runtime.active_side);
    let target_chunks = target_chunks_for(center, runtime.active_side);
    if target_chunks == runtime.desired_chunks {
        return;
    }

    for obsolete in runtime
        .desired_chunks
        .difference(&target_chunks)
        .copied()
        .collect::<Vec<_>>()
    {
        if !runtime.despawn_queue.contains(&obsolete) {
            runtime.despawn_queue.push_back(obsolete);
        }
    }

    let task_pool = AsyncComputeTaskPool::get();
    for chunk in target_chunks
        .difference(&runtime.desired_chunks)
        .copied()
        .collect::<Vec<_>>()
    {
        ensure_chunk_generated(&runtime.map, chunk);
        if runtime.chunk_roots.contains_key(&chunk) || runtime.pending_renders.contains_key(&chunk) {
            continue;
        }
        let Some(snapshot) = runtime
            .map
            .with_chunk_read(chunk, snapshot_chunk_cells)
        else {
            continue;
        };
        let task = task_pool.spawn(async move { build_chunk_render_data(snapshot) });
        runtime.pending_renders.insert(chunk, task);
    }

    runtime.desired_chunks = target_chunks;
}

fn poll_chunk_render_tasks(
    mut commands: Commands,
    settings: Res<WaterSettings>,
    assets: Res<HypermapRenderAssets>,
    mut runtime: ResMut<HypermapRuntime>,
    roots: Query<Entity, With<RenderedChunkRoot>>,
    waters: Query<Entity, With<RenderedChunkWater>>,
) {
    let mut completed = Vec::new();
    for (coord, task) in &mut runtime.pending_renders {
        if let Some(prepared) = future::block_on(future::poll_once(task)) {
            completed.push((*coord, prepared));
        }
    }

    for (coord, prepared) in completed {
        runtime.pending_renders.remove(&coord);

        if !runtime.desired_chunks.contains(&coord) {
            continue;
        }
        if let Some(existing) = runtime.chunk_roots.get(&coord).copied() {
            if roots.get(existing).is_ok() {
                commands.entity(existing).despawn();
            }
        }
        if let Some(existing) = runtime.water_tiles.get(&coord).copied() {
            if waters.get(existing).is_ok() {
                commands.entity(existing).despawn();
            }
        }

        let chunk_origin_x = coord.x * HYPERMAP_CHUNK_SIZE;
        let chunk_origin_y = coord.y * HYPERMAP_CHUNK_SIZE;
        let chunk_root = commands
            .spawn((
                Name::new(format!("Hypermap chunk {},{}", coord.x, coord.y)),
                RenderedChunkRoot,
                Transform::default(),
            ))
            .id();
        commands
            .entity(chunk_root)
            .with_children(|parent| spawn_chunk_tiles(parent, &prepared, chunk_origin_x, chunk_origin_y, &assets));
        runtime.chunk_roots.insert(coord, chunk_root);

        if prepared.has_void {
            let chunk_center_x = chunk_origin_x as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
            let chunk_center_z = chunk_origin_y as f32 + HYPERMAP_CHUNK_SIZE as f32 * 0.5;
            let water_tile = commands
                .spawn((
                    Name::new(format!("Hypermap water {},{}", coord.x, coord.y)),
                    RenderedChunkWater,
                    WaterTile {
                        offset: Vec2::new(chunk_origin_x as f32, chunk_origin_y as f32),
                    },
                    Mesh3d(assets.water_mesh.clone()),
                    MeshMaterial3d(assets.water_material.clone()),
                    WaveDirection::with_duration(
                        settings.wave_direction,
                        settings.wave_direction_blend_duration,
                    ),
                    Transform::from_xyz(chunk_center_x, WATER_SURFACE_Y, chunk_center_z),
                    NotShadowCaster,
                ))
                .id();
            if matches!(settings.water_quality, WaterQuality::Basic | WaterQuality::Medium) {
                commands.entity(water_tile).insert(NotShadowReceiver);
            }
            commands.entity(runtime.water_root).add_child(water_tile);
            runtime.water_tiles.insert(coord, water_tile);
        }
    }
}

fn process_chunk_despawns(
    mut commands: Commands,
    mut runtime: ResMut<HypermapRuntime>,
    roots: Query<Entity, With<RenderedChunkRoot>>,
    waters: Query<Entity, With<RenderedChunkWater>>,
) {
    let Some(coord) = runtime.despawn_queue.pop_front() else {
        return;
    };

    if runtime.desired_chunks.contains(&coord) {
        return;
    }

    if let Some(entity) = runtime.chunk_roots.remove(&coord) {
        if roots.get(entity).is_ok() {
            commands.entity(entity).despawn();
        }
    }
    if let Some(entity) = runtime.water_tiles.remove(&coord) {
        if waters.get(entity).is_ok() {
            commands.entity(entity).despawn();
        }
    }
}

fn ensure_chunk_generated(map: &Hypermap<CellType>, coord: ChunkCoord) {
    if map.has_chunk(coord) {
        return;
    }

    map.with_chunk_write(coord, |chunk| {
        fill_chunk_random(chunk, coord);
        if coord == CENTER_CHUNK {
            if let Err(err) = apply_center_map_overlay(chunk) {
                warn!("failed to apply `{WORLD_MAP_FILE_PATH}` to center chunk: {err}");
            }
        }
    });
}

fn fill_chunk_random(chunk: &mut HypermapChunk<CellType>, coord: ChunkCoord) {
    let seed = hash_chunk_seed(coord);
    let mut rng = StdRng::seed_from_u64(seed);
    for y in 0..HYPERMAP_CHUNK_SIZE as u8 {
        for x in 0..HYPERMAP_CHUNK_SIZE as u8 {
            let tile = if rng.gen_bool(WALL_PROBABILITY as f64) {
                CellType::Wall(match rng.gen_range(0..4) {
                    0 => WallSide::North,
                    1 => WallSide::South,
                    2 => WallSide::East,
                    _ => WallSide::West,
                })
            } else {
                CellType::Road
            };
            chunk.set_local(LocalCoord::new(x, y), tile);
        }
    }
}

fn spawn_chunk_tiles(
    parent: &mut ChildSpawnerCommands,
    prepared: &PreparedChunkRender,
    origin_x: i32,
    origin_y: i32,
    assets: &HypermapRenderAssets,
) {
    for &(x, y, cell_type) in &prepared.cells {
        let world_x = origin_x as f32 + x as f32 + 0.5;
        let world_z = origin_y as f32 + y as f32 + 0.5;

        parent.spawn((
            Name::new(format!("Tile {},{}", origin_x + x as i32, origin_y + y as i32)),
            Mesh3d(assets.floor_mesh.clone()),
            MeshMaterial3d(assets.road_material.clone()),
            Transform::from_xyz(world_x, 0.0, world_z),
        ));

        if let CellType::Wall(side) = cell_type {
            let (mesh, offset) = match side {
                WallSide::North => (
                    assets.wall_ns_mesh.clone(),
                    Vec3::new(0.0, WALL_HEIGHT * 0.5, -0.4),
                ),
                WallSide::South => (
                    assets.wall_ns_mesh.clone(),
                    Vec3::new(0.0, WALL_HEIGHT * 0.5, 0.4),
                ),
                WallSide::East => (
                    assets.wall_ew_mesh.clone(),
                    Vec3::new(0.4, WALL_HEIGHT * 0.5, 0.0),
                ),
                WallSide::West => (
                    assets.wall_ew_mesh.clone(),
                    Vec3::new(-0.4, WALL_HEIGHT * 0.5, 0.0),
                ),
            };

            parent.spawn((
                Name::new(format!("Wall {},{}", origin_x + x as i32, origin_y + y as i32)),
                Mesh3d(mesh),
                MeshMaterial3d(assets.wall_material.clone()),
                Transform::from_translation(Vec3::new(world_x, 0.0, world_z) + offset),
            ));
        }
    }
}

fn hash_chunk_seed(coord: ChunkCoord) -> u64 {
    let x = coord.x as u64;
    let y = coord.y as u64;
    x.wrapping_mul(0x9E37_79B9_85F3_7D87) ^ y.wrapping_mul(0xC2B2_AE3D_27D4_F4F5) ^ 0xA32D_192E_2AA3_4C13
}

fn snapshot_chunk_cells(chunk: &HypermapChunk<CellType>) -> Vec<CellType> {
    let mut cells = Vec::with_capacity((HYPERMAP_CHUNK_SIZE * HYPERMAP_CHUNK_SIZE) as usize);
    for y in 0..HYPERMAP_CHUNK_SIZE as u8 {
        for x in 0..HYPERMAP_CHUNK_SIZE as u8 {
            cells.push(*chunk.get_local(LocalCoord::new(x, y)));
        }
    }
    cells
}

fn build_chunk_render_data(cells: Vec<CellType>) -> PreparedChunkRender {
    let mut has_void = false;
    let mut render_cells = Vec::new();
    for (i, cell_type) in cells.into_iter().enumerate() {
        let x = (i % HYPERMAP_CHUNK_SIZE as usize) as u8;
        let y = (i / HYPERMAP_CHUNK_SIZE as usize) as u8;
        if cell_type == CellType::Void {
            has_void = true;
            continue;
        }
        render_cells.push((x, y, cell_type));
    }
    PreparedChunkRender { has_void, cells: render_cells }
}

fn target_chunks_for(center: ChunkCoord, side: HorizontalSide) -> HashSet<ChunkCoord> {
    let side_chunk = match side {
        HorizontalSide::West => ChunkCoord::new(center.x - 1, center.y),
        HorizontalSide::East => ChunkCoord::new(center.x + 1, center.y),
    };
    let north_of_side = ChunkCoord::new(side_chunk.x, side_chunk.y - 1);
    HashSet::from([
        center,
        ChunkCoord::new(center.x, center.y - 1), // north
        side_chunk,
        north_of_side,
    ])
}

fn select_horizontal_side(local_x: u8, current: HorizontalSide) -> HorizontalSide {
    let distance_to_west = local_x as i32;
    let distance_to_east = (HYPERMAP_CHUNK_SIZE - 1 - local_x as i32).max(0);
    if distance_to_west < distance_to_east {
        HorizontalSide::West
    } else if distance_to_east < distance_to_west {
        HorizontalSide::East
    } else {
        current
    }
}

fn is_in_center_dead_zone(local: LocalCoord) -> bool {
    let center = (HYPERMAP_CHUNK_SIZE / 2) as i32;
    let half = (DEAD_ZONE_SIZE as i32) / 2;
    let min = center - half;
    let max = center + half;
    let lx = local.x as i32;
    let ly = local.y as i32;
    lx >= min && lx < max && ly >= min && ly < max
}

fn apply_center_map_overlay(chunk: &mut HypermapChunk<CellType>) -> Result<(), String> {
    let text = std::fs::read_to_string(Path::new(WORLD_MAP_FILE_PATH))
        .map_err(|err| format!("read error: {err}"))?;
    let map = WorldMapFloor::from_ascii(&text).map_err(|err| format!("parse error: {err}"))?;

    let chunk_size = HYPERMAP_CHUNK_SIZE as usize;
    let start_x = (chunk_size as i32 - map.width() as i32) / 2;
    let start_y = (chunk_size as i32 - map.height() as i32) / 2;

    for map_y in 0..map.height() {
        for map_x in 0..map.width() {
            let local_x = start_x + map_x as i32;
            let local_y = start_y + map_y as i32;
            if local_x < 0
                || local_y < 0
                || local_x >= HYPERMAP_CHUNK_SIZE
                || local_y >= HYPERMAP_CHUNK_SIZE
            {
                continue;
            }

            let Some(cell) = map.get(map_x, map_y) else {
                continue;
            };
            chunk.set_local(
                LocalCoord::new(local_x as u8, local_y as u8),
                cell.get_cell_type(),
            );
        }
    }

    Ok(())
}

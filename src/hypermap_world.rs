//! Runtime renderer for Hypermap chunks around the strategy camera.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use bevy::asset::RenderAssetUsages;
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::Indices;
use bevy::mesh::PlaneMeshBuilder;
use bevy::picking::prelude::Pickable;
use bevy::prelude::*;
use bevy::render::render_resource::PrimitiveTopology;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use bevy_water::water::material::{StandardWaterMaterial, WaterMaterial};
use bevy_water::{setup_water, WaterQuality, WaterSettings, WaterTile, WaterTiles, WaveDirection};
use futures_lite::future;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::camera::StrategyCamera;
use crate::floor_level::{ActiveFloorLevel, HYPERMAP_FLOOR_HEIGHT, HYPERMAP_WALL_HEIGHT};
use crate::hypermap::{
    world_to_chunk_local, ChunkCoord, Hypermap, HypermapChunk, LocalCoord, HYPERMAP_CHUNK_SIZE,
    HYPERMAP_FLOOR_COUNT,
};
use crate::map_selection::{floor_grid_click, MapSelectionRoadMaterial};
use crate::world_map::{
    for_each_wall_segment, CellType, WallMask, WorldMapFloor, WALL_THICKNESS, MASK_EAST,
    MASK_NORTH, MASK_SOUTH, MASK_WEST, WATER_SURFACE_Y,
};

/// Wall slab height — [`HYPERMAP_WALL_HEIGHT`] (floor plane spacing is slightly larger).
const WALL_HEIGHT: f32 = HYPERMAP_WALL_HEIGHT;
/// Void margin around each generated chunk (water / shoreline).
const PROCEDURAL_VOID_MARGIN: u8 = 2;
/// Alley width between house plots in procedural chunks.
const PROCEDURAL_ALLEY: u8 = 2;
const WORLD_MAP_FILE_PATH: &str = "world_map.txt";
const WORLD_MAP_FLOOR1_FILE_PATH: &str = "world_map_floor1.txt";
const CENTER_CHUNK: ChunkCoord = ChunkCoord { x: 0, y: 0 };
const DEAD_ZONE_SIZE: u8 = 20;
const RENDER_TICK_HZ: f32 = 30.0;
const MAX_SPAWNS_PER_TICK: usize = 1;
const MAX_DESPAWNS_PER_TICK: usize = 1;

#[derive(Component)]
struct RenderedChunkRoot;

#[derive(Component)]
struct RenderedChunkWater;

#[derive(Component)]
struct RenderedChunkFloor0Road;

#[derive(Component)]
struct RenderedChunkFloor0Walls;

#[derive(Component)]
struct RenderedChunkUpperRoad;

#[derive(Component)]
struct RenderedChunkUpperWalls;

/// Upper-layer mesh entities (refreshed when HUD floor changes). Floor 0 meshes stay on the chunk root.
#[derive(Clone, Copy)]
struct ChunkUpperMeshEntities {
    road: Entity,
    walls: Entity,
}

/// Chunks that must be re-baked after [`Hypermap`](crate::hypermap::Hypermap) cell edits (drained in [`render_chunks_30fps`]).
#[derive(Resource, Default, Debug)]
pub struct HypermapChunkRemeshQueue(pub HashSet<ChunkCoord>);

/// Queues a chunk for mesh rebuild after editing tile `(world_x, world_z)` (ground plane indices).
pub fn queue_hypermap_chunk_remesh(queue: &mut HypermapChunkRemeshQueue, world_x: i32, world_z: i32) {
    let (coord, _) = world_to_chunk_local(world_x, world_z);
    queue.0.insert(coord);
}

#[derive(Resource)]
pub(crate) struct HypermapRuntime {
    pub(crate) map: Arc<Hypermap<CellType>>,
    desired_chunks: HashSet<ChunkCoord>,
    chunk_roots: HashMap<ChunkCoord, Entity>,
    chunk_upper_meshes: HashMap<ChunkCoord, ChunkUpperMeshEntities>,
    water_tiles: HashMap<ChunkCoord, Entity>,
    pending_renders: HashMap<ChunkCoord, Task<PreparedChunkRender>>,
    ready_renders: VecDeque<(ChunkCoord, PreparedChunkRender)>,
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
    /// True when ground floor (`0`) has void in this chunk (drives shoreline water for that chunk).
    has_void_floor0_water: bool,
    /// Non-void ground floor tiles (never depends on HUD floor; mesh is baked once per chunk).
    floor0_cells: Vec<(u8, u8, CellType)>,
    /// Upper-floor tiles for the active level at **bake** time (`floor > 0 && floor >= active_floor`).
    upper_cells: Vec<(u8, u8, u8, CellType)>,
}

#[derive(Resource)]
struct ChunkRenderCadence {
    timer: Timer,
}

#[derive(Resource)]
struct HypermapRenderAssets {
    water_mesh: Handle<Mesh>,
    /// Invisible placeholder mesh for upper layers when empty.
    empty_mesh: Handle<Mesh>,
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
                    refresh_chunk_upper_layers_on_floor_change,
                    update_visible_hypermap_chunks,
                    render_chunks_30fps,
                )
                    .chain(),
            );
    }
}

fn setup_hypermap_runtime(mut commands: Commands) {
    let water_root = commands
        .spawn((Name::new("Hypermap water"), WaterTiles))
        .id();
    commands.init_resource::<HypermapChunkRemeshQueue>();
    commands.insert_resource(HypermapRuntime {
        map: Arc::new(Hypermap::new(CellType::Road)),
        desired_chunks: HashSet::new(),
        chunk_roots: HashMap::new(),
        chunk_upper_meshes: HashMap::new(),
        water_tiles: HashMap::new(),
        pending_renders: HashMap::new(),
        ready_renders: VecDeque::new(),
        despawn_queue: VecDeque::new(),
        last_center_chunk: None,
        active_side: HorizontalSide::East,
        water_root,
    });
    commands.insert_resource(ChunkRenderCadence {
        timer: Timer::from_seconds(1.0 / RENDER_TICK_HZ, TimerMode::Repeating),
    });
}

fn setup_hypermap_assets(
    mut commands: Commands,
    settings: Res<WaterSettings>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut water_materials: ResMut<Assets<StandardWaterMaterial>>,
) {
    let chunk_size = HYPERMAP_CHUNK_SIZE as f32;
    let water_mesh = meshes.add(PlaneMeshBuilder::from_size(Vec2::new(chunk_size, chunk_size)));
    let empty_mesh = meshes.add(PlaneMeshBuilder::from_size(Vec2::splat(0.02)));

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
        cull_mode: None,
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
        water_mesh,
        empty_mesh,
        road_material: road_material.clone(),
        wall_material,
        water_material,
    });
    commands.insert_resource(MapSelectionRoadMaterial(road_material));
}

fn refresh_chunk_upper_layers_on_floor_change(
    floor: Res<ActiveFloorLevel>,
    mut prev_floor: Local<Option<u8>>,
    runtime: Res<HypermapRuntime>,
    mut meshes: ResMut<Assets<Mesh>>,
    assets: Res<HypermapRenderAssets>,
    mut commands: Commands,
) {
    let current = floor.0;
    if prev_floor.as_ref() == Some(&current) {
        return;
    }
    let had_previous = prev_floor.is_some();
    *prev_floor = Some(current);
    if !had_previous {
        return;
    }

    let map = runtime.map.clone();
    let chunk_list: Vec<(ChunkCoord, ChunkUpperMeshEntities)> =
        runtime.chunk_upper_meshes.iter().map(|(&c, &e)| (c, e)).collect();

    for (coord, mesh_ids) in chunk_list {
        let Some(snapshot) = map.with_chunk_read(coord, clone_chunk_for_render_start) else {
            continue;
        };
        let (_, _, upper_cells) = partition_chunk_cells_from_vec(snapshot, current);
        let ox = coord.x * HYPERMAP_CHUNK_SIZE;
        let oy = coord.y * HYPERMAP_CHUNK_SIZE;

        if let Some(mesh) = build_upper_road_mesh(&upper_cells, ox, oy) {
            let h = meshes.add(mesh);
            commands.entity(mesh_ids.road).insert((
                Mesh3d(h),
                MeshMaterial3d(assets.road_material.clone()),
                Visibility::Inherited,
                Pickable::default(),
            ));
        } else {
            commands.entity(mesh_ids.road).insert((
                Mesh3d(assets.empty_mesh.clone()),
                MeshMaterial3d(assets.road_material.clone()),
                Visibility::Hidden,
                Pickable::IGNORE,
            ));
        }

        if let Some(mesh) = build_upper_wall_mesh(&upper_cells, ox, oy) {
            let h = meshes.add(mesh);
            commands.entity(mesh_ids.walls).insert((
                Mesh3d(h),
                MeshMaterial3d(assets.wall_material.clone()),
                Visibility::Inherited,
            ));
        } else {
            commands.entity(mesh_ids.walls).insert((
                Mesh3d(assets.empty_mesh.clone()),
                MeshMaterial3d(assets.wall_material.clone()),
                Visibility::Hidden,
                Pickable::IGNORE,
            ));
        }
    }
}

fn update_visible_hypermap_chunks(
    mut runtime: ResMut<HypermapRuntime>,
    cameras: Query<&StrategyCamera>,
    active_floor: Res<ActiveFloorLevel>,
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
    let floor_for_render = active_floor.0;
    for chunk in target_chunks.iter().copied() {
        ensure_chunk_generated(&runtime.map, chunk);
        if runtime.chunk_roots.contains_key(&chunk) || runtime.pending_renders.contains_key(&chunk) {
            continue;
        }
        let Some(snapshot) = runtime
            .map
            .with_chunk_read(chunk, clone_chunk_for_render_start)
        else {
            continue;
        };
        let task = task_pool.spawn(async move { build_chunk_render_data(snapshot, floor_for_render) });
        runtime.pending_renders.insert(chunk, task);
    }

    runtime.desired_chunks = target_chunks;
}

fn render_chunks_30fps(
    mut commands: Commands,
    time: Res<Time>,
    mut cadence: ResMut<ChunkRenderCadence>,
    settings: Res<WaterSettings>,
    mut meshes: ResMut<Assets<Mesh>>,
    assets: Res<HypermapRenderAssets>,
    mut runtime: ResMut<HypermapRuntime>,
    mut remesh: ResMut<HypermapChunkRemeshQueue>,
    active_floor: Res<ActiveFloorLevel>,
    roots: Query<Entity, With<RenderedChunkRoot>>,
    waters: Query<Entity, With<RenderedChunkWater>>,
) {
    let remesh_coords: Vec<ChunkCoord> = remesh.0.drain().collect();
    for coord in remesh_coords {
        if !runtime.desired_chunks.contains(&coord) {
            continue;
        }
        ensure_chunk_generated(&runtime.map, coord);
        if runtime.chunk_roots.contains_key(&coord) {
            despawn_chunk_entities(&mut commands, &mut runtime, &roots, &waters, coord);
        }
        runtime.pending_renders.remove(&coord);
        runtime
            .ready_renders
            .retain(|(c, _)| *c != coord);
        let task_pool = AsyncComputeTaskPool::get();
        let floor_for_render = active_floor.0;
        if let Some(snapshot) = runtime
            .map
            .with_chunk_read(coord, clone_chunk_for_render_start)
        {
            let task = task_pool.spawn(async move {
                build_chunk_render_data(snapshot, floor_for_render)
            });
            runtime.pending_renders.insert(coord, task);
        }
    }

    let mut completed = Vec::new();
    for (coord, task) in &mut runtime.pending_renders {
        if let Some(prepared) = future::block_on(future::poll_once(task)) {
            completed.push((*coord, prepared));
        }
    }

    for (coord, prepared) in completed {
        runtime.pending_renders.remove(&coord);
        runtime.ready_renders.push_back((coord, prepared));
    }

    cadence.timer.tick(time.delta());
    if !cadence.timer.just_finished() {
        return;
    }

    for _ in 0..MAX_DESPAWNS_PER_TICK {
        let Some(coord) = runtime.despawn_queue.pop_front() else {
            break;
        };
        if runtime.desired_chunks.contains(&coord) {
            continue;
        }
        despawn_chunk_entities(&mut commands, &mut runtime, &roots, &waters, coord);
    }

    for _ in 0..MAX_SPAWNS_PER_TICK {
        let Some((coord, prepared)) = runtime.ready_renders.pop_front() else {
            break;
        };

        if !runtime.desired_chunks.contains(&coord) {
            continue;
        }
        despawn_chunk_entities(&mut commands, &mut runtime, &roots, &waters, coord);

        let chunk_origin_x = coord.x * HYPERMAP_CHUNK_SIZE;
        let chunk_origin_y = coord.y * HYPERMAP_CHUNK_SIZE;
        let chunk_root = commands
            .spawn((
                Name::new(format!("Hypermap chunk {},{}", coord.x, coord.y)),
                RenderedChunkRoot,
                Transform::default(),
                Visibility::default(),
            ))
            .id();
        let mesh_entities = spawn_chunk_meshes(
            &mut commands,
            chunk_root,
            &mut meshes,
            &prepared,
            chunk_origin_x,
            chunk_origin_y,
            &assets,
        );
        runtime.chunk_roots.insert(coord, chunk_root);
        runtime.chunk_upper_meshes.insert(coord, mesh_entities);

        if prepared.has_void_floor0_water {
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

fn despawn_chunk_entities(
    commands: &mut Commands,
    runtime: &mut HypermapRuntime,
    roots: &Query<Entity, With<RenderedChunkRoot>>,
    waters: &Query<Entity, With<RenderedChunkWater>>,
    coord: ChunkCoord,
) {
    if let Some(entity) = runtime.chunk_roots.remove(&coord) {
        if roots.get(entity).is_ok() {
            commands.entity(entity).despawn();
        }
    }
    runtime.chunk_upper_meshes.remove(&coord);
    if let Some(entity) = runtime.water_tiles.remove(&coord) {
        if waters.get(entity).is_ok() {
            commands.entity(entity).despawn();
        }
    }
}

pub(crate) fn ensure_chunk_generated(map: &Hypermap<CellType>, coord: ChunkCoord) {
    if map.has_chunk(coord) {
        return;
    }

    map.with_chunk_write(coord, |chunk| {
        fill_chunk_random(chunk, coord);
        if coord == CENTER_CHUNK {
            if let Err(err) = apply_world_map_file_to_floor(chunk, 0, WORLD_MAP_FILE_PATH) {
                warn!("failed to apply `{WORLD_MAP_FILE_PATH}` to center chunk floor 0: {err}");
            }
            if let Err(err) = apply_world_map_file_to_floor(chunk, 1, WORLD_MAP_FLOOR1_FILE_PATH) {
                warn!("failed to apply `{WORLD_MAP_FLOOR1_FILE_PATH}` to center chunk floor 1: {err}");
            }
        }
    });
}

fn fill_chunk_random(chunk: &mut HypermapChunk<CellType>, coord: ChunkCoord) {
    fill_chunk_neighborhood(chunk, coord);
    clear_upper_floors_to_void(chunk);
}

/// Procedural authoring only fills ground floor; upper levels stay open void until edited.
fn clear_upper_floors_to_void(chunk: &mut HypermapChunk<CellType>) {
    let sz = HYPERMAP_CHUNK_SIZE as u8;
    for y in 0..sz {
        for x in 0..sz {
            let local = LocalCoord::new(x, y);
            for floor in 1..HYPERMAP_FLOOR_COUNT as u8 {
                chunk.set_local_floor(local, floor, CellType::Void);
            }
        }
    }
}

/// Seeded procedural neighborhood: void ring, streets, and rectangular houses with
/// internal partitions and door gaps (missing wall segments on `__` cells).
fn fill_chunk_neighborhood(chunk: &mut HypermapChunk<CellType>, coord: ChunkCoord) {
    let seed = hash_chunk_seed(coord) ^ 0xBEE5_00D0_C0BB_1Eu64;
    let mut rng = StdRng::seed_from_u64(seed);
    let sz = HYPERMAP_CHUNK_SIZE as u8;
    let m = PROCEDURAL_VOID_MARGIN;

    for y in 0..sz {
        for x in 0..sz {
            let tile = if x < m || y < m || x >= sz - m || y >= sz - m {
                CellType::Void
            } else {
                CellType::Road
            };
            chunk.set_local(LocalCoord::new(x, y), tile);
        }
    }

    let x1 = sz - m;
    let y1 = sz - m;
    let mut ox = m;
    while ox + 10 < x1 {
        let plot_w = rng.gen_range(14..=20);
        let mut oy = m;
        while oy + 10 < y1 {
            let plot_h = rng.gen_range(12..=22);
            if ox + plot_w > x1 || oy + plot_h > y1 {
                break;
            }
            if rng.gen_bool(0.88) {
                stamp_house(chunk, &mut rng, ox, oy, plot_w, plot_h);
            }
            oy += plot_h + PROCEDURAL_ALLEY;
        }
        ox += plot_w + PROCEDURAL_ALLEY;
    }
}

#[derive(Clone, Copy)]
struct Plot {
    x0: u8,
    y0: u8,
    w: u8,
    h: u8,
}

impl Plot {
    fn x1(self) -> u8 {
        self.x0 + self.w - 1
    }

    fn y1(self) -> u8 {
        self.y0 + self.h - 1
    }
}

fn stamp_house(chunk: &mut HypermapChunk<CellType>, rng: &mut StdRng, x0: u8, y0: u8, w: u8, h: u8) {
    let p = Plot { x0, y0, w, h };
    if p.w < 8 || p.h < 8 {
        return;
    }

    for y in p.y0..=p.y1() {
        for x in p.x0..=p.x1() {
            set_cell(chunk, x, y, CellType::Road);
        }
    }
    stamp_rect_perimeter(chunk, p);

    match rng.gen_range(0u8..4) {
        0 => stamp_two_rooms_vertical(chunk, rng, p),
        1 => stamp_two_rooms_horizontal(chunk, rng, p),
        2 => stamp_three_rooms_el(chunk, rng, p),
        _ => stamp_four_rooms_quad(chunk, rng, p),
    }
}

fn set_cell(chunk: &mut HypermapChunk<CellType>, x: u8, y: u8, t: CellType) {
    chunk.set_local(LocalCoord::new(x, y), t);
}

fn wall_cell(bits: u8) -> CellType {
    CellType::Wall(WallMask::from_bits(bits).expect("wall bitmask"))
}

/// Clears one edge bit on a wall cell (opening onto road / interior). If no
/// wall bits remain, the cell becomes [`CellType::Road`].
fn clear_wall_edge_bit(chunk: &mut HypermapChunk<CellType>, x: u8, y: u8, bit: u8) {
    let local = LocalCoord::new(x, y);
    let CellType::Wall(mask) = *chunk.get_local(local) else {
        return;
    };
    let new_bits = mask.bits() & !bit;
    let next = if new_bits == 0 {
        CellType::Road
    } else {
        wall_cell(new_bits)
    };
    chunk.set_local(local, next);
}

fn stamp_rect_perimeter(chunk: &mut HypermapChunk<CellType>, p: Plot) {
    let x0 = p.x0;
    let y0 = p.y0;
    let x1 = p.x1();
    let y1 = p.y1();

    set_cell(chunk, x0, y0, wall_cell(MASK_NORTH | MASK_WEST));
    set_cell(chunk, x1, y0, wall_cell(MASK_NORTH | MASK_EAST));
    set_cell(chunk, x0, y1, wall_cell(MASK_SOUTH | MASK_WEST));
    set_cell(chunk, x1, y1, wall_cell(MASK_SOUTH | MASK_EAST));

    for x in (x0 + 1)..x1 {
        set_cell(chunk, x, y0, wall_cell(MASK_NORTH));
        set_cell(chunk, x, y1, wall_cell(MASK_SOUTH));
    }
    for y in (y0 + 1)..y1 {
        set_cell(chunk, x0, y, wall_cell(MASK_WEST));
        set_cell(chunk, x1, y, wall_cell(MASK_EAST));
    }

    // Exterior door: opening on the south façade toward the alley / plot exterior.
    if x1 > x0 + 1 {
        let door_x = (x0 + x1) / 2;
        clear_wall_edge_bit(chunk, door_x, y1, MASK_SOUTH);
    }
}

fn stamp_two_rooms_vertical(chunk: &mut HypermapChunk<CellType>, rng: &mut StdRng, p: Plot) {
    let inner_w = p.w.saturating_sub(2);
    if inner_w < 6 {
        return;
    }
    let vx = p.x0 + 1 + rng.gen_range(2..inner_w.saturating_sub(2).max(3));
    let door_y = (p.y0 + 2).max((p.y0 + p.h / 2).saturating_sub(1));
    let door_y = door_y.min(p.y1().saturating_sub(2));

    for y in (p.y0 + 1)..p.y1() {
        if y == door_y {
            continue;
        }
        set_cell(chunk, vx, y, wall_cell(MASK_EAST));
        set_cell(chunk, vx + 1, y, wall_cell(MASK_WEST));
    }
}

fn stamp_two_rooms_horizontal(chunk: &mut HypermapChunk<CellType>, rng: &mut StdRng, p: Plot) {
    let inner_h = p.h.saturating_sub(2);
    if inner_h < 6 {
        return;
    }
    let hy = p.y0 + 1 + rng.gen_range(2..inner_h.saturating_sub(2).max(3));
    let door_x = (p.x0 + 2).max((p.x0 + p.w / 2).saturating_sub(1));
    let door_x = door_x.min(p.x1().saturating_sub(2));

    for x in (p.x0 + 1)..p.x1() {
        if x == door_x {
            continue;
        }
        set_cell(chunk, x, hy, wall_cell(MASK_SOUTH));
    }
}

/// Vertical split plus a horizontal split in the east wing (three rooms).
fn stamp_three_rooms_el(chunk: &mut HypermapChunk<CellType>, rng: &mut StdRng, p: Plot) {
    let inner_w = p.w.saturating_sub(2);
    if inner_w < 10 || p.h < 10 {
        stamp_two_rooms_vertical(chunk, rng, p);
        return;
    }
    let vx = p.x0 + inner_w / 3;
    let door_vy = rng.gen_range((p.y0 + 2)..=(p.y1().saturating_sub(2)));
    for y in (p.y0 + 1)..p.y1() {
        if y == door_vy {
            continue;
        }
        set_cell(chunk, vx, y, wall_cell(MASK_EAST));
        set_cell(chunk, vx + 1, y, wall_cell(MASK_WEST));
    }

    let hy_lo = (p.y0 + 3).min(p.y1().saturating_sub(3));
    let hy_hi = (p.y1() - 3).max(hy_lo);
    let hy = rng.gen_range(hy_lo..=hy_hi);
    let hx_lo = (vx + 2).min(p.x1().saturating_sub(2));
    let hx_hi = (p.x1() - 2).max(hx_lo);
    let door_hx = rng.gen_range(hx_lo..=hx_hi);
    for x in (vx + 1)..p.x1() {
        if x == door_hx {
            continue;
        }
        set_cell(chunk, x, hy, wall_cell(MASK_SOUTH));
    }
}

/// Four rooms: vertical and horizontal splits; intersection cells use corner walls.
fn stamp_four_rooms_quad(chunk: &mut HypermapChunk<CellType>, rng: &mut StdRng, p: Plot) {
    let inner_w = p.w.saturating_sub(2);
    let inner_h = p.h.saturating_sub(2);
    if inner_w < 10 || inner_h < 10 {
        stamp_two_rooms_vertical(chunk, rng, p);
        return;
    }

    let vx = p.x0 + inner_w / 2;
    let hy = p.y0 + inner_h / 2;
    let door_vy = if hy > p.y0 + 3 {
        rng.gen_range((p.y0 + 2)..hy)
    } else if hy + 2 < p.y1() {
        rng.gen_range((hy + 1)..p.y1())
    } else {
        (p.y0 + 2).min(p.y1().saturating_sub(2))
    };
    let door_hx = if vx > p.x0 + 3 {
        rng.gen_range((p.x0 + 2)..vx)
    } else if vx + 2 < p.x1() {
        rng.gen_range((vx + 2)..p.x1())
    } else {
        (p.x0 + 2).min(p.x1().saturating_sub(2))
    };

    for y in (p.y0 + 1)..p.y1() {
        if y == door_vy {
            continue;
        }
        if y == hy {
            set_cell(chunk, vx, y, wall_cell(MASK_SOUTH | MASK_EAST));
            set_cell(chunk, vx + 1, y, wall_cell(MASK_SOUTH | MASK_WEST));
        } else {
            set_cell(chunk, vx, y, wall_cell(MASK_EAST));
            set_cell(chunk, vx + 1, y, wall_cell(MASK_WEST));
        }
    }

    for x in (p.x0 + 1)..p.x1() {
        if x == door_hx {
            continue;
        }
        if x == vx || x == vx + 1 {
            continue;
        }
        set_cell(chunk, x, hy, wall_cell(MASK_SOUTH));
    }
}

fn spawn_chunk_meshes(
    commands: &mut Commands,
    chunk_root: Entity,
    meshes: &mut Assets<Mesh>,
    prepared: &PreparedChunkRender,
    origin_x: i32,
    origin_y: i32,
    assets: &HypermapRenderAssets,
) -> ChunkUpperMeshEntities {
    let ox = origin_x;
    let oy = origin_y;

    {
        let (mesh3d, vis, pick) = if let Some(m) = build_floor0_road_mesh(&prepared.floor0_cells, ox, oy) {
            (
                Mesh3d(meshes.add(m)),
                Visibility::Inherited,
                Pickable::default(),
            )
        } else {
            (
                Mesh3d(assets.empty_mesh.clone()),
                Visibility::Hidden,
                Pickable::IGNORE,
            )
        };
        let id = commands
            .spawn((
                Name::new(format!("Chunk floor0 road {},{}", ox, oy)),
                RenderedChunkFloor0Road,
                mesh3d,
                MeshMaterial3d(assets.road_material.clone()),
                Transform::default(),
                vis,
                pick,
            ))
            .observe(floor_grid_click)
            .id();
        commands.entity(chunk_root).add_child(id);
    }

    {
        let (mesh3d, vis) = if let Some(m) = build_floor0_wall_mesh(&prepared.floor0_cells, ox, oy) {
            (Mesh3d(meshes.add(m)), Visibility::Inherited)
        } else {
            (Mesh3d(assets.empty_mesh.clone()), Visibility::Hidden)
        };
        let id = commands
            .spawn((
                Name::new(format!("Chunk floor0 walls {},{}", ox, oy)),
                RenderedChunkFloor0Walls,
                mesh3d,
                MeshMaterial3d(assets.wall_material.clone()),
                Transform::default(),
                vis,
                Pickable::IGNORE,
            ))
            .id();
        commands.entity(chunk_root).add_child(id);
    }

    let upper_road = {
        let (mesh3d, vis, pick) = if let Some(m) = build_upper_road_mesh(&prepared.upper_cells, ox, oy) {
            (
                Mesh3d(meshes.add(m)),
                Visibility::Inherited,
                Pickable::default(),
            )
        } else {
            (
                Mesh3d(assets.empty_mesh.clone()),
                Visibility::Hidden,
                Pickable::IGNORE,
            )
        };
        let id = commands
            .spawn((
                Name::new(format!("Chunk upper road {},{}", ox, oy)),
                RenderedChunkUpperRoad,
                mesh3d,
                MeshMaterial3d(assets.road_material.clone()),
                Transform::default(),
                vis,
                pick,
            ))
            .observe(floor_grid_click)
            .id();
        commands.entity(chunk_root).add_child(id);
        id
    };

    let upper_walls = {
        let (mesh3d, vis) = if let Some(m) = build_upper_wall_mesh(&prepared.upper_cells, ox, oy) {
            (Mesh3d(meshes.add(m)), Visibility::Inherited)
        } else {
            (Mesh3d(assets.empty_mesh.clone()), Visibility::Hidden)
        };
        let id = commands
            .spawn((
                Name::new(format!("Chunk upper walls {},{}", ox, oy)),
                RenderedChunkUpperWalls,
                mesh3d,
                MeshMaterial3d(assets.wall_material.clone()),
                Transform::default(),
                vis,
                Pickable::IGNORE,
            ))
            .id();
        commands.entity(chunk_root).add_child(id);
        id
    };

    ChunkUpperMeshEntities {
        road: upper_road,
        walls: upper_walls,
    }
}

pub(crate) fn build_floor0_road_mesh(cells: &[(u8, u8, CellType)], origin_x: i32, origin_y: i32) -> Option<Mesh> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    for &(x, y, cell_type) in cells {
        if matches!(cell_type, CellType::Void) {
            continue;
        }
        let x0 = origin_x as f32 + x as f32;
        let z0 = origin_y as f32 + y as f32;
        let x1 = x0 + 1.0;
        let z1 = z0 + 1.0;
        append_quad(
            &mut positions,
            &mut normals,
            &mut uvs,
            &mut indices,
            [x0, 0.0, z0],
            [x1, 0.0, z0],
            [x1, 0.0, z1],
            [x0, 0.0, z1],
            [0.0, 1.0, 0.0],
        );
    }

    if positions.is_empty() {
        return None;
    }
    finalize_mesh_from_buffers(positions, normals, uvs, indices)
}

pub(crate) fn build_floor0_wall_mesh(cells: &[(u8, u8, CellType)], origin_x: i32, origin_y: i32) -> Option<Mesh> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    for &(x, y, cell_type) in cells {
        let cx = origin_x as f32 + x as f32 + 0.5;
        let cz = origin_y as f32 + y as f32 + 0.5;
        match cell_type {
            CellType::Wall(mask) => {
                for_each_wall_segment(mask.bits(), |sx, sz, ox, oz| {
                    append_box(
                        &mut positions,
                        &mut normals,
                        &mut uvs,
                        &mut indices,
                        cx + ox,
                        WALL_HEIGHT * 0.5,
                        cz + oz,
                        sx,
                        WALL_HEIGHT,
                        sz,
                    );
                });
            }
            CellType::Corner(corner) => {
                let (ox, oz) = corner.xz_offset_from_cell_center();
                append_box(
                    &mut positions,
                    &mut normals,
                    &mut uvs,
                    &mut indices,
                    cx + ox,
                    WALL_HEIGHT * 0.5,
                    cz + oz,
                    WALL_THICKNESS,
                    WALL_HEIGHT,
                    WALL_THICKNESS,
                );
            }
            _ => {}
        }
    }

    if positions.is_empty() {
        return None;
    }
    finalize_mesh_from_buffers(positions, normals, uvs, indices)
}

pub(crate) fn build_upper_road_mesh(cells: &[(u8, u8, u8, CellType)], origin_x: i32, origin_y: i32) -> Option<Mesh> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    for &(x, y, floor, cell_type) in cells {
        if matches!(cell_type, CellType::Void) {
            continue;
        }
        let y_base = floor as f32 * HYPERMAP_FLOOR_HEIGHT;
        let x0 = origin_x as f32 + x as f32;
        let z0 = origin_y as f32 + y as f32;
        let x1 = x0 + 1.0;
        let z1 = z0 + 1.0;
        append_quad(
            &mut positions,
            &mut normals,
            &mut uvs,
            &mut indices,
            [x0, y_base, z0],
            [x1, y_base, z0],
            [x1, y_base, z1],
            [x0, y_base, z1],
            [0.0, 1.0, 0.0],
        );
    }

    if positions.is_empty() {
        return None;
    }
    finalize_mesh_from_buffers(positions, normals, uvs, indices)
}

pub(crate) fn build_upper_wall_mesh(cells: &[(u8, u8, u8, CellType)], origin_x: i32, origin_y: i32) -> Option<Mesh> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    for &(x, y, floor, cell_type) in cells {
        let y_base = floor as f32 * HYPERMAP_FLOOR_HEIGHT;
        let cx = origin_x as f32 + x as f32 + 0.5;
        let cz = origin_y as f32 + y as f32 + 0.5;
        match cell_type {
            CellType::Wall(mask) => {
                for_each_wall_segment(mask.bits(), |sx, sz, ox, oz| {
                    append_box(
                        &mut positions,
                        &mut normals,
                        &mut uvs,
                        &mut indices,
                        cx + ox,
                        y_base + WALL_HEIGHT * 0.5,
                        cz + oz,
                        sx,
                        WALL_HEIGHT,
                        sz,
                    );
                });
            }
            CellType::Corner(corner) => {
                let (ox, oz) = corner.xz_offset_from_cell_center();
                append_box(
                    &mut positions,
                    &mut normals,
                    &mut uvs,
                    &mut indices,
                    cx + ox,
                    y_base + WALL_HEIGHT * 0.5,
                    cz + oz,
                    WALL_THICKNESS,
                    WALL_HEIGHT,
                    WALL_THICKNESS,
                );
            }
            _ => {}
        }
    }

    if positions.is_empty() {
        return None;
    }
    finalize_mesh_from_buffers(positions, normals, uvs, indices)
}

fn finalize_mesh_from_buffers(
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u32>,
) -> Option<Mesh> {
    if positions.is_empty() {
        return None;
    }
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(Indices::U32(indices));
    Some(mesh)
}

fn append_quad(
    positions: &mut Vec<[f32; 3]>,
    normals: &mut Vec<[f32; 3]>,
    uvs: &mut Vec<[f32; 2]>,
    indices: &mut Vec<u32>,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    d: [f32; 3],
    normal: [f32; 3],
) {
    let base = positions.len() as u32;
    positions.extend_from_slice(&[a, b, c, d]);
    normals.extend_from_slice(&[normal, normal, normal, normal]);
    uvs.extend_from_slice(&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]);
    // Bevy uses backface culling; keep winding CCW for the face normal.
    indices.extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
}

fn append_box(
    positions: &mut Vec<[f32; 3]>,
    normals: &mut Vec<[f32; 3]>,
    uvs: &mut Vec<[f32; 2]>,
    indices: &mut Vec<u32>,
    cx: f32,
    cy: f32,
    cz: f32,
    sx: f32,
    sy: f32,
    sz: f32,
) {
    let hx = sx * 0.5;
    let hy = sy * 0.5;
    let hz = sz * 0.5;

    // +Y top
    append_quad(
        positions,
        normals,
        uvs,
        indices,
        [cx - hx, cy + hy, cz - hz],
        [cx + hx, cy + hy, cz - hz],
        [cx + hx, cy + hy, cz + hz],
        [cx - hx, cy + hy, cz + hz],
        [0.0, 1.0, 0.0],
    );
    // -Y bottom
    append_quad(
        positions,
        normals,
        uvs,
        indices,
        [cx - hx, cy - hy, cz + hz],
        [cx + hx, cy - hy, cz + hz],
        [cx + hx, cy - hy, cz - hz],
        [cx - hx, cy - hy, cz - hz],
        [0.0, -1.0, 0.0],
    );
    // +X
    append_quad(
        positions,
        normals,
        uvs,
        indices,
        [cx + hx, cy - hy, cz - hz],
        [cx + hx, cy - hy, cz + hz],
        [cx + hx, cy + hy, cz + hz],
        [cx + hx, cy + hy, cz - hz],
        [1.0, 0.0, 0.0],
    );
    // -X
    append_quad(
        positions,
        normals,
        uvs,
        indices,
        [cx - hx, cy - hy, cz + hz],
        [cx - hx, cy - hy, cz - hz],
        [cx - hx, cy + hy, cz - hz],
        [cx - hx, cy + hy, cz + hz],
        [-1.0, 0.0, 0.0],
    );
    // +Z
    append_quad(
        positions,
        normals,
        uvs,
        indices,
        [cx - hx, cy - hy, cz + hz],
        [cx + hx, cy - hy, cz + hz],
        [cx + hx, cy + hy, cz + hz],
        [cx - hx, cy + hy, cz + hz],
        [0.0, 0.0, 1.0],
    );
    // -Z
    append_quad(
        positions,
        normals,
        uvs,
        indices,
        [cx + hx, cy - hy, cz - hz],
        [cx - hx, cy - hy, cz - hz],
        [cx - hx, cy + hy, cz - hz],
        [cx + hx, cy + hy, cz - hz],
        [0.0, 0.0, -1.0],
    );
}

fn hash_chunk_seed(coord: ChunkCoord) -> u64 {
    let x = coord.x as u64;
    let y = coord.y as u64;
    x.wrapping_mul(0x9E37_79B9_85F3_7D87) ^ y.wrapping_mul(0xC2B2_AE3D_27D4_F4F5) ^ 0xA32D_192E_2AA3_4C13
}

fn clone_chunk_for_render_start(chunk: &HypermapChunk<CellType>) -> Vec<CellType> {
    let mut cells = Vec::with_capacity(HYPERMAP_CHUNK_SIZE as usize * HYPERMAP_CHUNK_SIZE as usize * HYPERMAP_FLOOR_COUNT);
    for y in 0..HYPERMAP_CHUNK_SIZE as u8 {
        for x in 0..HYPERMAP_CHUNK_SIZE as u8 {
            let local = LocalCoord::new(x, y);
            for floor in 0..HYPERMAP_FLOOR_COUNT as u8 {
                cells.push(*chunk.get_local_floor(local, floor));
            }
        }
    }
    cells
}

fn partition_chunk_cells_from_vec(
    cells: Vec<CellType>,
    active_floor: u8,
) -> (bool, Vec<(u8, u8, CellType)>, Vec<(u8, u8, u8, CellType)>) {
    let mut has_void_floor0_water = false;
    let mut floor0_cells = Vec::new();
    let mut upper_cells = Vec::new();
    let mut i = 0usize;
    for y in 0..HYPERMAP_CHUNK_SIZE as u8 {
        for x in 0..HYPERMAP_CHUNK_SIZE as u8 {
            for floor in 0..HYPERMAP_FLOOR_COUNT as u8 {
                let cell_type = cells[i];
                i += 1;
                if cell_type == CellType::Void {
                    if floor == 0 {
                        has_void_floor0_water = true;
                    }
                    continue;
                }
                if floor == 0 {
                    floor0_cells.push((x, y, cell_type));
                } else if active_floor > 0 && floor >= active_floor {
                    upper_cells.push((x, y, floor, cell_type));
                }
            }
        }
    }
    (has_void_floor0_water, floor0_cells, upper_cells)
}

fn build_chunk_render_data(cells: Vec<CellType>, active_floor: u8) -> PreparedChunkRender {
    let (has_void_floor0_water, floor0_cells, upper_cells) = partition_chunk_cells_from_vec(cells, active_floor);
    PreparedChunkRender {
        has_void_floor0_water,
        floor0_cells,
        upper_cells,
    }
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

fn apply_world_map_file_to_floor(
    chunk: &mut HypermapChunk<CellType>,
    floor: u8,
    path: &str,
) -> Result<(), String> {
    let text = std::fs::read_to_string(Path::new(path))
        .map_err(|err| format!("read `{path}`: {err}"))?;
    let map = WorldMapFloor::from_ascii(&text).map_err(|err| format!("parse `{path}`: {err}"))?;

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
            let local = LocalCoord::new(local_x as u8, local_y as u8);
            chunk.set_local_floor(local, floor, cell.get_cell_type());
        }
    }

    Ok(())
}

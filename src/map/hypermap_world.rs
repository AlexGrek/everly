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
use bevy_water::{WaterQuality, WaterSettings, WaterTile, WaterTiles, WaveDirection};
use futures_lite::future;
use crate::edit::map_selection::{floor_grid_click, MapSelectionRoadMaterial};
use crate::map::floor_level::{ActiveFloorLevel, HYPERMAP_FLOOR_HEIGHT, HYPERMAP_WALL_HEIGHT};
use crate::map::hypermap::{
    world_to_chunk_local, ChunkCoord, Hypermap, HypermapChunk, LocalCoord, HYPERMAP_CHUNK_SIZE,
    HYPERMAP_FLOOR_COUNT,
};
use crate::map::level::{
    chunk_geometry_path, chunk_style_floor_path, chunk_style_wall_path,
    try_load_chunk_geometry_file, try_load_chunk_style_file_into_map, LevelName,
};
use crate::menu::main_menu::GameState;
use crate::map::dirt::DirtMap;
use crate::map::passability::{cell_subtile_flags, SubtilePassability, SUBTILE_COUNT};
use crate::map::chunk_metadata::{try_load_chunk_metadata, ChunkGeneratorMetadata};
use crate::map::map_generator::{fill_procedural_chunk, CHUNK_VOID_MARGIN};
use crate::map::temperature::TemperatureMap;
use crate::map::world_map::{
    cell_passability, for_each_wall_segment, CellType, TileStyle, WorldMapFloor,
    WALL_THICKNESS, WATER_SURFACE_Y,
    WORLD_MAP_FILE_PATH,
};
use crate::scene::camera::StrategyCamera;

/// Wall slab height — [`HYPERMAP_WALL_HEIGHT`] (floor plane spacing is slightly larger).
const WALL_HEIGHT: f32 = HYPERMAP_WALL_HEIGHT;
/// Floor-0 void must fall inside this inset (local cell coords) before a water
/// plane is spawned; the water mesh is also shrunk by this strip so nothing
/// renders in the chunk border band.
const WATER_MESH_EDGE_STRIP: i32 = CHUNK_VOID_MARGIN;
const WORLD_MAP_FLOOR1_FILE_PATH: &str = "world_map_floor1.txt";
const CENTER_CHUNK: ChunkCoord = ChunkCoord { x: 0, y: 0 };
const DEAD_ZONE_SIZE: i32 = 20;
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
struct RenderedChunkFloor0GlassWalls;

#[derive(Component)]
struct RenderedChunkFloor0GlassFloor;

#[derive(Component)]
struct RenderedChunkFloor0Pavement;

#[derive(Component)]
struct RenderedChunkFloor0Marble;

#[derive(Component)]
struct RenderedChunkUpperRoad;

#[derive(Component)]
struct RenderedChunkUpperWalls;

#[derive(Component)]
struct RenderedChunkUpperGlassWalls;

#[derive(Component)]
struct RenderedChunkUpperGlassFloor;

#[derive(Component)]
struct RenderedChunkUpperPavement;

#[derive(Component)]
struct RenderedChunkUpperMarble;

/// Upper-layer mesh entities (refreshed when HUD floor changes). Floor 0 meshes stay on the chunk root.
#[derive(Clone, Copy)]
struct ChunkUpperMeshEntities {
    road: Entity,
    road_glass: Entity,
    road_pavement: Entity,
    road_marble: Entity,
    walls: Entity,
    glass_walls: Entity,
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
    /// Per-cell static passability (`0.0` blocked, `1.0` walkable) mirroring [`map`](Self::map).
    /// Derived from [`CellType`] via [`cell_passability`] whenever a chunk is generated or a
    /// cell is edited (see [`write_world_cell`] / [`ensure_chunk_generated`]). "Static" because
    /// no runtime obstacles (units, doors) participate — only the authored / procedural geometry.
    pub(crate) static_passability_map: Arc<Hypermap<f32>>,
    /// Per-subtile flag cache for static geometry. Each world tile stores a
    /// [`SubtilePassability`] with pre-computed [`cell_subtile_flags`] values.
    /// Updated only when geometry changes (chunk generation or cell edit) —
    /// never re-iterated per frame.
    pub(crate) static_subtile_cache: Arc<Hypermap<SubtilePassability>>,
    /// Per-cell floor quad style. Default is [`TileStyle::DEFAULT`].
    /// Controls which floor material the quad under any cell uses.
    pub(crate) style_floor_map: Arc<Hypermap<TileStyle>>,
    /// Per-cell wall slab style. Default is [`TileStyle::DEFAULT`] (regular).
    /// Controls whether wall/corner slabs render as glass or regular material.
    pub(crate) style_wall_map: Arc<Hypermap<TileStyle>>,
    /// Per-chunk procedural layout reference (rooms, entrypoint). See `docs/map-generator.md`.
    pub procedural_metadata: ChunkGeneratorMetadata,
    desired_chunks: HashSet<ChunkCoord>,
    chunk_roots: HashMap<ChunkCoord, Entity>,
    chunk_upper_meshes: HashMap<ChunkCoord, ChunkUpperMeshEntities>,
    water_tiles: HashMap<ChunkCoord, Entity>,
    pending_renders: HashMap<ChunkCoord, Task<PreparedChunkRender>>,
    ready_renders: VecDeque<(ChunkCoord, PreparedChunkRender)>,
    despawn_queue: VecDeque<ChunkCoord>,
    last_center_chunk: Option<ChunkCoord>,
    water_root: Entity,
}

impl HypermapRuntime {
    pub(crate) fn ensure_chunk_generated(&mut self, coord: ChunkCoord, level_name: &str) {
        let map = self.map.clone();
        let passability = self.static_passability_map.clone();
        let subtile_cache = self.static_subtile_cache.clone();
        let style_floor_map = self.style_floor_map.clone();
        let style_wall_map = self.style_wall_map.clone();
        ensure_chunk_generated(
            &map,
            &passability,
            &subtile_cache,
            &style_floor_map,
            &style_wall_map,
            coord,
            level_name,
            &mut self.procedural_metadata,
        );
    }

    /// Chunk coordinates currently targeted for rendering (see `update_visible_hypermap_chunks`).
    pub fn desired_chunk_coords(&self) -> Vec<ChunkCoord> {
        self.desired_chunks.iter().copied().collect()
    }

    /// Returns `true` when the chunk that contains world tile `(world_x, world_z)` has a
    /// spawned mesh entity — i.e. the tile is currently visible on screen.
    pub fn is_world_pos_rendered(&self, world_x: i32, world_z: i32) -> bool {
        let (coord, _) = world_to_chunk_local(world_x, world_z);
        self.chunk_roots.contains_key(&coord)
    }
}

struct PreparedChunkRender {
    /// True when floor `0` has void **strictly inside** [`WATER_MESH_EDGE_STRIP`]
    /// from each chunk edge — drives an inset water plane (no water in the border band).
    has_void_floor0_water: bool,
    // ── Floor-0 floor quad meshes (one per material, keyed by floor_style) ───
    floor0_road_cells: Vec<(i32, i32, CellType)>,
    floor0_glass_floor_cells: Vec<(i32, i32, CellType)>,
    floor0_pavement_cells: Vec<(i32, i32, CellType)>,
    floor0_marble_cells: Vec<(i32, i32, CellType)>,
    // ── Floor-0 wall slab meshes (keyed by wall_style) ──────────────────────
    floor0_wall_cells: Vec<(i32, i32, CellType)>,
    floor0_glass_cells: Vec<(i32, i32, CellType)>,
    // ── Upper floor meshes (active_floor at bake time) ───────────────────────
    upper_cells: Vec<(i32, i32, i32, CellType)>,
    upper_glass_cells: Vec<(i32, i32, i32, CellType)>,
    upper_road_default_cells: Vec<(i32, i32, i32, CellType)>,
    upper_road_glass_cells: Vec<(i32, i32, i32, CellType)>,
    upper_road_pavement_cells: Vec<(i32, i32, i32, CellType)>,
    upper_road_marble_cells: Vec<(i32, i32, i32, CellType)>,
}

#[derive(Resource)]
struct ChunkRenderCadence {
    timer: Timer,
}

#[derive(Resource)]
struct HypermapRenderAssets {
    /// Horizontal water plane sized to the chunk **interior** (excluding
    /// [`WATER_MESH_EDGE_STRIP`] cells on each side) so water never covers the border band.
    water_mesh: Handle<Mesh>,
    /// Invisible placeholder mesh for upper layers when empty.
    empty_mesh: Handle<Mesh>,
    // ── Floor materials ──────────────────────────────────────────────────────
    road_material: Handle<StandardMaterial>,
    glass_road_material: Handle<StandardMaterial>,
    pavement_material: Handle<StandardMaterial>,
    marble_material: Handle<StandardMaterial>,
    // ── Wall materials ───────────────────────────────────────────────────────
    wall_material: Handle<StandardMaterial>,
    glass_wall_material: Handle<StandardMaterial>,
    water_material: Handle<StandardWaterMaterial>,
}

pub struct HypermapWorldPlugin;

impl Plugin for HypermapWorldPlugin {
    fn build(&self, app: &mut App) {
        // `setup_water` runs in `Startup` (before any state transition), so by
        // the time we enter `GameState::InGame` its resources are ready and
        // the cross-schedule `.after(setup_water)` from before is no longer
        // needed.
        app.add_systems(
            OnEnter(GameState::InGame),
            (setup_hypermap_runtime, setup_hypermap_assets).chain(),
        )
        .add_systems(
            Update,
            (
                refresh_chunk_upper_layers_on_floor_change,
                update_visible_hypermap_chunks,
                render_chunks_30fps,
            )
                .chain()
                .run_if(in_state(GameState::InGame)),
        );
    }
}

pub(crate) fn setup_hypermap_runtime(mut commands: Commands) {
    let water_root = commands
        .spawn((Name::new("Hypermap water"), WaterTiles))
        .id();
    commands.init_resource::<HypermapChunkRemeshQueue>();
    commands.insert_resource(HypermapRuntime {
        map: Arc::new(Hypermap::new(CellType::Road)),
        static_passability_map: Arc::new(Hypermap::new(cell_passability(CellType::Road))),
        static_subtile_cache: Arc::new(Hypermap::new(SubtilePassability::EMPTY)),
        style_floor_map: Arc::new(Hypermap::new(TileStyle::DEFAULT)),
        style_wall_map: Arc::new(Hypermap::new(TileStyle::DEFAULT)),
        procedural_metadata: ChunkGeneratorMetadata::default(),
        desired_chunks: HashSet::new(),
        chunk_roots: HashMap::new(),
        chunk_upper_meshes: HashMap::new(),
        water_tiles: HashMap::new(),
        pending_renders: HashMap::new(),
        ready_renders: VecDeque::new(),
        despawn_queue: VecDeque::new(),
        last_center_chunk: None,
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
    let interior = (HYPERMAP_CHUNK_SIZE - 2 * WATER_MESH_EDGE_STRIP).max(1) as f32;
    let water_mesh = meshes.add(PlaneMeshBuilder::from_size(Vec2::new(interior, interior)));
    let empty_mesh = meshes.add(PlaneMeshBuilder::from_size(Vec2::splat(0.02)));

    let road_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.10, 0.10, 0.12),
        perceptual_roughness: 0.96,
        metallic: 0.0,
        ..default()
    });
    let glass_road_material = materials.add(StandardMaterial {
        base_color: Color::srgba(0.04, 0.04, 0.06, 0.90),
        perceptual_roughness: 0.02,
        metallic: 0.0,
        reflectance: 0.95,
        alpha_mode: AlphaMode::Blend,
        ..default()
    });
    let pavement_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.52, 0.53, 0.55),
        perceptual_roughness: 0.88,
        metallic: 0.0,
        ..default()
    });
    let marble_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.95, 0.94, 0.91),
        perceptual_roughness: 0.12,
        metallic: 0.0,
        reflectance: 0.70,
        ..default()
    });
    let wall_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.78, 0.79, 0.82),
        perceptual_roughness: 0.72,
        metallic: 0.02,
        cull_mode: None,
        ..default()
    });
    let glass_wall_material = materials.add(StandardMaterial {
        base_color: Color::srgba(0.58, 0.80, 0.96, 0.20),
        perceptual_roughness: 0.04,
        metallic: 0.0,
        reflectance: 0.90,
        alpha_mode: AlphaMode::Blend,
        cull_mode: None,
        ..default()
    });
    let normalized_dir = settings.wave_direction.normalize_or_zero();
    let coord_scale = Vec2::splat(interior);
    let coord_offset = Vec2::splat(-interior * 0.5);
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
        glass_road_material,
        pavement_material,
        marble_material,
        wall_material,
        glass_wall_material,
        water_material,
    });
    commands.insert_resource(MapSelectionRoadMaterial(road_material));
}

fn refresh_chunk_upper_layers_on_floor_change(
    floor: Res<ActiveFloorLevel>,
    mut prev_floor: Local<Option<i32>>,
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
    let style_floor_map = runtime.style_floor_map.clone();
    let style_wall_map = runtime.style_wall_map.clone();
    let chunk_list: Vec<(ChunkCoord, ChunkUpperMeshEntities)> =
        runtime.chunk_upper_meshes.iter().map(|(&c, &e)| (c, e)).collect();

    for (coord, mesh_ids) in chunk_list {
        let Some(cells) = map.with_chunk_read(coord, clone_chunk_for_render_start) else {
            continue;
        };
        let n = cells.len();
        let floor_styles = style_floor_map
            .with_chunk_read(coord, clone_styles_for_render_start)
            .unwrap_or_else(|| vec![TileStyle::DEFAULT; n]);
        let wall_styles = style_wall_map
            .with_chunk_read(coord, clone_styles_for_render_start)
            .unwrap_or_else(|| vec![TileStyle::DEFAULT; n]);
        let snapshot: Vec<(CellType, TileStyle, TileStyle)> = cells.into_iter()
            .zip(floor_styles)
            .zip(wall_styles)
            .map(|((c, fs), ws)| (c, fs, ws))
            .collect();
        let prepared = partition_chunk_cells_from_vec(snapshot, current);
        let ox = coord.x * HYPERMAP_CHUNK_SIZE;
        let oy = coord.y * HYPERMAP_CHUNK_SIZE;

        update_upper_road_entity(
            &mut commands, &mut meshes, mesh_ids.road,
            build_upper_road_mesh(&prepared.upper_road_default_cells, ox, oy),
            assets.road_material.clone(), assets.empty_mesh.clone(),
        );
        update_upper_road_entity(
            &mut commands, &mut meshes, mesh_ids.road_glass,
            build_upper_road_mesh(&prepared.upper_road_glass_cells, ox, oy),
            assets.glass_road_material.clone(), assets.empty_mesh.clone(),
        );
        update_upper_road_entity(
            &mut commands, &mut meshes, mesh_ids.road_pavement,
            build_upper_road_mesh(&prepared.upper_road_pavement_cells, ox, oy),
            assets.pavement_material.clone(), assets.empty_mesh.clone(),
        );
        update_upper_road_entity(
            &mut commands, &mut meshes, mesh_ids.road_marble,
            build_upper_road_mesh(&prepared.upper_road_marble_cells, ox, oy),
            assets.marble_material.clone(), assets.empty_mesh.clone(),
        );

        if let Some(mesh) = build_upper_wall_mesh(&prepared.upper_cells, ox, oy) {
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

        if let Some(mesh) = build_upper_wall_mesh(&prepared.upper_glass_cells, ox, oy) {
            let h = meshes.add(mesh);
            commands.entity(mesh_ids.glass_walls).insert((
                Mesh3d(h),
                MeshMaterial3d(assets.glass_wall_material.clone()),
                Visibility::Inherited,
            ));
        } else {
            commands.entity(mesh_ids.glass_walls).insert((
                Mesh3d(assets.empty_mesh.clone()),
                MeshMaterial3d(assets.glass_wall_material.clone()),
                Visibility::Hidden,
                Pickable::IGNORE,
            ));
        }
    }
}

fn update_upper_road_entity(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    entity: Entity,
    mesh: Option<Mesh>,
    material: Handle<StandardMaterial>,
    empty_mesh: Handle<Mesh>,
) {
    if let Some(m) = mesh {
        let h = meshes.add(m);
        commands.entity(entity).insert((
            Mesh3d(h),
            MeshMaterial3d(material),
            Visibility::Inherited,
            Pickable::default(),
        ));
    } else {
        commands.entity(entity).insert((
            Mesh3d(empty_mesh),
            MeshMaterial3d(material),
            Visibility::Hidden,
            Pickable::IGNORE,
        ));
    }
}

fn update_visible_hypermap_chunks(
    mut runtime: ResMut<HypermapRuntime>,
    cameras: Query<&StrategyCamera>,
    active_floor: Res<ActiveFloorLevel>,
    level: Res<LevelName>,
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

    let target_chunks = target_chunks_for(center, local);
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
        runtime.ensure_chunk_generated(chunk, level.0.as_str());
        if runtime.chunk_roots.contains_key(&chunk) || runtime.pending_renders.contains_key(&chunk) {
            continue;
        }
        let Some(cells) = runtime.map.with_chunk_read(chunk, clone_chunk_for_render_start) else {
            continue;
        };
        let n = cells.len();
        let floor_styles = runtime.style_floor_map
            .with_chunk_read(chunk, clone_styles_for_render_start)
            .unwrap_or_else(|| vec![TileStyle::DEFAULT; n]);
        let wall_styles = runtime.style_wall_map
            .with_chunk_read(chunk, clone_styles_for_render_start)
            .unwrap_or_else(|| vec![TileStyle::DEFAULT; n]);
        let snapshot: Vec<(CellType, TileStyle, TileStyle)> = cells.into_iter()
            .zip(floor_styles)
            .zip(wall_styles)
            .map(|((c, fs), ws)| (c, fs, ws))
            .collect();
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
    level: Res<LevelName>,
    roots: Query<Entity, With<RenderedChunkRoot>>,
    waters: Query<Entity, With<RenderedChunkWater>>,
) {
    let remesh_coords: Vec<ChunkCoord> = remesh.0.drain().collect();
    for coord in remesh_coords {
        if !runtime.desired_chunks.contains(&coord) {
            continue;
        }
        runtime.ensure_chunk_generated(coord, level.0.as_str());
        // Keep existing meshes until the new bake is ready; despawn happens in the
        // `ready_renders` spawn path. Eager despawn here left a hole (async bake +
        // 30 Hz cadence) and read as a black blink on each paint click.
        runtime.pending_renders.remove(&coord);
        runtime
            .ready_renders
            .retain(|(c, _)| *c != coord);
        let task_pool = AsyncComputeTaskPool::get();
        let floor_for_render = active_floor.0;
        if let Some(cells) = runtime.map.with_chunk_read(coord, clone_chunk_for_render_start) {
            let n = cells.len();
            let floor_styles = runtime.style_floor_map
                .with_chunk_read(coord, clone_styles_for_render_start)
                .unwrap_or_else(|| vec![TileStyle::DEFAULT; n]);
            let wall_styles = runtime.style_wall_map
                .with_chunk_read(coord, clone_styles_for_render_start)
                .unwrap_or_else(|| vec![TileStyle::DEFAULT; n]);
            let snapshot: Vec<(CellType, TileStyle, TileStyle)> = cells.into_iter()
                .zip(floor_styles)
                .zip(wall_styles)
                .map(|((c, fs), ws)| (c, fs, ws))
                .collect();
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
    if !cadence.timer.just_finished() && runtime.ready_renders.is_empty() {
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

pub(crate) fn ensure_chunk_generated(
    map: &Hypermap<CellType>,
    passability: &Hypermap<f32>,
    subtile_cache: &Hypermap<SubtilePassability>,
    style_floor_map: &Hypermap<TileStyle>,
    style_wall_map: &Hypermap<TileStyle>,
    coord: ChunkCoord,
    level_name: &str,
    chunk_metadata: &mut ChunkGeneratorMetadata,
) {
    if map.has_chunk(coord) {
        return;
    }

    let path = chunk_geometry_path(level_name, coord);
    let mut loaded_from_disk = false;
    map.with_chunk_write(coord, |chunk| {
        match try_load_chunk_geometry_file(&path, chunk) {
            Ok(true) => {
                loaded_from_disk = true;
                return;
            }
            Ok(false) => {}
            Err(e) => warn!("level geometry `{}`: {e}", path.display()),
        }
        fill_chunk_random(chunk, style_floor_map, coord, chunk_metadata);
        if coord == CENTER_CHUNK {
            if let Err(err) = apply_world_map_file_to_floor(chunk, 0, WORLD_MAP_FILE_PATH) {
                warn!("failed to apply `{WORLD_MAP_FILE_PATH}` to center chunk floor 0: {err}");
            }
            if let Err(err) = apply_world_map_file_to_floor(chunk, 1, WORLD_MAP_FLOOR1_FILE_PATH) {
                warn!("failed to apply `{WORLD_MAP_FLOOR1_FILE_PATH}` to center chunk floor 1: {err}");
            }
        }
    });

    if loaded_from_disk {
        match try_load_chunk_metadata(level_name, coord) {
            Ok(Some(meta)) => chunk_metadata.insert(coord, meta),
            Ok(None) => chunk_metadata.remove(coord),
            Err(e) => warn!("chunk metadata `{}`: {e}", chunk_metadata_path_display(level_name, coord)),
        }
    }

    mirror_chunk_into_passability(map, passability, coord);
    mirror_chunk_into_subtile_cache(map, subtile_cache, coord);

    let floor_style_path = chunk_style_floor_path(level_name, coord);
    if let Err(e) = try_load_chunk_style_file_into_map(&floor_style_path, style_floor_map, coord) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("chunk floor style `{}`: {e}", floor_style_path.display());
        }
    }
    let wall_style_path = chunk_style_wall_path(level_name, coord);
    if let Err(e) = try_load_chunk_style_file_into_map(&wall_style_path, style_wall_map, coord) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("chunk wall style `{}`: {e}", wall_style_path.display());
        }
    }
}

/// Reads every cell of a freshly written world chunk and writes the corresponding
/// passability value into the same chunk of [`HypermapRuntime::static_passability_map`].
fn mirror_chunk_into_passability(
    map: &Hypermap<CellType>,
    passability: &Hypermap<f32>,
    coord: ChunkCoord,
) {
    let Some(world_handle) = map.get_chunk(coord) else {
        return;
    };
    let world = world_handle.read().expect("chunk lock poisoned");
    passability.with_chunk_write(coord, |pchunk| {
        for y in 0..HYPERMAP_CHUNK_SIZE {
            for x in 0..HYPERMAP_CHUNK_SIZE {
                let local = LocalCoord::new(x, y);
                for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
                    let cell = *world.get_local_floor(local, floor);
                    pchunk.set_local_floor(local, floor, cell_passability(cell));
                }
            }
        }
    });
}

/// Builds the per-subtile flag grid for an entire chunk from its cell types.
fn mirror_chunk_into_subtile_cache(
    map: &Hypermap<CellType>,
    subtile_cache: &Hypermap<SubtilePassability>,
    coord: ChunkCoord,
) {
    let Some(world_handle) = map.get_chunk(coord) else {
        return;
    };
    let world = world_handle.read().expect("chunk lock poisoned");
    subtile_cache.with_chunk_write(coord, |sc_chunk| {
        for y in 0..HYPERMAP_CHUNK_SIZE {
            for x in 0..HYPERMAP_CHUNK_SIZE {
                let local = LocalCoord::new(x, y);
                let cell = *world.get_local_floor(local, 0);
                let mut tile = SubtilePassability::EMPTY;
                for sy in 0..SUBTILE_COUNT {
                    for sx in 0..SUBTILE_COUNT {
                        let flags = cell_subtile_flags(cell, sx, sy);
                        if flags != 0 {
                            tile.or_flags(sy, sx, flags);
                        }
                    }
                }
                sc_chunk.set_local(local, tile);
            }
        }
    });
}

/// Writes a world cell **and** mirrors its passability in lock-step. Use this from edit
/// systems instead of touching `runtime.map` directly so the two maps never drift.
pub(crate) fn write_world_cell(
    runtime: &HypermapRuntime,
    world_x: i32,
    world_y: i32,
    floor: i32,
    cell: CellType,
) {
    runtime.map.set_floor(world_x, world_y, floor, cell);
    runtime
        .static_passability_map
        .set_floor(world_x, world_y, floor, cell_passability(cell));

    if floor == 0 {
        let mut tile = SubtilePassability::EMPTY;
        for sy in 0..SUBTILE_COUNT {
            for sx in 0..SUBTILE_COUNT {
                let flags = cell_subtile_flags(cell, sx, sy);
                if flags != 0 {
                    tile.or_flags(sy, sx, flags);
                }
            }
        }
        runtime.static_subtile_cache.set(world_x, world_y, tile);
    }
}

/// Writes the floor quad style for a cell. Call alongside [`write_world_cell`] when painting.
pub(crate) fn write_world_floor_style(
    runtime: &HypermapRuntime,
    world_x: i32,
    world_y: i32,
    floor: i32,
    style: TileStyle,
) {
    runtime.style_floor_map.set_floor(world_x, world_y, floor, style);
}

/// Writes the wall slab style for a cell. Call alongside [`write_world_cell`] when painting walls.
pub(crate) fn write_world_wall_style(
    runtime: &HypermapRuntime,
    world_x: i32,
    world_y: i32,
    floor: i32,
    style: TileStyle,
) {
    runtime.style_wall_map.set_floor(world_x, world_y, floor, style);
}

fn fill_chunk_random(
    chunk: &mut HypermapChunk<CellType>,
    style_floor_map: &Hypermap<TileStyle>,
    coord: ChunkCoord,
    chunk_metadata: &mut ChunkGeneratorMetadata,
) {
    fill_procedural_chunk(chunk, style_floor_map, coord, chunk_metadata);
    clear_upper_floors_to_void(chunk);
}

fn chunk_metadata_path_display(level_name: &str, coord: ChunkCoord) -> String {
    crate::map::chunk_metadata::chunk_metadata_path(level_name, coord)
        .display()
        .to_string()
}

fn reset_style_chunk(style_map: &Hypermap<TileStyle>, coord: ChunkCoord) {
    style_map.with_chunk_write(coord, |chunk| {
        for y in 0..HYPERMAP_CHUNK_SIZE {
            for x in 0..HYPERMAP_CHUNK_SIZE {
                for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
                    chunk.set_local_floor(LocalCoord::new(x, y), floor, TileStyle::DEFAULT);
                }
            }
        }
    });
}

/// Replaces one chunk with fresh procedural geometry (no disk load, no `world_map` overlay).
/// Callers should despawn actors on the chunk and queue a remesh afterward.
pub(crate) fn regenerate_procedural_chunk(
    runtime: &mut HypermapRuntime,
    coord: ChunkCoord,
    level_name: &str,
    dirt: &DirtMap,
    temperature: &TemperatureMap,
) {
    reset_style_chunk(&runtime.style_floor_map, coord);
    reset_style_chunk(&runtime.style_wall_map, coord);
    runtime.map.with_chunk_write(coord, |chunk| {
        fill_chunk_random(
            chunk,
            &runtime.style_floor_map,
            coord,
            &mut runtime.procedural_metadata,
        );
    });
    mirror_chunk_into_passability(&runtime.map, &runtime.static_passability_map, coord);
    mirror_chunk_into_subtile_cache(&runtime.map, &runtime.static_subtile_cache, coord);
    dirt.reset_chunk_for_regeneration(coord);
    temperature.reset_chunk_for_regeneration(coord);
    dirt.ensure_chunk_seeded(&runtime.map, coord, level_name);
    temperature.ensure_chunk_seeded(&runtime.map, coord, level_name);
}

/// Procedural authoring only fills ground floor; upper levels stay open void until edited.
fn clear_upper_floors_to_void(chunk: &mut HypermapChunk<CellType>) {
    let sz = HYPERMAP_CHUNK_SIZE;
    for y in 0..sz {
        for x in 0..sz {
            let local = LocalCoord::new(x, y);
            for floor in 1..HYPERMAP_FLOOR_COUNT as i32 {
                chunk.set_local_floor(local, floor, CellType::Void);
            }
        }
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

    // ── Floor 0: road (default) ──────────────────────────────────────────────
    spawn_floor0_road_entity(commands, meshes, chunk_root, &prepared.floor0_road_cells, ox, oy,
        assets.road_material.clone(), assets.empty_mesh.clone(), "road");
    // ── Floor 0: glass floor ─────────────────────────────────────────────────
    {
        let (mesh3d, vis) =
            if let Some(m) = build_floor0_road_mesh(&prepared.floor0_glass_floor_cells, ox, oy) {
                (Mesh3d(meshes.add(m)), Visibility::Inherited)
            } else {
                (Mesh3d(assets.empty_mesh.clone()), Visibility::Hidden)
            };
        let id = commands
            .spawn((
                Name::new(format!("Chunk floor0 glass floor {},{}", ox, oy)),
                RenderedChunkFloor0GlassFloor,
                mesh3d,
                MeshMaterial3d(assets.glass_road_material.clone()),
                Transform::default(),
                vis,
                Pickable::IGNORE,
            ))
            .id();
        commands.entity(chunk_root).add_child(id);
    }
    // ── Floor 0: pavement ────────────────────────────────────────────────────
    {
        let (mesh3d, vis) =
            if let Some(m) = build_floor0_road_mesh(&prepared.floor0_pavement_cells, ox, oy) {
                (Mesh3d(meshes.add(m)), Visibility::Inherited)
            } else {
                (Mesh3d(assets.empty_mesh.clone()), Visibility::Hidden)
            };
        let id = commands
            .spawn((
                Name::new(format!("Chunk floor0 pavement {},{}", ox, oy)),
                RenderedChunkFloor0Pavement,
                mesh3d,
                MeshMaterial3d(assets.pavement_material.clone()),
                Transform::default(),
                vis,
                Pickable::IGNORE,
            ))
            .id();
        commands.entity(chunk_root).add_child(id);
    }
    // ── Floor 0: marble ──────────────────────────────────────────────────────
    {
        let (mesh3d, vis) =
            if let Some(m) = build_floor0_road_mesh(&prepared.floor0_marble_cells, ox, oy) {
                (Mesh3d(meshes.add(m)), Visibility::Inherited)
            } else {
                (Mesh3d(assets.empty_mesh.clone()), Visibility::Hidden)
            };
        let id = commands
            .spawn((
                Name::new(format!("Chunk floor0 marble {},{}", ox, oy)),
                RenderedChunkFloor0Marble,
                mesh3d,
                MeshMaterial3d(assets.marble_material.clone()),
                Transform::default(),
                vis,
                Pickable::IGNORE,
            ))
            .id();
        commands.entity(chunk_root).add_child(id);
    }
    // ── Floor 0: walls ───────────────────────────────────────────────────────
    {
        let (mesh3d, vis) =
            if let Some(m) = build_floor0_wall_mesh(&prepared.floor0_wall_cells, ox, oy) {
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
    // ── Floor 0: glass walls ─────────────────────────────────────────────────
    {
        let (mesh3d, vis) =
            if let Some(m) = build_floor0_wall_mesh(&prepared.floor0_glass_cells, ox, oy) {
                (Mesh3d(meshes.add(m)), Visibility::Inherited)
            } else {
                (Mesh3d(assets.empty_mesh.clone()), Visibility::Hidden)
            };
        let id = commands
            .spawn((
                Name::new(format!("Chunk floor0 glass walls {},{}", ox, oy)),
                RenderedChunkFloor0GlassWalls,
                mesh3d,
                MeshMaterial3d(assets.glass_wall_material.clone()),
                Transform::default(),
                vis,
                Pickable::IGNORE,
            ))
            .id();
        commands.entity(chunk_root).add_child(id);
    }

    // ── Upper floors ─────────────────────────────────────────────────────────
    let upper_road = spawn_upper_road_entity(
        commands, meshes, chunk_root, &prepared.upper_road_default_cells, ox, oy,
        assets.road_material.clone(), assets.empty_mesh.clone(),
        RenderedChunkUpperRoad, "road",
    );
    let road_glass = spawn_upper_road_entity_no_pick(
        commands, meshes, chunk_root, &prepared.upper_road_glass_cells, ox, oy,
        assets.glass_road_material.clone(), assets.empty_mesh.clone(),
        RenderedChunkUpperGlassFloor, "glass floor",
    );
    let road_pavement = spawn_upper_road_entity_no_pick(
        commands, meshes, chunk_root, &prepared.upper_road_pavement_cells, ox, oy,
        assets.pavement_material.clone(), assets.empty_mesh.clone(),
        RenderedChunkUpperPavement, "pavement",
    );
    let road_marble = spawn_upper_road_entity_no_pick(
        commands, meshes, chunk_root, &prepared.upper_road_marble_cells, ox, oy,
        assets.marble_material.clone(), assets.empty_mesh.clone(),
        RenderedChunkUpperMarble, "marble",
    );

    let upper_walls = {
        let (mesh3d, vis) =
            if let Some(m) = build_upper_wall_mesh(&prepared.upper_cells, ox, oy) {
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

    let upper_glass_walls = {
        let (mesh3d, vis) =
            if let Some(m) = build_upper_wall_mesh(&prepared.upper_glass_cells, ox, oy) {
                (Mesh3d(meshes.add(m)), Visibility::Inherited)
            } else {
                (Mesh3d(assets.empty_mesh.clone()), Visibility::Hidden)
            };
        let id = commands
            .spawn((
                Name::new(format!("Chunk upper glass walls {},{}", ox, oy)),
                RenderedChunkUpperGlassWalls,
                mesh3d,
                MeshMaterial3d(assets.glass_wall_material.clone()),
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
        road_glass,
        road_pavement,
        road_marble,
        walls: upper_walls,
        glass_walls: upper_glass_walls,
    }
}

fn spawn_floor0_road_entity(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    chunk_root: Entity,
    cells: &[(i32, i32, CellType)],
    ox: i32,
    oy: i32,
    material: Handle<StandardMaterial>,
    empty_mesh: Handle<Mesh>,
    label: &str,
) {
    let (mesh3d, vis, pick) =
        if let Some(m) = build_floor0_road_mesh(cells, ox, oy) {
            (Mesh3d(meshes.add(m)), Visibility::Inherited, Pickable::default())
        } else {
            (Mesh3d(empty_mesh), Visibility::Hidden, Pickable::IGNORE)
        };
    let id = commands
        .spawn((
            Name::new(format!("Chunk floor0 {} {},{}", label, ox, oy)),
            RenderedChunkFloor0Road,
            mesh3d,
            MeshMaterial3d(material),
            Transform::default(),
            vis,
            pick,
        ))
        .observe(floor_grid_click)
        .id();
    commands.entity(chunk_root).add_child(id);
}

fn spawn_upper_road_entity(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    chunk_root: Entity,
    cells: &[(i32, i32, i32, CellType)],
    ox: i32,
    oy: i32,
    material: Handle<StandardMaterial>,
    empty_mesh: Handle<Mesh>,
    marker: impl Component,
    label: &str,
) -> Entity {
    let (mesh3d, vis, pick) =
        if let Some(m) = build_upper_road_mesh(cells, ox, oy) {
            (Mesh3d(meshes.add(m)), Visibility::Inherited, Pickable::default())
        } else {
            (Mesh3d(empty_mesh), Visibility::Hidden, Pickable::IGNORE)
        };
    let id = commands
        .spawn((
            Name::new(format!("Chunk upper {} {},{}", label, ox, oy)),
            marker,
            mesh3d,
            MeshMaterial3d(material),
            Transform::default(),
            vis,
            pick,
        ))
        .observe(floor_grid_click)
        .id();
    commands.entity(chunk_root).add_child(id);
    id
}

fn spawn_upper_road_entity_no_pick(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    chunk_root: Entity,
    cells: &[(i32, i32, i32, CellType)],
    ox: i32,
    oy: i32,
    material: Handle<StandardMaterial>,
    empty_mesh: Handle<Mesh>,
    marker: impl Component,
    label: &str,
) -> Entity {
    let (mesh3d, vis) =
        if let Some(m) = build_upper_road_mesh(cells, ox, oy) {
            (Mesh3d(meshes.add(m)), Visibility::Inherited)
        } else {
            (Mesh3d(empty_mesh), Visibility::Hidden)
        };
    let id = commands
        .spawn((
            Name::new(format!("Chunk upper {} {},{}", label, ox, oy)),
            marker,
            mesh3d,
            MeshMaterial3d(material),
            Transform::default(),
            vis,
            Pickable::IGNORE,
        ))
        .id();
    commands.entity(chunk_root).add_child(id);
    id
}

pub(crate) fn build_floor0_road_mesh(cells: &[(i32, i32, CellType)], origin_x: i32, origin_y: i32) -> Option<Mesh> {
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

pub(crate) fn build_floor0_wall_mesh(cells: &[(i32, i32, CellType)], origin_x: i32, origin_y: i32) -> Option<Mesh> {
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

pub(crate) fn build_upper_road_mesh(cells: &[(i32, i32, i32, CellType)], origin_x: i32, origin_y: i32) -> Option<Mesh> {
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

pub(crate) fn build_upper_wall_mesh(cells: &[(i32, i32, i32, CellType)], origin_x: i32, origin_y: i32) -> Option<Mesh> {
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

fn clone_chunk_for_render_start(chunk: &HypermapChunk<CellType>) -> Vec<CellType> {
    let mut cells =
        Vec::with_capacity(HYPERMAP_CHUNK_SIZE as usize * HYPERMAP_CHUNK_SIZE as usize * HYPERMAP_FLOOR_COUNT);
    for y in 0..HYPERMAP_CHUNK_SIZE {
        for x in 0..HYPERMAP_CHUNK_SIZE {
            let local = LocalCoord::new(x, y);
            for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
                cells.push(*chunk.get_local_floor(local, floor));
            }
        }
    }
    cells
}

fn clone_styles_for_render_start(chunk: &HypermapChunk<TileStyle>) -> Vec<TileStyle> {
    let mut styles =
        Vec::with_capacity(HYPERMAP_CHUNK_SIZE as usize * HYPERMAP_CHUNK_SIZE as usize * HYPERMAP_FLOOR_COUNT);
    for y in 0..HYPERMAP_CHUNK_SIZE {
        for x in 0..HYPERMAP_CHUNK_SIZE {
            let local = LocalCoord::new(x, y);
            for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
                styles.push(*chunk.get_local_floor(local, floor));
            }
        }
    }
    styles
}

fn wall_is_glass(style: TileStyle) -> bool {
    style.0 == [b'w', b'g']
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FloorBucket {
    Road,
    Glass,
    Pavement,
    Marble,
}

fn floor_bucket(style: TileStyle) -> FloorBucket {
    match style.0 {
        [b'f', b'g'] => FloorBucket::Glass,
        [b'f', b'p'] => FloorBucket::Pavement,
        [b'f', b'm'] => FloorBucket::Marble,
        _ => FloorBucket::Road,
    }
}

fn partition_chunk_cells_from_vec(
    snapshot: Vec<(CellType, TileStyle, TileStyle)>,
    active_floor: i32,
) -> PreparedChunkRender {
    let mut has_void_floor0_water = false;
    let mut floor0_road_cells = Vec::new();
    let mut floor0_glass_floor_cells = Vec::new();
    let mut floor0_pavement_cells = Vec::new();
    let mut floor0_marble_cells = Vec::new();
    let mut floor0_wall_cells = Vec::new();
    let mut floor0_glass_cells = Vec::new();
    let mut upper_cells = Vec::new();
    let mut upper_glass_cells = Vec::new();
    let mut upper_road_default_cells = Vec::new();
    let mut upper_road_glass_cells = Vec::new();
    let mut upper_road_pavement_cells = Vec::new();
    let mut upper_road_marble_cells = Vec::new();
    let w = WATER_MESH_EDGE_STRIP;
    let sz = HYPERMAP_CHUNK_SIZE;
    let mut i = 0usize;
    for y in 0..HYPERMAP_CHUNK_SIZE {
        for x in 0..HYPERMAP_CHUNK_SIZE {
            for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
                let (cell_type, floor_style, wall_style) = snapshot[i];
                i += 1;
                if cell_type == CellType::Void {
                    if floor == 0 {
                        let interior = x >= w && y >= w && x < sz - w && y < sz - w;
                        if interior {
                            has_void_floor0_water = true;
                        }
                    }
                    continue;
                }
                if floor == 0 {
                    match cell_type {
                        CellType::Wall(_) | CellType::Corner(_) => {
                            // Floor quad uses floor_style (walls can now have any floor material).
                            match floor_bucket(floor_style) {
                                FloorBucket::Glass => floor0_glass_floor_cells.push((x, y, cell_type)),
                                FloorBucket::Pavement => floor0_pavement_cells.push((x, y, cell_type)),
                                FloorBucket::Marble => floor0_marble_cells.push((x, y, cell_type)),
                                FloorBucket::Road => floor0_road_cells.push((x, y, cell_type)),
                            }
                            // Wall slab uses wall_style.
                            if wall_is_glass(wall_style) {
                                floor0_glass_cells.push((x, y, cell_type));
                            } else {
                                floor0_wall_cells.push((x, y, cell_type));
                            }
                        }
                        _ => {
                            // Road cell: floor quad bucketed by floor_style.
                            match floor_bucket(floor_style) {
                                FloorBucket::Glass => floor0_glass_floor_cells.push((x, y, cell_type)),
                                FloorBucket::Pavement => floor0_pavement_cells.push((x, y, cell_type)),
                                FloorBucket::Marble => floor0_marble_cells.push((x, y, cell_type)),
                                FloorBucket::Road => floor0_road_cells.push((x, y, cell_type)),
                            }
                        }
                    }
                } else if floor == active_floor {
                    match cell_type {
                        CellType::Wall(_) | CellType::Corner(_) => {
                            // Floor quad uses floor_style.
                            match floor_bucket(floor_style) {
                                FloorBucket::Glass => upper_road_glass_cells.push((x, y, floor, cell_type)),
                                FloorBucket::Pavement => upper_road_pavement_cells.push((x, y, floor, cell_type)),
                                FloorBucket::Marble => upper_road_marble_cells.push((x, y, floor, cell_type)),
                                FloorBucket::Road => upper_road_default_cells.push((x, y, floor, cell_type)),
                            }
                            // Wall slab uses wall_style.
                            if wall_is_glass(wall_style) {
                                upper_glass_cells.push((x, y, floor, cell_type));
                            } else {
                                upper_cells.push((x, y, floor, cell_type));
                            }
                        }
                        _ => {
                            // Upper road cell: floor quad bucketed by floor_style.
                            match floor_bucket(floor_style) {
                                FloorBucket::Glass => upper_road_glass_cells.push((x, y, floor, cell_type)),
                                FloorBucket::Pavement => upper_road_pavement_cells.push((x, y, floor, cell_type)),
                                FloorBucket::Marble => upper_road_marble_cells.push((x, y, floor, cell_type)),
                                FloorBucket::Road => upper_road_default_cells.push((x, y, floor, cell_type)),
                            }
                        }
                    }
                }
            }
        }
    }
    PreparedChunkRender {
        has_void_floor0_water,
        floor0_road_cells,
        floor0_glass_floor_cells,
        floor0_pavement_cells,
        floor0_marble_cells,
        floor0_wall_cells,
        floor0_glass_cells,
        upper_cells,
        upper_glass_cells,
        upper_road_default_cells,
        upper_road_glass_cells,
        upper_road_pavement_cells,
        upper_road_marble_cells,
    }
}

fn build_chunk_render_data(snapshot: Vec<(CellType, TileStyle, TileStyle)>, active_floor: i32) -> PreparedChunkRender {
    partition_chunk_cells_from_vec(snapshot, active_floor)
}

/// Exactly three chunks: camera chunk plus one neighbor on each axis toward the
/// nearer chunk border (prefetch ahead of panning).
fn target_chunks_for(center: ChunkCoord, local: LocalCoord) -> HashSet<ChunkCoord> {
    let mid = HYPERMAP_CHUNK_SIZE / 2;
    let x_neighbor = if local.x >= mid {
        ChunkCoord::new(center.x + 1, center.y)
    } else {
        ChunkCoord::new(center.x - 1, center.y)
    };
    let y_neighbor = if local.y >= mid {
        ChunkCoord::new(center.x, center.y + 1)
    } else {
        ChunkCoord::new(center.x, center.y - 1)
    };
    HashSet::from([center, x_neighbor, y_neighbor])
}

fn is_in_center_dead_zone(local: LocalCoord) -> bool {
    let center = HYPERMAP_CHUNK_SIZE / 2;
    let half = DEAD_ZONE_SIZE / 2;
    let min = center - half;
    let max = center + half;
    local.x >= min && local.x < max && local.y >= min && local.y < max
}

fn apply_world_map_file_to_floor(
    chunk: &mut HypermapChunk<CellType>,
    floor: i32,
    path: &str,
) -> Result<(), String> {
    let text = std::fs::read_to_string(Path::new(path))
        .map_err(|err| format!("read `{path}`: {err}"))?;
    let map = WorldMapFloor::from_ascii(&text).map_err(|err| format!("parse `{path}`: {err}"))?;

    let start_x = (HYPERMAP_CHUNK_SIZE - map.width() as i32) / 2;
    let start_y = (HYPERMAP_CHUNK_SIZE - map.height() as i32) / 2;

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
            let local = LocalCoord::new(local_x, local_y);
            chunk.set_local_floor(local, floor, cell.get_cell_type());
        }
    }

    Ok(())
}

#[cfg(test)]
mod visibility_tests {
    use super::*;
    use crate::map::hypermap::ChunkCoord;

    #[test]
    fn target_chunks_always_three() {
        let center = ChunkCoord::new(0, 0);
        for &(lx, ly) in &[(0, 0), (63, 63), (64, 64), (127, 127)] {
            let set = target_chunks_for(center, LocalCoord::new(lx, ly));
            assert_eq!(set.len(), 3, "local ({lx},{ly})");
            assert!(set.contains(&center));
        }
    }

    #[test]
    fn target_chunks_prefetch_toward_camera_side() {
        let center = ChunkCoord::new(0, 0);
        let west_south = target_chunks_for(center, LocalCoord::new(10, 10));
        assert!(west_south.contains(&ChunkCoord::new(-1, 0)));
        assert!(west_south.contains(&ChunkCoord::new(0, -1)));

        let east_north = target_chunks_for(center, LocalCoord::new(100, 100));
        assert!(east_north.contains(&ChunkCoord::new(1, 0)));
        assert!(east_north.contains(&ChunkCoord::new(0, 1)));
    }
}

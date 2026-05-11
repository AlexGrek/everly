//! BlackBot — a ground-walking metallic black sphere that pathfinds to
//! random nearby points.
//!
//! The bot picks a random walkable tile within 15 tiles of its current
//! position, computes an A* path, and follows it step by step. When the
//! destination is reached or unreachable it picks a new target. It uses
//! the default ground-walker traversal rules (blocked by walls and void).

use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::actor::{process_actors, Actor, ActorMoveBuffer, ActorObject, ActorState};
use crate::map::chunk_overlay::{ChunkOverlayState, OVERLAY_RES};
use crate::map::hypermap::{world_to_chunk_local, ChunkCoord};
use crate::map::hypermap_pathfind::{
    astar_shortest_world_path, HypermapPathResult, HypermapSearchLimits,
};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::passability::SUBTILE_COUNT;
use crate::menu::main_menu::GameState;

const BLACK_RADIUS_SUBTILES: i32 = 2;
const SPEED_SUBTILES_PER_S: f32 = 6.0;
const SPHERE_RADIUS: f32 = 0.45;
const WANDER_RADIUS: f32 = 15.0;
const MAX_TARGET_ATTEMPTS: u32 = 8;

#[derive(Component)]
pub struct BlackBotVisual {
    accumulator: Vec2,
    direction: Vec2,
    path: Vec<(i32, i32)>,
    path_index: usize,
    rng: StdRng,
}

impl BlackBotVisual {
    /// Returns the final destination tile the bot is walking toward, or `None`
    /// if it has no active path.
    pub fn target_tile(&self) -> Option<(i32, i32)> {
        self.path.last().copied()
    }
}

#[derive(Resource)]
pub struct BlackBotRng(pub StdRng);

impl Default for BlackBotRng {
    fn default() -> Self {
        Self(StdRng::seed_from_u64(0xB1AC_B07))
    }
}

pub struct BlackBot {
    state: ActorState,
}

impl BlackBot {
    pub fn new(center: Vec2) -> Self {
        let sc = SUBTILE_COUNT as f32;
        let initial_sub = IVec2::new(
            (center.x * sc).floor() as i32,
            (center.y * sc).floor() as i32,
        );
        Self {
            state: ActorState {
                center,
                radius_subtiles: BLACK_RADIUS_SUBTILES,
                rotation: 0.0,
                move_buffer: ActorMoveBuffer::default(),
                last_movement_error: None,
                last_accepted_center_subtile: Some(initial_sub),
                last_accepted_radius_subtiles: BLACK_RADIUS_SUBTILES,
            },
        }
    }
}

impl Actor for BlackBot {
    fn state(&self) -> &ActorState {
        &self.state
    }

    fn state_mut(&mut self) -> &mut ActorState {
        &mut self.state
    }
}

pub struct BlackBotPlugin;

impl Plugin for BlackBotPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BlackBotRng>().add_systems(
            Update,
            (
                black_bot_think.before(process_actors),
                sync_black_bot_transforms.after(process_actors),
                paint_black_bot_targets.after(process_actors),
            )
                .run_if(in_state(GameState::InGame)),
        );
    }
}

fn pick_random_target(
    rng: &mut StdRng,
    current_tile: (i32, i32),
    passability: &crate::map::hypermap::Hypermap<f32>,
) -> Option<Vec<(i32, i32)>> {
    for _ in 0..MAX_TARGET_ATTEMPTS {
        let dx: f32 = rng.gen_range(-WANDER_RADIUS..WANDER_RADIUS);
        let dy: f32 = rng.gen_range(-WANDER_RADIUS..WANDER_RADIUS);
        if dx * dx + dy * dy > WANDER_RADIUS * WANDER_RADIUS {
            continue;
        }
        let target = (
            current_tile.0 + dx.round() as i32,
            current_tile.1 + dy.round() as i32,
        );
        if target == current_tile {
            continue;
        }
        let result = astar_shortest_world_path(
            passability,
            current_tile,
            target,
            HypermapSearchLimits { max_expanded: 2000 },
        );
        if let HypermapPathResult::Found { path, .. } = result {
            if path.len() > 1 {
                return Some(path);
            }
        }
    }
    None
}

fn black_bot_think(
    time: Res<Time>,
    hypermap: Res<HypermapRuntime>,
    mut query: Query<(&mut ActorObject, &mut BlackBotVisual)>,
) {
    let dt = time.delta_secs();
    let passability = &*hypermap.static_passability_map;

    for (mut obj, mut vis) in &mut query {
        let state = obj.inner.state_mut();

        let current_sub = state
            .last_accepted_center_subtile
            .unwrap_or_else(|| state.center_subtile_i32());
        let sc = SUBTILE_COUNT as i32;
        let current_tile = (
            current_sub.x.div_euclid(sc),
            current_sub.y.div_euclid(sc),
        );

        let needs_new_target = if state.last_movement_error.is_some() {
            vis.accumulator = Vec2::ZERO;
            true
        } else if vis.path.is_empty() || vis.path_index >= vis.path.len() {
            true
        } else {
            let target_tile = vis.path[vis.path_index];
            if current_tile == target_tile {
                vis.path_index += 1;
                vis.path_index >= vis.path.len()
            } else {
                false
            }
        };

        if needs_new_target {
            if let Some(path) = pick_random_target(&mut vis.rng, current_tile, passability) {
                vis.path = path;
                vis.path_index = 1;
                vis.accumulator = Vec2::ZERO;
            } else {
                vis.path.clear();
                vis.path_index = 0;
                state.move_buffer.tile_delta = Vec2::ZERO;
                state.move_buffer.subtile_shift = IVec2::ZERO;
                state.move_buffer.rotation_shift = 0.0;
                continue;
            }
        }

        let next_tile = vis.path[vis.path_index];
        let dir = Vec2::new(
            (next_tile.0 - current_tile.0) as f32,
            (next_tile.1 - current_tile.1) as f32,
        );
        let dir = if dir.length_squared() > 0.0 {
            dir.normalize()
        } else {
            Vec2::X
        };
        vis.direction = dir;

        let delta = dir * SPEED_SUBTILES_PER_S * dt;
        vis.accumulator += delta;

        let step_x = vis.accumulator.x.trunc() as i32;
        let step_y = vis.accumulator.y.trunc() as i32;
        vis.accumulator.x -= step_x as f32;
        vis.accumulator.y -= step_y as f32;

        state.move_buffer.tile_delta = Vec2::ZERO;
        state.move_buffer.subtile_shift = IVec2::new(step_x, step_y);
        state.move_buffer.rotation_shift = 0.0;
    }
}

fn sync_black_bot_transforms(
    actors: Query<(&ActorObject, &BlackBotVisual, &Children)>,
    mut children_data: Query<Option<&mut Transform>, Without<ActorObject>>,
) {
    for (obj, vis, children) in &actors {
        let s = obj.inner.state();
        let sub = s
            .last_accepted_center_subtile
            .unwrap_or_else(|| s.center_subtile_i32());
        let sc = SUBTILE_COUNT as f32;
        let world_pos = (sub.as_vec2() + vis.accumulator) / sc;

        for child in children.iter() {
            if let Ok(Some(mut t)) = children_data.get_mut(child) {
                t.translation = Vec3::new(world_pos.x, SPHERE_RADIUS, world_pos.y);
            }
        }
    }
}

const TARGET_COLOR: [u8; 4] = [180, 40, 220, 160];
const TARGET_HALO_COLOR: [u8; 4] = [180, 40, 220, 60];

fn paint_black_bot_targets(
    overlay: Res<ChunkOverlayState>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    bots: Query<&BlackBotVisual>,
) {
    let mut touched_chunks: std::collections::HashSet<ChunkCoord> = std::collections::HashSet::new();
    let res = OVERLAY_RES as usize;
    let sc = SUBTILE_COUNT;

    // Collect all target tiles first.
    let targets: Vec<(i32, i32)> = bots
        .iter()
        .filter_map(|vis| vis.target_tile())
        .collect();

    // Clear previous target marks on all visible overlay images.
    // We repaint every frame so stale marks from bots that changed target
    // don't linger. Only the generic layer's pixels that belong to this
    // feature are cleared — we use a dedicated "stamp and clear" approach
    // limited to a 3×3 tile neighbourhood around each previous/current target.
    // For simplicity and correctness, clear the entire overlay each frame
    // (all texels are transparent by default, and only this system writes
    // to the generic layer right now).
    for coord in overlay.iter_coords() {
        let Some(img_h) = overlay.image_for(coord) else { continue };
        let Some(image) = images.get_mut(img_h) else { continue };
        let Some(data) = image.data.as_mut() else { continue };
        data.fill(0);
        touched_chunks.insert(coord);
    }

    // Paint each target as a highlighted tile with a 1-tile halo.
    for &(wx, wy) in &targets {
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                let tx = wx + dx;
                let ty = wy + dy;
                let (coord, local) = world_to_chunk_local(tx, ty);
                let Some(img_h) = overlay.image_for(coord) else { continue };
                let Some(image) = images.get_mut(img_h) else { continue };
                let Some(data) = image.data.as_mut() else { continue };

                let is_center = dx == 0 && dy == 0;
                let color = if is_center { TARGET_COLOR } else { TARGET_HALO_COLOR };

                let base_px = local.x as usize * sc;
                let base_py = local.y as usize * sc;
                for sy in 0..sc {
                    for sx in 0..sc {
                        let px = base_px + sx;
                        let py = base_py + sy;
                        let idx = (py * res + px) * 4;
                        let existing_a = data[idx + 3] as u16;
                        let new_a = color[3] as u16;
                        if new_a > existing_a {
                            data[idx..idx + 4].copy_from_slice(&color);
                        }
                    }
                }
                touched_chunks.insert(coord);
            }
        }
    }

    for coord in &touched_chunks {
        if let Some(mat_h) = overlay.material_for(*coord) {
            materials.get_mut(mat_h);
        }
    }
}

pub fn spawn_black_bot(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    rng: &mut StdRng,
    center: Vec2,
) -> Entity {
    let vis_seed: u64 = rng.gen_range(0..u64::MAX);
    let vis_rng = StdRng::seed_from_u64(vis_seed);
    let bot = BlackBot::new(center);

    let mat = materials.add(StandardMaterial {
        base_color: Color::srgb(0.02, 0.02, 0.02),
        metallic: 1.0,
        perceptual_roughness: 0.05,
        reflectance: 1.0,
        ..default()
    });
    let mesh = meshes.add(Sphere::new(SPHERE_RADIUS).mesh().ico(3).unwrap());

    let sc = SUBTILE_COUNT as f32;
    let accumulator = Vec2::new(
        (center.x * sc).rem_euclid(1.0),
        (center.y * sc).rem_euclid(1.0),
    );

    let parent = commands
        .spawn((
            Name::new("BlackBot"),
            BlackBotVisual {
                accumulator,
                direction: Vec2::X,
                path: Vec::new(),
                path_index: 0,
                rng: vis_rng,
            },
            ActorObject::new(Box::new(bot)),
            Transform::default(),
            Visibility::Inherited,
        ))
        .id();

    let mesh_child = commands
        .spawn((
            Name::new("BlackBot mesh"),
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(center.x, SPHERE_RADIUS, center.y),
        ))
        .id();

    commands.entity(parent).add_children(&[mesh_child]);
    parent
}

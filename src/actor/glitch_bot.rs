//! GlitchBot — a flying, glowing sphere that wanders randomly.
//!
//! The bot picks a random direction every 3–5 s and moves continuously at a
//! fixed speed (subtiles/s). Fractional subtile displacement is accumulated
//! over frames; integer steps are emitted into `move_buffer` only when enough
//! has built up. On collision the bot immediately stops (zeroes accumulator)
//! and tries escape directions (flip, then perpendicular, then random). It
//! occupies a circle of radius 2 subtiles
//! (diameter ≈ 5 subtiles ≈ 1 tile).

use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::actor::actor_name::random_actor_name;
use crate::actor::actor_pick::{ActorInspectable, ActorPickMesh};
use crate::actor::charge::Charge;
use bevy::picking::prelude::Pickable;
use crate::actor::snapshot::{GlitchBotVisualSnap, SerVec2};
use crate::actor::{is_paused, process_actors, Actor, ActorMoveBuffer, ActorObject, ActorState, OffScreenActor};
use crate::map::passability::{FLAG_BLOCKED, SUBTILE_COUNT};
use crate::menu::main_menu::GameState;

const GLITCH_RADIUS_SUBTILES: i32 = 2;
const DIR_CHANGE_MIN_S: f32 = 3.0;
const DIR_CHANGE_MAX_S: f32 = 5.0;
/// Continuous speed in subtiles per second.
const SPEED_SUBTILES_PER_S: f32 = 8.0;
const HOVER_HEIGHT: f32 = 0.6;
const SPHERE_RADIUS: f32 = 0.5;

/// Per-entity state that drives continuous movement between direction changes.
///
/// Lives on the same entity as `ActorObject`. The `glitch_bot_think` system
/// ticks it each frame **before** `process_actors`, so `move_buffer` is ready
/// when `try_move` fires.
#[derive(Component)]
pub struct GlitchBotVisual {
    /// Normalized direction the bot is currently heading (subtile-space).
    direction: Vec2,
    /// Fractional subtile displacement accumulated across frames. When any
    /// component reaches ±1.0, the integer part becomes a `subtile_shift`
    /// step and the remainder stays for the next frame.
    accumulator: Vec2,
    dir_timer: f32,
    dir_interval: f32,
    collision_streak: u32,
    /// Seed for [`Self::rng`]; persisted in level actor snapshots only.
    rng_seed: u64,
    rng: StdRng,
}

impl GlitchBotVisual {
    pub(crate) fn to_snapshot(&self) -> GlitchBotVisualSnap {
        GlitchBotVisualSnap {
            direction: SerVec2 {
                x: self.direction.x,
                y: self.direction.y,
            },
            accumulator: SerVec2 {
                x: self.accumulator.x,
                y: self.accumulator.y,
            },
            dir_timer: self.dir_timer,
            dir_interval: self.dir_interval,
            collision_streak: self.collision_streak,
            rng_seed: self.rng_seed,
        }
    }

    fn from_snapshot(snap: GlitchBotVisualSnap) -> Self {
        Self {
            direction: Vec2::new(snap.direction.x, snap.direction.y),
            accumulator: Vec2::new(snap.accumulator.x, snap.accumulator.y),
            dir_timer: snap.dir_timer,
            dir_interval: snap.dir_interval,
            collision_streak: snap.collision_streak,
            rng_seed: snap.rng_seed,
            rng: StdRng::seed_from_u64(snap.rng_seed),
        }
    }

    pub fn direction(&self) -> Vec2 {
        self.direction
    }

    pub fn collision_streak(&self) -> u32 {
        self.collision_streak
    }
}

/// Seeded RNG resource shared across bot spawns for deterministic colors/seeds.
#[derive(Resource)]
pub struct GlitchBotRng(pub StdRng);

impl Default for GlitchBotRng {
    fn default() -> Self {
        Self(StdRng::seed_from_u64(0xB07_CAFE))
    }
}

pub struct GlitchBot {
    state: ActorState,
}

impl GlitchBot {
    pub fn from_state(state: ActorState) -> Self {
        Self { state }
    }

    pub fn new(center: Vec2) -> Self {
        let sc = SUBTILE_COUNT as f32;
        // Pre-compute the containing subtile so try_move never falls back to
        // center_subtile_i32 on the first frame.  Floor gives the subtile that
        // contains the position; round would mis-place actors spawned at tile
        // centres (fractional part 0.5) by a full subtile (0.2 m).
        let initial_sub = IVec2::new(
            (center.x * sc).floor() as i32,
            (center.y * sc).floor() as i32,
        );
        Self {
            state: ActorState {
                center,
                radius_subtiles: GLITCH_RADIUS_SUBTILES,
                rotation: 0.0,
                move_buffer: ActorMoveBuffer::default(),
                last_movement_error: None,
                last_accepted_center_subtile: Some(initial_sub),
                last_accepted_radius_subtiles: GLITCH_RADIUS_SUBTILES,
                next_waypoint_hint: None,
                field_main_tile: None,
            },
        }
    }
}

impl Actor for GlitchBot {
    fn state(&self) -> &ActorState {
        &self.state
    }

    fn state_mut(&mut self) -> &mut ActorState {
        &mut self.state
    }

    /// GlitchBot flies: it crosses void freely but is stopped by wall geometry
    /// (`FLAG_BLOCKED`). The static geometry stamper already bakes subtile-
    /// accurate wall/corner flags, so no further per-actor check is needed.
    fn blocked_flags(&self) -> u64 {
        FLAG_BLOCKED
    }
}

fn random_direction(rng: &mut StdRng) -> Vec2 {
    let angle: f32 = rng.gen_range(0.0..std::f32::consts::TAU);
    Vec2::new(angle.cos(), angle.sin())
}

fn direction_color(direction: Vec2) -> Color {
    let h = direction.y.atan2(direction.x).to_degrees().rem_euclid(360.0);
    Color::hsl(h, 0.95, 0.6)
}


pub struct GlitchBotPlugin;

impl Plugin for GlitchBotPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GlitchBotRng>().add_systems(
            Update,
            (
                glitch_bot_think
                    .before(process_actors)
                    .run_if(not(is_paused)),
                sync_glitch_bot_transforms.after(process_actors),
            )
                .run_if(in_state(GameState::InGame)),
        );
    }
}

/// Per-frame: advance the sub-subtile accumulator and emit integer subtile steps.
///
/// The canonical position is `last_accepted_center_subtile` (integer).
/// `accumulator` is the fractional progress within the current subtile [−1, 1),
/// used only for smooth rendering.  Both are reset to zero on every direction
/// change so they can never diverge.
fn glitch_bot_think(
    time: Res<Time>,
    mut query: Query<(&mut ActorObject, &mut GlitchBotVisual, Option<&Charge>)>,
) {
    let dt = time.delta_secs();

    for (mut obj, mut vis, charge) in &mut query {
        let state = obj.inner.state_mut();

        // Depleted bots are immobilized: clear any pending intent and freeze the
        // accumulator so the bot neither steps on the collision grid nor drifts
        // visually (sync renders from last_accepted_center_subtile + accumulator).
        if charge.is_some_and(Charge::is_depleted) {
            state.move_buffer = ActorMoveBuffer::default();
            continue;
        }

        if state.last_movement_error.is_some() {
            vis.collision_streak = vis.collision_streak.saturating_add(1);
            vis.direction = match vis.collision_streak {
                1 => -vis.direction,
                2 => {
                    let turn_left = vis.rng.gen_range(0..2) == 0;
                    if turn_left {
                        Vec2::new(-vis.direction.y, vis.direction.x)
                    } else {
                        Vec2::new(vis.direction.y, -vis.direction.x)
                    }
                }
                _ => {
                    vis.collision_streak = 0;
                    random_direction(&mut vis.rng)
                }
            };
            vis.accumulator = Vec2::ZERO;
        } else {
            vis.collision_streak = 0;
        }

        vis.dir_timer += dt;
        if vis.dir_timer >= vis.dir_interval {
            vis.direction = random_direction(&mut vis.rng);
            vis.dir_timer = 0.0;
            vis.dir_interval = vis.rng.gen_range(DIR_CHANGE_MIN_S..DIR_CHANGE_MAX_S);
            vis.accumulator = Vec2::ZERO;
        }

        let delta = vis.direction * SPEED_SUBTILES_PER_S * dt;
        vis.accumulator += delta;

        // Integer steps toward zero: fires only after a full subtile of motion.
        let step_x = vis.accumulator.x.trunc() as i32;
        let step_y = vis.accumulator.y.trunc() as i32;
        vis.accumulator.x -= step_x as f32;
        vis.accumulator.y -= step_y as f32;

        // Advance center by the integer subtile amount so the field stays roughly
        // synchronized with last_accepted_center_subtile. sync_glitch_bot_transforms
        // uses last_accepted_center_subtile + accumulator for sub-subtile smooth
        // rendering and does not read center, but other systems (snapshots, future
        // queries) may read it and must not see a permanently stale spawn position.
        let sc = SUBTILE_COUNT as f32;
        state.move_buffer.tile_delta = Vec2::new(step_x as f32 / sc, step_y as f32 / sc);
        state.move_buffer.subtile_shift = IVec2::new(step_x, step_y);
        state.move_buffer.rotation_shift = 0.0;
    }
}

/// Keeps the 3D mesh child aligned with the actor's subtile position.
///
/// World position is derived from `last_accepted_center_subtile` (the integer
/// collision grid) plus the fractional `accumulator` (sub-subtile smooth offset).
/// This makes the visual and the footprint identical sources of truth — the
/// accumulator is always reset on direction change, so drift is impossible.
fn sync_glitch_bot_transforms(
    actors: Query<(&ActorObject, &GlitchBotVisual, &Children), Without<OffScreenActor>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut children_data: Query<
        (
            Option<&MeshMaterial3d<StandardMaterial>>,
            Option<&mut PointLight>,
            Option<&mut Transform>,
        ),
        Without<ActorObject>,
    >,
) {
    for (obj, vis, children) in &actors {
        let s = obj.inner.state();
        let sub = s.last_accepted_center_subtile.unwrap_or_else(|| s.center_subtile_i32());
        let sc = SUBTILE_COUNT as f32;
        let world_pos = (sub.as_vec2() + vis.accumulator) / sc;
        let world_x = world_pos.x;
        let world_z = world_pos.y;
        let color = direction_color(vis.direction);
        let emissive = color.to_linear() * 4.0;

        for child in children.iter() {
            if let Ok((mesh_mat, light, transform)) = children_data.get_mut(child) {
                if let Some(mut t) = transform {
                    t.translation = Vec3::new(world_x, HOVER_HEIGHT, world_z);
                }
                if let Some(mat_handle) = mesh_mat {
                    if let Some(mat) = materials.get_mut(&mat_handle.0) {
                        mat.base_color = color;
                        mat.emissive = emissive;
                    }
                }
                if let Some(mut light) = light {
                    light.color = color;
                }
            }
        }
    }
}

fn glitch_bot_world_position(state: &ActorState, accumulator: Vec2) -> Vec3 {
    let sub = state
        .last_accepted_center_subtile
        .unwrap_or_else(|| state.center_subtile_i32());
    let sc = SUBTILE_COUNT as f32;
    let world_pos = (sub.as_vec2() + accumulator) / sc;
    Vec3::new(world_pos.x, HOVER_HEIGHT, world_pos.y)
}

/// Spawns a GlitchBot from a level snapshot (mesh + restored runtime state).
pub fn spawn_glitch_bot_from_snapshot(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    name: &str,
    state: ActorState,
    visual: GlitchBotVisualSnap,
) -> Entity {
    let bot = GlitchBot::from_state(state);
    let vis = GlitchBotVisual::from_snapshot(visual);
    let direction = vis.direction;
    let color = direction_color(direction);
    let world_pos = glitch_bot_world_position(bot.state(), vis.accumulator);

    let emissive_color = color.to_linear();
    let mat = materials.add(StandardMaterial {
        base_color: color,
        emissive: emissive_color * 4.0,
        ..default()
    });
    let mesh = meshes.add(Sphere::new(SPHERE_RADIUS).mesh().ico(3).unwrap());

    let parent = commands
        .spawn((
            Name::new(name.to_string()),
            ActorInspectable,
            vis,
            ActorObject::new(Box::new(bot)),
            Transform::default(),
            Visibility::Inherited,
        ))
        .id();

    let mesh_child = commands
        .spawn((
            Name::new("GlitchBot mesh"),
            ActorPickMesh,
            Pickable::default(),
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_translation(world_pos),
        ))
        .id();

    let light_child = commands
        .spawn((
            Name::new("GlitchBot light"),
            PointLight {
                color,
                intensity: 3000.0,
                range: 8.0,
                shadows_enabled: false,
                ..default()
            },
            Transform::from_translation(world_pos),
        ))
        .id();

    commands.entity(parent).add_children(&[mesh_child, light_child]);
    parent
}

/// Spawns a GlitchBot entity with a glowing sphere mesh at the given tile.
pub fn spawn_glitch_bot(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    rng: &mut StdRng,
    center: Vec2,
) -> Entity {
    let vis_seed: u64 = rng.gen_range(0..u64::MAX);
    let mut vis_rng = StdRng::seed_from_u64(vis_seed);
    let initial_dir = random_direction(&mut vis_rng);
    let color = direction_color(initial_dir);
    let bot = GlitchBot::new(center);

    let emissive_color = color.to_linear();
    let mat = materials.add(StandardMaterial {
        base_color: color,
        emissive: emissive_color * 4.0,
        ..default()
    });
    let mesh = meshes.add(Sphere::new(SPHERE_RADIUS).mesh().ico(3).unwrap());

    // Seed the accumulator with the fractional subtile offset of the spawn
    // position so the first integer step fires exactly when the actor crosses
    // a subtile boundary, not up to one subtile (0.2 m) too late.
    let sc = SUBTILE_COUNT as f32;
    let accumulator = Vec2::new(
        (center.x * sc).rem_euclid(1.0),
        (center.y * sc).rem_euclid(1.0),
    );

    let parent = commands
        .spawn((
            Name::new(random_actor_name()),
            ActorInspectable,
            Charge::random(rng),
            GlitchBotVisual {
                direction: initial_dir,
                accumulator,
                dir_timer: 0.0,
                dir_interval: vis_rng.gen_range(DIR_CHANGE_MIN_S..DIR_CHANGE_MAX_S),
                collision_streak: 0,
                rng_seed: vis_seed,
                rng: vis_rng,
            },
            ActorObject::new(Box::new(bot)),
            Transform::default(),
            Visibility::Inherited,
        ))
        .id();

    let mesh_child = commands
        .spawn((
            Name::new("GlitchBot mesh"),
            ActorPickMesh,
            Pickable::default(),
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(center.x, HOVER_HEIGHT, center.y),
        ))
        .id();

    let light_child = commands
        .spawn((
            Name::new("GlitchBot light"),
            PointLight {
                color,
                intensity: 3000.0,
                range: 8.0,
                shadows_enabled: false,
                ..default()
            },
            Transform::from_xyz(center.x, HOVER_HEIGHT, center.y),
        ))
        .id();

    commands.entity(parent).add_children(&[mesh_child, light_child]);
    parent
}


//! GlitchBot — a flying, glowing sphere that wanders randomly.
//!
//! The bot picks a random direction every 3–5 s, accelerates toward it with
//! inertia, and immediately stops and repicks on collision. It occupies a
//! circle of radius 2 subtiles (diameter ≈ 5 subtiles ≈ 1 tile).

use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::actor::{process_actors, Actor, ActorMoveBuffer, ActorObject, ActorState};
use crate::map::passability::ActorFootprint;
use crate::menu::main_menu::GameState;

const GLITCH_RADIUS_SUBTILES: i32 = 2;
const DIR_CHANGE_MIN_S: f32 = 3.0;
const DIR_CHANGE_MAX_S: f32 = 5.0;
const HOVER_HEIGHT: f32 = 0.6;
const SPHERE_RADIUS: f32 = 0.5;

/// Marker + per-entity timer that lives on the same entity as `ActorObject`.
/// The processing system ticks it each frame and feeds direction changes back
/// into the `GlitchBot` actor via the `ActorObject` trait interface.
#[derive(Component)]
pub struct GlitchBotVisual {
    dir_timer: f32,
    dir_interval: f32,
    rng: StdRng,
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
    pub fn new(center: Vec2) -> Self {
        Self {
            state: ActorState {
                center,
                radius_subtiles: GLITCH_RADIUS_SUBTILES,
                rotation: 0.0,
                move_buffer: ActorMoveBuffer::default(),
                last_movement_error: None,
                footprint: ActorFootprint::new(),
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
}

fn random_cardinal(rng: &mut StdRng) -> IVec2 {
    match rng.gen_range(0u8..8) {
        0 => IVec2::new(1, 0),
        1 => IVec2::new(-1, 0),
        2 => IVec2::new(0, 1),
        3 => IVec2::new(0, -1),
        4 => IVec2::new(1, 1),
        5 => IVec2::new(1, -1),
        6 => IVec2::new(-1, 1),
        _ => IVec2::new(-1, -1),
    }
}

fn random_bright_color(rng: &mut StdRng) -> Color {
    let h: f32 = rng.gen_range(0.0..360.0);
    Color::hsl(h, 0.95, 0.6)
}

pub struct GlitchBotPlugin;

impl Plugin for GlitchBotPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GlitchBotRng>().add_systems(
            Update,
            (
                glitch_bot_think.before(process_actors),
                sync_glitch_bot_transforms,
            )
                .run_if(in_state(GameState::InGame)),
        );
    }
}

/// Per-frame: tick direction timer, handle collision resets, write move buffer.
///
/// Runs **before** the generic `process_actors` system so the move buffer is
/// ready when `try_move` fires. The actor trait's `think_low_level` and
/// `prepare_movement` are no-ops; all GlitchBot-specific logic lives here
/// because it needs the `GlitchBotVisual` component's timer and RNG.
fn glitch_bot_think(
    time: Res<Time>,
    mut query: Query<(&mut ActorObject, &mut GlitchBotVisual)>,
) {
    let dt = time.delta_secs();
    for (mut obj, mut vis) in &mut query {
        let state = obj.inner.state_mut();

        if state.last_movement_error.is_some() {
            let dir = random_cardinal(&mut vis.rng);
            state.move_buffer.subtile_shift = dir;
            state.move_buffer.rotation_shift = 0.0;
            vis.dir_timer = 0.0;
            vis.dir_interval = vis.rng.gen_range(DIR_CHANGE_MIN_S..DIR_CHANGE_MAX_S);
            continue;
        }

        vis.dir_timer += dt;
        if vis.dir_timer >= vis.dir_interval {
            let dir = random_cardinal(&mut vis.rng);
            state.move_buffer.subtile_shift = dir;
            state.move_buffer.rotation_shift = 0.0;
            vis.dir_timer = 0.0;
            vis.dir_interval = vis.rng.gen_range(DIR_CHANGE_MIN_S..DIR_CHANGE_MAX_S);
        }
    }
}

/// Keeps the 3D mesh child aligned with the actor's tile-space center.
fn sync_glitch_bot_transforms(
    actors: Query<(&ActorObject, &Children), With<GlitchBotVisual>>,
    mut transforms: Query<&mut Transform, Without<ActorObject>>,
) {
    for (obj, children) in &actors {
        let s = obj.inner.state();
        let world_x = s.center.x;
        let world_z = s.center.y;
        for child in children.iter() {
            if let Ok(mut t) = transforms.get_mut(child) {
                t.translation = Vec3::new(world_x, HOVER_HEIGHT, world_z);
            }
        }
    }
}

/// Spawns a GlitchBot entity with a glowing sphere mesh at the given tile.
pub fn spawn_glitch_bot(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    rng: &mut StdRng,
    center: Vec2,
) -> Entity {
    let color = random_bright_color(rng);
    let bot = GlitchBot::new(center);

    let emissive_color = color.to_linear();
    let mat = materials.add(StandardMaterial {
        base_color: color,
        emissive: emissive_color * 4.0,
        ..default()
    });
    let mesh = meshes.add(Sphere::new(SPHERE_RADIUS).mesh().ico(3).unwrap());

    let vis_seed: u64 = rng.gen_range(0..u64::MAX);
    let mut vis_rng = StdRng::seed_from_u64(vis_seed);

    let parent = commands
        .spawn((
            Name::new("GlitchBot"),
            GlitchBotVisual {
                dir_timer: 0.0,
                dir_interval: vis_rng.gen_range(DIR_CHANGE_MIN_S..DIR_CHANGE_MAX_S),
                rng: vis_rng,
            },
            ActorObject::new(Box::new(bot)),
            Transform::default(),
            Visibility::Inherited,
        ))
        .id();

    let child = commands
        .spawn((
            Name::new("GlitchBot mesh"),
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(center.x, HOVER_HEIGHT, center.y),
        ))
        .id();

    commands.entity(parent).add_children(&[child]);
    parent
}

//! BlackBot — a ground-walking metallic black sphere that pathfinds to
//! random nearby points.
//!
//! The bot picks a random walkable tile within 15 tiles of its current
//! position, runs A* on the tile grid, and simplifies the result with
//! [`simplify_path_line_of_sight`] so only obstacle-corner waypoints survive.
//! It then follows those waypoints in straight floating-point segments — the
//! float `center` is the canonical position, never snapped to the grid.
//!
//! Path-following "thinking" (waypoint advance, repath, heading update) runs
//! only when the actor's **main tile** — `center.floor()` — changes between
//! frames. On every other frame the bot simply integrates the cached heading.
//! Ground walker traversal rules (blocked by walls and void) are inherited
//! from the default [`Actor`] impl.

use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::actor::snapshot::{BlackBotVisualSnap, MovementStateSnap};
use crate::actor::{
    is_paused, process_actors, Actor, ActorMoveBuffer, ActorMovementError, ActorObject, ActorState,
};
use crate::map::chunk_overlay::{ChunkOverlayState, OVERLAY_RES};
use crate::map::hypermap::{world_to_chunk_local, ChunkCoord, Hypermap};
use crate::map::hypermap_pathfind::{
    astar_shortest_world_path, simplify_path_line_of_sight, HypermapPathResult,
    HypermapSearchLimits,
};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::passability::{
    DynamicPassabilityMap, SubtilePassability, TryUpdateFootprintError, SUBTILE_COUNT,
};
use crate::menu::main_menu::GameState;

/// Epsilon kept inside the passable subtile when snapping the float center to
/// a wall edge. Subtile boundaries are at multiples of `1/5`, so any value
/// strictly less than that keeps `floor(center * 5)` inside the accepted
/// subtile.
const SUBTILE_SNAP_EPSILON: f32 = 1e-4;

const BLACK_RADIUS_SUBTILES: i32 = 2;
/// Continuous travel speed in tiles per second. 1 tile = 5 subtiles.
const SPEED_TILES_PER_S: f32 = 1.2;
const SPHERE_RADIUS: f32 = 0.45;
const WANDER_RADIUS: f32 = 15.0;
const MAX_TARGET_ATTEMPTS: u32 = 8;
/// Distance (in tiles) within which the bot considers a waypoint reached even
/// without entering its containing tile — protects against numerical overshoot
/// near short segments.
const WAYPOINT_REACHED_EPSILON: f32 = 0.05;
/// Tiles kept on each side of every bend during path simplification. The
/// tile-level line-of-sight test ignores actor radius, so a lone corner
/// waypoint can leave a wide follower clipping the wall; a small buffer adds
/// axis-aligned approach + departure tiles that funnel the actor through the
/// bend.
const PATH_CORNER_BUFFER: usize = 2;
/// Chance per bot-on-bot collision that the blocked bot pauses instead of
/// continuing to push. Walls never trigger this — only `BlockedByOccupancy`.
const BOT_COLLISION_WAIT_CHANCE: f32 = 0.25;
/// How long the `Waiting` movement state lasts before the bot resumes path
/// following.
const BOT_COLLISION_WAIT_S: f32 = 1.0;

/// High-level movement mode. Distinct from low-level [`ActorState`] — this is
/// the bot's intent ("moving" vs "pausing on contact"), not the per-frame
/// collision outcome.
#[derive(Debug, Clone, Copy)]
enum MovementState {
    Moving,
    /// Pausing after bumping another bot. Movement is suppressed until the
    /// timer expires.
    Waiting { remaining_s: f32 },
}

#[derive(Component)]
pub struct BlackBotVisual {
    /// Last observed `floor(center)`. `None` forces a think on the first frame.
    pub(crate) main_tile: Option<IVec2>,
    /// Cached unit heading toward `path[path_index]`. Recomputed only on a
    /// main-tile change.
    pub(crate) direction: Vec2,
    pub(crate) has_target: bool,
    pub(crate) path: Vec<(i32, i32)>,
    pub(crate) path_index: usize,
    movement_state: MovementState,
    /// Seed used to construct [`Self::rng`]; persisted in level actor snapshots.
    pub(crate) rng_seed: u64,
    rng: StdRng,
}

impl BlackBotVisual {
    pub(crate) fn movement_state_snapshot(&self) -> MovementStateSnap {
        match self.movement_state {
            MovementState::Moving => MovementStateSnap::Moving,
            MovementState::Waiting { remaining_s } => {
                MovementStateSnap::Waiting { remaining_s }
            }
        }
    }

    fn movement_state_from_snapshot(snap: MovementStateSnap) -> MovementState {
        match snap {
            MovementStateSnap::Moving => MovementState::Moving,
            MovementStateSnap::Waiting { remaining_s } => MovementState::Waiting { remaining_s },
        }
    }
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
    pub fn from_state(state: ActorState) -> Self {
        Self { state }
    }

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

    /// Axis-decomposed collision response.
    ///
    /// The default [`Actor::try_move`] tests the combined `(shift_x, shift_y)`
    /// as one footprint update — if any part is blocked, both axes are
    /// cancelled. For a path-follower this turns every grazing-wall step into a
    /// full stop, even though sliding along the wall is the natural response.
    ///
    /// Strategy: probe X-only and Y-only via
    /// [`DynamicPassabilityMap::probe_footprint`] (no writes), build a final
    /// shift containing only the axes that passed, and commit at most one
    /// footprint write via [`DynamicPassabilityMap::try_update_footprint`].
    /// Probing avoids stamping intermediate footprints into the write buffer,
    /// which would otherwise flush into the next frame's read buffer and make
    /// the actor block itself.
    ///
    /// On a blocked axis the float `center` is snapped to just inside the far
    /// edge of the last accepted subtile in that axis (by
    /// [`SUBTILE_SNAP_EPSILON`]) so the actor rests against the obstacle; the
    /// other axis continues unchanged.
    fn try_move(
        &mut self,
        dynamic: &DynamicPassabilityMap,
        static_cache: &Hypermap<SubtilePassability>,
    ) {
        let move_buf = std::mem::replace(&mut self.state.move_buffer, ActorMoveBuffer::default());
        let actor_blocked = self.blocked_flags();
        let radius = self.state.radius_subtiles;
        self.state.rotation += move_buf.rotation_shift;
        self.state.last_movement_error = None;

        let origin = self
            .state
            .last_accepted_center_subtile
            .unwrap_or_else(|| self.state.center_subtile_i32());
        let previous = self
            .state
            .last_accepted_center_subtile
            .map(|c| (c, self.state.last_accepted_radius_subtiles));

        let probe = |shift: IVec2| -> Result<(), TryUpdateFootprintError> {
            dynamic.probe_footprint(origin + shift, radius, previous, actor_blocked, static_cache)
        };

        let want = move_buf.subtile_shift;
        let x_probe = if want.x == 0 { Ok(()) } else { probe(IVec2::new(want.x, 0)) };
        let y_probe = if want.y == 0 { Ok(()) } else { probe(IVec2::new(0, want.y)) };
        let x_ok = x_probe.is_ok();
        let y_ok = y_probe.is_ok();

        let final_shift = IVec2::new(
            if x_ok { want.x } else { 0 },
            if y_ok { want.y } else { 0 },
        );

        if let Err(e) =
            dynamic.try_update_footprint(origin + final_shift, radius, previous, actor_blocked, static_cache)
        {
            self.state.last_movement_error = Some(translate_err(e));
        } else {
            self.state.last_accepted_center_subtile = Some(origin + final_shift);
            self.state.last_accepted_radius_subtiles = radius;
        }

        let sc_f = SUBTILE_COUNT as f32;
        if x_ok {
            self.state.center.x += move_buf.tile_delta.x;
        } else {
            self.state.center.x = if want.x > 0 {
                (origin.x as f32 + 1.0) / sc_f - SUBTILE_SNAP_EPSILON
            } else {
                origin.x as f32 / sc_f + SUBTILE_SNAP_EPSILON
            };
            if let Err(e) = x_probe {
                self.state.last_movement_error = Some(translate_err(e));
            }
        }
        if y_ok {
            self.state.center.y += move_buf.tile_delta.y;
        } else {
            self.state.center.y = if want.y > 0 {
                (origin.y as f32 + 1.0) / sc_f - SUBTILE_SNAP_EPSILON
            } else {
                origin.y as f32 / sc_f + SUBTILE_SNAP_EPSILON
            };
            if let Err(e) = y_probe {
                self.state.last_movement_error = Some(translate_err(e));
            }
        }
    }
}

#[inline]
fn translate_err(e: TryUpdateFootprintError) -> ActorMovementError {
    match e {
        TryUpdateFootprintError::InvalidRadius(r) => ActorMovementError::InvalidRadius(r),
        TryUpdateFootprintError::BlockedByOccupancy { world_subtile } => {
            ActorMovementError::BlockedByOccupancy {
                world_subtile_x: world_subtile.x,
                world_subtile_y: world_subtile.y,
            }
        }
        TryUpdateFootprintError::BlockedByStatic { world_subtile } => {
            ActorMovementError::BlockedByStatic {
                world_subtile_x: world_subtile.x,
                world_subtile_y: world_subtile.y,
            }
        }
    }
}

pub struct BlackBotPlugin;

impl Plugin for BlackBotPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BlackBotRng>().add_systems(
            Update,
            (
                black_bot_think
                    .before(process_actors)
                    .run_if(not(is_paused)),
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
    passability: &Hypermap<f32>,
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
                return Some(simplify_path_line_of_sight(
                    passability,
                    &path,
                    PATH_CORNER_BUFFER,
                ));
            }
        }
    }
    None
}

#[inline]
fn waypoint_center(tile: (i32, i32)) -> Vec2 {
    Vec2::new(tile.0 as f32 + 0.5, tile.1 as f32 + 0.5)
}

#[inline]
fn float_subtile(pos: Vec2) -> IVec2 {
    let sc = SUBTILE_COUNT as f32;
    IVec2::new((pos.x * sc).floor() as i32, (pos.y * sc).floor() as i32)
}

#[inline]
fn float_tile(pos: Vec2) -> IVec2 {
    IVec2::new(pos.x.floor() as i32, pos.y.floor() as i32)
}

/// Decides direction and waypoint progression. Called only when the actor's
/// main tile changes (or on first frame).
fn think(
    vis: &mut BlackBotVisual,
    center: Vec2,
    current_tile: IVec2,
    passability: &Hypermap<f32>,
) {
    let here = (current_tile.x, current_tile.y);

    while vis.path_index < vis.path.len() && vis.path[vis.path_index] == here {
        vis.path_index += 1;
    }

    if vis.path.is_empty() || vis.path_index >= vis.path.len() {
        match pick_random_target(&mut vis.rng, here, passability) {
            Some(path) => {
                vis.path = path;
                vis.path_index = 0;
                while vis.path_index < vis.path.len() && vis.path[vis.path_index] == here {
                    vis.path_index += 1;
                }
            }
            None => {
                vis.path.clear();
                vis.path_index = 0;
                vis.has_target = false;
                return;
            }
        }
    }

    if vis.path_index >= vis.path.len() {
        vis.has_target = false;
        return;
    }

    let wp = waypoint_center(vis.path[vis.path_index]);
    let to_wp = wp - center;
    if to_wp.length_squared() > WAYPOINT_REACHED_EPSILON * WAYPOINT_REACHED_EPSILON {
        vis.direction = to_wp.normalize();
        vis.has_target = true;
    } else {
        vis.path_index += 1;
        vis.has_target = false;
    }
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

        // Tick the waiting timer first. `black_bot_think` runs before
        // `process_actors`, so the error we observe here is from the previous
        // frame's `try_move`.
        if let MovementState::Waiting { remaining_s } = &mut vis.movement_state {
            *remaining_s -= dt;
            if *remaining_s <= 0.0 {
                vis.movement_state = MovementState::Moving;
            } else {
                state.move_buffer = ActorMoveBuffer::default();
                continue;
            }
        }

        // Bot-on-bot bumps only — walls (`BlockedByStatic`) are handled by the
        // per-axis snap in `try_move` and don't trigger a pause.
        if matches!(
            state.last_movement_error,
            Some(ActorMovementError::BlockedByOccupancy { .. })
        ) && vis.rng.gen_range(0.0_f32..1.0) < BOT_COLLISION_WAIT_CHANCE
        {
            vis.movement_state = MovementState::Waiting {
                remaining_s: BOT_COLLISION_WAIT_S,
            };
            state.move_buffer = ActorMoveBuffer::default();
            continue;
        }

        let center = state.center;
        let current_tile = float_tile(center);

        let tile_changed = vis.main_tile != Some(current_tile);
        if tile_changed {
            vis.main_tile = Some(current_tile);
            think(&mut vis, center, current_tile, passability);
        }

        if !vis.has_target {
            state.move_buffer = ActorMoveBuffer::default();
            continue;
        }

        let delta = vis.direction * SPEED_TILES_PER_S * dt;
        let new_center = center + delta;
        let old_sub = float_subtile(center);
        let new_sub = float_subtile(new_center);

        state.move_buffer.tile_delta = delta;
        state.move_buffer.subtile_shift = new_sub - old_sub;
        state.move_buffer.rotation_shift = 0.0;
    }
}

fn sync_black_bot_transforms(
    actors: Query<(&ActorObject, &Children), With<BlackBotVisual>>,
    mut children_data: Query<Option<&mut Transform>, Without<ActorObject>>,
) {
    for (obj, children) in &actors {
        let world_pos = obj.inner.state().center;
        for child in children.iter() {
            if let Ok(Some(mut t)) = children_data.get_mut(child) {
                t.translation = Vec3::new(world_pos.x, SPHERE_RADIUS, world_pos.y);
            }
        }
    }
}

const TARGET_COLOR: [u8; 4] = [180, 40, 220, 160];
const TARGET_HALO_COLOR: [u8; 4] = [180, 40, 220, 60];
/// Outline drawn around each upcoming simplified-path waypoint tile so the
/// route a bot will follow is visible alongside its destination.
const PATH_NODE_COLOR: [u8; 4] = [60, 200, 255, 180];

/// Stamps a single tile at `(tx, ty)` with `color`. Only overwrites pixels
/// whose existing alpha is lower than the new alpha so a brighter mark from
/// another bot is not erased. Returns the chunk this tile lives in.
fn stamp_tile(
    overlay: &ChunkOverlayState,
    images: &mut Assets<Image>,
    tx: i32,
    ty: i32,
    color: [u8; 4],
    inset: usize,
) -> Option<ChunkCoord> {
    let res = OVERLAY_RES as usize;
    let sc = SUBTILE_COUNT;
    let (coord, local) = world_to_chunk_local(tx, ty);
    let img_h = overlay.image_for(coord)?;
    let image = images.get_mut(img_h)?;
    let data = image.data.as_mut()?;

    let base_px = local.x as usize * sc;
    let base_py = local.y as usize * sc;
    let lo = inset.min(sc);
    let hi = sc.saturating_sub(inset);
    for sy in lo..hi {
        for sx in lo..hi {
            let idx = ((base_py + sy) * res + (base_px + sx)) * 4;
            let existing_a = data[idx + 3] as u16;
            let new_a = color[3] as u16;
            if new_a > existing_a {
                data[idx..idx + 4].copy_from_slice(&color);
            }
        }
    }
    Some(coord)
}

fn paint_black_bot_targets(
    overlay: Res<ChunkOverlayState>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    bots: Query<&BlackBotVisual>,
) {
    let mut touched_chunks: std::collections::HashSet<ChunkCoord> =
        std::collections::HashSet::new();

    // Clear previous marks on all visible overlay images. Only this system
    // writes to the generic layer, so a full clear is the simplest correct
    // strategy and keeps stale targets/paths from lingering.
    for coord in overlay.iter_coords() {
        let Some(img_h) = overlay.image_for(coord) else { continue };
        let Some(image) = images.get_mut(img_h) else { continue };
        let Some(data) = image.data.as_mut() else { continue };
        data.fill(0);
        touched_chunks.insert(coord);
    }

    // Paint upcoming path waypoints first; targets paint on top so the
    // destination always wins where both overlap.
    for vis in bots.iter() {
        if vis.path_index >= vis.path.len() {
            continue;
        }
        for &(tx, ty) in &vis.path[vis.path_index..] {
            if let Some(c) = stamp_tile(&overlay, &mut images, tx, ty, PATH_NODE_COLOR, 1) {
                touched_chunks.insert(c);
            }
        }
    }

    // Paint each target as a highlighted tile with a 1-tile halo.
    for vis in bots.iter() {
        let Some((wx, wy)) = vis.target_tile() else { continue };
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                let is_center = dx == 0 && dy == 0;
                let color = if is_center { TARGET_COLOR } else { TARGET_HALO_COLOR };
                if let Some(c) = stamp_tile(&overlay, &mut images, wx + dx, wy + dy, color, 0) {
                    touched_chunks.insert(c);
                }
            }
        }
    }

    for coord in &touched_chunks {
        if let Some(mat_h) = overlay.material_for(*coord) {
            materials.get_mut(mat_h);
        }
    }
}

/// Spawns a BlackBot from a level snapshot (mesh + restored runtime state).
pub fn spawn_black_bot_from_snapshot(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    name: Option<&str>,
    state: ActorState,
    visual: BlackBotVisualSnap,
) -> Entity {
    let center = state.center;
    let bot = BlackBot::from_state(state);

    let mat = materials.add(StandardMaterial {
        base_color: Color::srgb(0.02, 0.02, 0.02),
        metallic: 1.0,
        perceptual_roughness: 0.05,
        reflectance: 1.0,
        ..default()
    });
    let mesh = meshes.add(Sphere::new(SPHERE_RADIUS).mesh().ico(3).unwrap());

    let parent = commands
        .spawn((
            Name::new(
                name.map(str::to_string)
                    .unwrap_or_else(|| "BlackBot".to_string()),
            ),
            BlackBotVisual {
                main_tile: visual.main_tile.map(Into::into),
                direction: visual.direction.into(),
                has_target: visual.has_target,
                path: visual.path,
                path_index: visual.path_index,
                movement_state: BlackBotVisual::movement_state_from_snapshot(visual.movement_state),
                rng_seed: visual.rng_seed,
                rng: StdRng::seed_from_u64(visual.rng_seed),
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

    let parent = commands
        .spawn((
            Name::new("BlackBot"),
            BlackBotVisual {
                main_tile: None,
                direction: Vec2::X,
                has_target: false,
                path: Vec::new(),
                path_index: 0,
                movement_state: MovementState::Moving,
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
            Name::new("BlackBot mesh"),
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(center.x, SPHERE_RADIUS, center.y),
        ))
        .id();

    commands.entity(parent).add_children(&[mesh_child]);
    parent
}

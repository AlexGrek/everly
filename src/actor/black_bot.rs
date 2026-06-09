//! BlackBot — a ground-walking metallic black sphere driven by an OOP
//! [`Brain`](crate::actor::brain).
//!
//! All of BlackBot's *intent* (wander, recharge when low) lives in its brain:
//! [`Behavior`](crate::actor::brain::Behavior)s
//! ([`RandomWalker`](crate::actor::brain::RandomWalker),
//! [`ChargeSelfKeeper`](crate::actor::brain::ChargeSelfKeeper)) raise
//! [`Priorities`](crate::actor::brain::Priorities); the dominant one selects a
//! high-level action ([`GoToRandomPoints`](crate::actor::brain::GoToRandomPoints)
//! / [`GoToChargeStation`](crate::actor::brain::GoToChargeStation)) which
//! dictates the per-frame low-level action
//! ([`FollowPath`](crate::actor::brain::FollowPath) /
//! [`Wait`](crate::actor::brain::Wait)). See `docs/actor-brain.md`.
//!
//! This module keeps only BlackBot-specific pieces: the visual/mesh, the
//! breakable sub-components and their wear, the battery-recharge wiring, and the
//! axis-decomposed [`Actor::try_move`] that gives the sphere wall-sliding
//! collision response. Path-following *feel* (mass/inertia, stuck-repath,
//! bot-on-bot reroute/wait) lives in [`FollowPath`].

use std::collections::HashSet;

use bevy::picking::prelude::Pickable;
use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::actor::actor_name::random_actor_name;
use crate::actor::actor_pick::{ActorInspectable, ActorPickMesh};
use crate::actor::brain::{
    generate_patrol_loop, make_high_level, AvoidanceViews, Behavior, Brain, BrainContext,
    BrainEffects, ChargeSelfKeeper, Patroller, RandomWalker,
};
use crate::actor::charge::Charge;
use crate::actor::snapshot::{BreakablePartSnap, BreakableSnap};
use crate::actor::{
    actor_main_tile, flush_actor_occupancy, is_front_collision, is_paused, process_actors, Actor,
    ActorMoveBuffer, ActorMovementError, ActorObject, ActorState, OffScreenActor,
};
use crate::map::chunk_overlay::{ChunkOverlayState, OVERLAY_RES};
use crate::map::hypermap::{world_to_chunk_local, ChunkCoord, Hypermap};
use crate::map::hypermap_world::{HypermapChunkRemeshQueue, HypermapRuntime};
use crate::map::interactive_entity::{
    sync_chargers_for_chunks, EntityCoordinates, InteractiveEntityMap,
};
use crate::map::level::LevelName;
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
const SPHERE_RADIUS: f32 = 0.45;
/// Hover height above the floor during normal operation. The sphere center
/// is at `SPHERE_RADIUS + HOVER_HEIGHT` when healthy; when the movement engine
/// breaks it falls to `SPHERE_RADIUS` (touching the floor).
const HOVER_HEIGHT: f32 = 0.3;

/// Wear accumulated per second on the movement engine while the bot is moving.
const MOVEMENT_ENGINE_WEAR_RATE: f32 = 0.0001;
/// Wear accumulated per second on the control plane at all times (while not depleted).
const CONTROL_PLANE_WEAR_RATE: f32 = 0.00004;
/// Wear accumulated per second on the sensory system at all times (while not depleted).
const SENSORY_SYSTEM_WEAR_RATE: f32 = 0.00002;

/// Healthy metallic-black base tint of a BlackBot.
const BLACK_TINT: Color = Color::srgb(0.02, 0.02, 0.02);
/// Red used for the persistent stuck state and the transient collision flash.
const ALERT_RED: Color = Color::srgb(0.95, 0.15, 0.15);
/// White shown when the control plane breaks.
const BROKEN_WHITE: Color = Color::srgb(1.0, 1.0, 1.0);
/// Seconds for the red collision flash to fade fully back to black. Short so a
/// bump reads as a quick blink rather than a lingering state.
const COLLISION_FLASH_FADE_SECS: f32 = 0.45;

/// Ring tube (minor) radius in meters — a thin band.
const RING_TUBE_RADIUS: f32 = 0.04;
/// Ring major radius, set to [`SPHERE_RADIUS`] so the band hugs the sphere's
/// equator rather than floating outside it as a detached halo.
const RING_MAJOR_RADIUS: f32 = SPHERE_RADIUS;
/// `DO_NOTHING` ring color (black).
const RING_DO_NOTHING: Color = Color::srgb(0.02, 0.02, 0.02);
/// `PATROL` ring color (blue).
const RING_PATROL: Color = Color::srgb(0.10, 0.45, 1.0);

/// State of one breakable sub-component of a [`Breakable`] bot.
#[derive(Debug, Clone)]
pub struct BreakablePartState {
    /// Monotonically increasing wear; never resets even when the part breaks.
    pub wear: f32,
    pub broken: bool,
}

impl BreakablePartState {
    fn new() -> Self {
        Self { wear: 0.0, broken: false }
    }

    /// Advance wear by `rate * dt`. When `tile_changed` is true, roll a break
    /// check using `break_chance = min(0.1, wear * 0.1)`.
    fn tick(&mut self, dt: f32, rate: f32, tile_changed: bool, rng: &mut StdRng) {
        if self.broken {
            return;
        }
        self.wear += rate * dt;
        if tile_changed {
            let chance = (self.wear * 0.01_f32).min(0.1);
            if rng.gen_range(0.0_f32..1.0) < chance {
                self.broken = true;
            }
        }
    }
}

/// Virtual (non-rendered) sub-components attached to every [`BlackBotVisual`]
/// entity. Each part accumulates wear over time and may break when the bot
/// crosses a tile boundary.
///
/// - `movement_engine`: wears only while the bot is moving.
/// - `control_plane`: wears at all times (while bot is not depleted).
/// - `sensory_system`: wears at all times (while bot is not depleted).
#[derive(Component, Debug, Clone)]
pub struct Breakable {
    pub movement_engine: BreakablePartState,
    pub control_plane: BreakablePartState,
    pub sensory_system: BreakablePartState,
}

impl Breakable {
    pub fn new() -> Self {
        Self {
            movement_engine: BreakablePartState::new(),
            control_plane: BreakablePartState::new(),
            sensory_system: BreakablePartState::new(),
        }
    }

    pub fn to_snapshot(&self) -> BreakableSnap {
        let part = |s: &BreakablePartState| BreakablePartSnap { wear: s.wear, broken: s.broken };
        BreakableSnap {
            movement_engine: part(&self.movement_engine),
            control_plane: part(&self.control_plane),
            sensory_system: part(&self.sensory_system),
        }
    }

    pub fn from_snapshot(snap: BreakableSnap) -> Self {
        let part = |s: BreakablePartSnap| BreakablePartState { wear: s.wear, broken: s.broken };
        Self {
            movement_engine: part(snap.movement_engine),
            control_plane: part(snap.control_plane),
            sensory_system: part(snap.sensory_system),
        }
    }
}

impl Default for Breakable {
    fn default() -> Self {
        Self::new()
    }
}

/// Marker + lightweight visual/debug state for a BlackBot. The `main_tile` is
/// the bot's last observed [`actor_main_tile`], used to drive wear `tile_changed`
/// rolls and shown in the inspector. All planning/movement state lives in the
/// entity's [`Brain`].
#[derive(Component)]
pub struct BlackBotVisual {
    main_tile: Option<IVec2>,
    /// Red collision-flash intensity in `0.0..=1.0`. Set to `1.0` on any frame
    /// the bot's movement step is blocked (wall graze or bot-on-bot), then
    /// decays linearly back to `0.0` so the sphere flashes red and settles to
    /// black. Transient render state — not serialized.
    collision_flash: f32,
    /// Last `base_color` written to the mesh material. The status system only
    /// touches [`Assets<StandardMaterial>`] when the displayed color actually
    /// changes, so a settled bot incurs no per-frame material writes.
    applied_color: Option<Color>,
    /// `true` once a non-operational (broken / depleted) bot has been evicted
    /// from all charger queues and released as a charger occupant. Gates the
    /// map-wide eviction to the offline *transition* so it never repeats every
    /// frame; cleared when the bot is operational again.
    offline_released: bool,
}

impl BlackBotVisual {
    pub fn main_tile(&self) -> Option<IVec2> {
        self.main_tile
    }
}

#[derive(Resource)]
pub struct BlackBotRng(pub StdRng);

impl Default for BlackBotRng {
    fn default() -> Self {
        Self(StdRng::seed_from_u64(0xB1AC_B07))
    }
}

/// What a BlackBot is *for*. Rolled randomly at spawn ([`BotSpecialization::roll`]),
/// it fixes both the bot's behavior set (its [`Brain`]) and the color of the ring
/// rendered around its sphere. Persisted in `actors.yaml` so a loaded bot keeps
/// its role (the patrol *loop* itself is regenerated on load — see [`Patrol`]).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotSpecialization {
    /// Wander to random reachable cells forever ([`RandomWalker`]). Black ring.
    #[default]
    DoNothing,
    /// Stick to a fixed loop of cells (a [`Patrol`] route, [`Patroller`] +
    /// [`GoToPatrol`](crate::actor::brain::GoToPatrol)), leaving only to recharge
    /// and resuming where it stopped. Blue ring.
    Patrol,
}

impl BotSpecialization {
    /// Rolls a specialization at spawn: [`Patrol`](Self::Patrol) with probability
    /// `1/4`, otherwise [`DoNothing`](Self::DoNothing).
    pub fn roll(rng: &mut StdRng) -> Self {
        if rng.gen_range(0..4) == 0 {
            Self::Patrol
        } else {
            Self::DoNothing
        }
    }

    /// Color of the ring rendered around this bot's sphere.
    fn ring_color(self) -> Color {
        match self {
            Self::DoNothing => RING_DO_NOTHING,
            Self::Patrol => RING_PATROL,
        }
    }

    /// Human-readable label for the inspector.
    pub fn label(self) -> &'static str {
        match self {
            Self::DoNothing => "DO_NOTHING",
            Self::Patrol => "PATROL",
        }
    }

    /// Builds the brain whose behavior set encodes this specialization. Every
    /// specialization keeps itself charged ([`ChargeSelfKeeper`]); only the
    /// routine behavior differs.
    fn build_brain(self, seed: u64) -> Brain {
        let routine: Box<dyn Behavior> = match self {
            Self::DoNothing => Box::new(RandomWalker),
            Self::Patrol => Box::new(Patroller),
        };
        Brain::new(vec![routine, Box::new(ChargeSelfKeeper::new())], make_high_level, seed)
    }
}

/// Fixed patrol route for a [`BotSpecialization::Patrol`] bot: an ordered loop of
/// world tiles the bot cycles through forever, generated lazily the first
/// operational frame (see [`black_bot_brain`]) from the bot's spawn tile and then
/// never changed. Surfaced to the brain via
/// [`BrainContext::patrol_loop`](crate::actor::brain::BrainContext::patrol_loop).
/// Only present on `PATROL` bots; not serialized (regenerated on load).
#[derive(Component, Default)]
pub struct Patrol {
    pub loop_tiles: Vec<(i32, i32)>,
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
        let initial_sub = IVec2::new((center.x * sc).floor() as i32, (center.y * sc).floor() as i32);
        Self {
            state: ActorState {
                center,
                radius_subtiles: BLACK_RADIUS_SUBTILES,
                rotation: 0.0,
                move_buffer: ActorMoveBuffer::default(),
                last_movement_error: None,
                last_accepted_center_subtile: Some(initial_sub),
                last_accepted_radius_subtiles: BLACK_RADIUS_SUBTILES,
                next_waypoint_hint: None,
                field_main_tile: None,
                dirtiness: 0.0,
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
    /// footprint write. On a blocked axis the float `center` is snapped to just
    /// inside the far edge of the last accepted subtile in that axis (by
    /// [`SUBTILE_SNAP_EPSILON`]) so the actor rests against the obstacle.
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

        let final_shift = IVec2::new(if x_ok { want.x } else { 0 }, if y_ok { want.y } else { 0 });

        // When `final_shift` keeps at most one axis it equals a shift already
        // probed above (or the origin, all self-overlap) — known passable, so
        // commit without a redundant re-probe. Only a diagonal `final_shift`
        // (both axes kept) needs a full collision check.
        let needs_probe = final_shift.x != 0 && final_shift.y != 0;
        let committed = if needs_probe {
            match dynamic.try_update_footprint(
                origin + final_shift,
                radius,
                previous,
                actor_blocked,
                static_cache,
            ) {
                Ok(()) => true,
                Err(e) => {
                    self.state.last_movement_error = Some(translate_err(e));
                    false
                }
            }
        } else {
            dynamic.commit_footprint(origin + final_shift, radius);
            true
        };
        if committed {
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
            // First blocked axis sets the error; the second does not overwrite it.
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
            if self.state.last_movement_error.is_none() {
                if let Err(e) = y_probe {
                    self.state.last_movement_error = Some(translate_err(e));
                }
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
                black_bot_brain
                    .after(flush_actor_occupancy)
                    .before(process_actors)
                    .run_if(not(is_paused)),
                sync_black_bot_transforms.after(process_actors),
                sync_black_bot_status_visual.after(process_actors),
                paint_black_bot_targets.after(process_actors),
            )
                .run_if(in_state(GameState::InGame)),
        );
    }
}

#[derive(Default)]
struct IndexedChargerChunks {
    level: String,
    chunks: HashSet<ChunkCoord>,
}

/// Runs each BlackBot's [`Brain`] once per frame (before `process_actors`),
/// then applies the requested side effects (charger docking, recharge).
///
/// Sequential by design: it mutates the [`InteractiveEntityMap`] resource and
/// the per-bot RNG lives in the brain, so it must not run on `par_iter`.
fn black_bot_brain(
    time: Res<Time>,
    level_name: Res<LevelName>,
    hypermap: Res<HypermapRuntime>,
    dynamic: Res<DynamicPassabilityMap>,
    remesh: Res<HypermapChunkRemeshQueue>,
    mut interactive: ResMut<InteractiveEntityMap>,
    mut indexed: Local<IndexedChargerChunks>,
    mut query: Query<(
        Entity,
        &mut ActorObject,
        &mut Brain,
        &mut BlackBotVisual,
        Option<&mut Charge>,
        Option<&mut Breakable>,
        Option<&mut Patrol>,
    )>,
) {
    let dt = time.delta_secs();
    let passability = &*hypermap.static_passability_map;
    let static_subtiles = &*hypermap.static_subtile_cache;
    refresh_charger_index(
        &hypermap.map,
        &mut interactive,
        &mut indexed,
        &level_name,
        &remesh,
    );

    for (entity, mut obj, mut brain, mut vis, mut charge, mut breakable, mut patrol) in &mut query {
        let blocked_flags = obj.inner.blocked_flags();
        let state = obj.inner.state_mut();

        let current_tile = actor_main_tile(state.center);
        let tile_changed = vis.main_tile.map_or(false, |prev| prev != current_tile);
        vis.main_tile = Some(current_tile);

        let depleted = charge.as_ref().is_some_and(|c| c.is_depleted());

        // CONTROL_PLANE and SENSORY_SYSTEM wear every non-depleted frame.
        if !depleted {
            if let Some(b) = breakable.as_mut() {
                b.control_plane.tick(dt, CONTROL_PLANE_WEAR_RATE, tile_changed, brain.rng_mut());
                b.sensory_system.tick(dt, SENSORY_SYSTEM_WEAR_RATE, tile_changed, brain.rng_mut());
            }
        }

        let broken = breakable
            .as_ref()
            .map_or(false, |b| b.movement_engine.broken || b.control_plane.broken);

        // Depleted or broken bots are immobilized: wipe the plan (routes/targets),
        // cancel movement, and release every charger queue slot / occupancy they
        // hold (once, on the offline transition) so they stop blocking working bots.
        if depleted || broken {
            brain.reset();
            state.move_buffer = ActorMoveBuffer::default();
            state.next_waypoint_hint = None;
            if !vis.offline_released {
                interactive.evict_actor_everywhere(entity);
                vis.offline_released = true;
            }
            continue;
        }
        vis.offline_released = false;

        // Generate the patrol loop once, lazily, from the bot's current (spawn)
        // tile. It then never changes — the bot sticks to it forever.
        if let Some(p) = patrol.as_mut() {
            if p.loop_tiles.is_empty() {
                p.loop_tiles =
                    generate_patrol_loop(brain.rng_mut(), (current_tile.x, current_tile.y), passability);
            }
        }
        let patrol_loop = patrol.as_deref().map(|p| p.loop_tiles.as_slice());

        let charge_level = charge.as_ref().map(|c| c.level).unwrap_or(1.0);
        let effects = {
            let ctx = BrainContext {
                entity,
                dt,
                center: state.center,
                main_tile: current_tile,
                main_tile_changed: tile_changed,
                floor: 0,
                charge: charge_level,
                missing_charge_pct: (1.0 - charge_level) * 100.0,
                depleted,
                broken,
                passability,
                interactive: &interactive,
                avoidance: Some(AvoidanceViews {
                    dynamic: &dynamic,
                    static_subtiles,
                    blocked_flags,
                }),
                patrol_loop,
            };
            brain.tick(&ctx, state)
        };

        // MOVEMENT_ENGINE wears only while actually moving this frame.
        if state.move_buffer.tile_delta != Vec2::ZERO {
            if let Some(b) = breakable.as_mut() {
                b.movement_engine.tick(dt, MOVEMENT_ENGINE_WEAR_RATE, tile_changed, brain.rng_mut());
            }
        }

        apply_brain_effects(entity, &effects, &mut interactive, charge.as_deref_mut());
    }
}

fn refresh_charger_index(
    map: &Hypermap<crate::map::world_map::CellType>,
    interactive: &mut InteractiveEntityMap,
    indexed: &mut IndexedChargerChunks,
    level_name: &LevelName,
    remesh: &HypermapChunkRemeshQueue,
) {
    if indexed.level != level_name.0 {
        indexed.level = level_name.0.clone();
        indexed.chunks.clear();
        interactive.clear();
    }

    let loaded_chunks = map.loaded_chunks();
    let loaded_set: HashSet<ChunkCoord> = loaded_chunks.iter().copied().collect();

    let mut sync_chunks: HashSet<ChunkCoord> = loaded_set
        .difference(&indexed.chunks)
        .copied()
        .collect();
    sync_chunks.extend(remesh.0.iter().copied());
    if interactive.is_empty() {
        sync_chunks.extend(loaded_chunks.iter().copied());
    }

    if !sync_chunks.is_empty() {
        sync_chargers_for_chunks(map, interactive, sync_chunks.iter().copied());
        indexed.chunks.extend(sync_chunks);
    }
    indexed.chunks.retain(|chunk| loaded_set.contains(chunk));
}

/// Applies a brain tick's [`BrainEffects`] to the world: charger docking and
/// battery recharge.
fn apply_brain_effects(
    entity: Entity,
    effects: &BrainEffects,
    interactive: &mut InteractiveEntityMap,
    charge: Option<&mut Charge>,
) {
    if let Some(coords) = effects.queue_unwant {
        interactive.remove_wanting(coords, entity);
    }
    if let Some(coords) = effects.queue_want {
        interactive.add_wanting(coords, entity);
    }
    if let Some(coords) = effects.queue_wait {
        interactive.add_waiting(coords, entity);
    }
    if let Some(coords) = effects.queue_unwait {
        interactive.remove_waiting(coords, entity);
    }
    if let Some(coords) = effects.dock {
        interactive.remove_waiting(coords, entity);
        set_charger_occupant(interactive, coords, Some(entity));
    }
    if let Some(coords) = effects.undock {
        // Only release if we are still the occupant (don't evict a new tenant).
        if charger_occupant(interactive, coords) == Some(entity) {
            set_charger_occupant(interactive, coords, None);
        }
        interactive.remove_actor_from_queues(coords, entity);
    }
    if effects.recharge > 0.0 {
        if let Some(c) = charge {
            c.level = (c.level + effects.recharge).min(1.0);
        }
    }
}

fn set_charger_occupant(
    map: &mut InteractiveEntityMap,
    coords: EntityCoordinates,
    occupant: Option<Entity>,
) {
    if let Some(list) = map.list_at_mut(coords) {
        for entry in list.iter_mut() {
            if let Some(charger) = entry.entity.as_charger_mut() {
                charger.set_occupant(occupant);
            }
        }
    }
}

fn charger_occupant(map: &InteractiveEntityMap, coords: EntityCoordinates) -> Option<Entity> {
    map.entities_at(coords)
        .iter()
        .filter_map(|e| e.entity.as_charger())
        .find_map(|c| c.occupant())
}

fn sync_black_bot_transforms(
    actors: Query<(&ActorObject, &Children, Option<&Breakable>), (With<BlackBotVisual>, Without<OffScreenActor>)>,
    mut children_data: Query<Option<&mut Transform>, Without<ActorObject>>,
) {
    for (obj, children, breakable) in &actors {
        let world_pos = obj.inner.state().center;
        let y = if breakable.map_or(false, |b| b.movement_engine.broken) {
            SPHERE_RADIUS // fallen: resting on the floor
        } else {
            SPHERE_RADIUS + HOVER_HEIGHT // hovering during normal operation
        };
        for child in children.iter() {
            if let Ok(Some(mut t)) = children_data.get_mut(child) {
                t.translation = Vec3::new(world_pos.x, y, world_pos.y);
            }
        }
    }
}

/// Linearly interpolates two colors in sRGB space (`t` clamped to `0.0..=1.0`).
fn lerp_srgb(a: Color, b: Color, t: f32) -> Color {
    let a = a.to_srgba();
    let b = b.to_srgba();
    let t = t.clamp(0.0, 1.0);
    Color::srgb(
        a.red + (b.red - a.red) * t,
        a.green + (b.green - a.green) * t,
        a.blue + (b.blue - a.blue) * t,
    )
}

/// Applies material color changes for BlackBot runtime status.
///
/// Priority order:
/// 1. Broken control plane => white
/// 2. Stuck while trying to route => red
/// 3. Collision flash => black→red by [`BlackBotVisual::collision_flash`],
///    which is relit to `1.0` on a blocked step and fades back over
///    [`COLLISION_FLASH_FADE_SECS`].
/// 4. Healthy / settled => default black
///
/// Runs `.after(process_actors)` so `last_movement_error` reflects this frame's
/// movement outcome. The material is only rewritten when the displayed color
/// actually changes, so a settled (non-flashing) bot costs no asset writes.
fn sync_black_bot_status_visual(
    time: Res<Time>,
    mut bots: Query<(&ActorObject, &Breakable, &Brain, &mut BlackBotVisual, &Children)>,
    pick_meshes: Query<&MeshMaterial3d<StandardMaterial>, With<ActorPickMesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let dt = time.delta_secs();
    for (obj, b, brain, mut vis, children) in &mut bots {
        // A blocked movement step this frame relights the flash; otherwise it
        // keeps fading. Wall grazes always count, but a bot-on-bot bump only
        // counts when it is **head-on** — a rear bump is ignored (mirrors the
        // movement response in `FollowPath`).
        let state = obj.inner.state();
        let collided = match state.last_movement_error {
            Some(ActorMovementError::BlockedByStatic { .. }) => true,
            Some(ActorMovementError::BlockedByOccupancy { world_subtile_x, world_subtile_y }) => {
                is_front_collision(state.center, brain.velocity(), world_subtile_x, world_subtile_y)
            }
            _ => false,
        };
        if collided {
            vis.collision_flash = 1.0;
        }

        let cp_broken = b.control_plane.broken;
        let stuck = brain.is_stuck();
        let target_color = if cp_broken {
            BROKEN_WHITE
        } else if stuck {
            ALERT_RED
        } else {
            lerp_srgb(BLACK_TINT, ALERT_RED, vis.collision_flash)
        };

        if vis.applied_color != Some(target_color) {
            for child in children.iter() {
                let Ok(mat_handle) = pick_meshes.get(child) else { continue };
                let Some(mat) = materials.get_mut(&mat_handle.0) else { continue };
                mat.base_color = target_color;
            }
            vis.applied_color = Some(target_color);
        }

        if vis.collision_flash > 0.0 {
            vis.collision_flash = (vis.collision_flash - dt / COLLISION_FLASH_FADE_SECS).max(0.0);
        }
    }
}

const TARGET_COLOR: [u8; 4] = [180, 40, 220, 160];
const TARGET_HALO_COLOR: [u8; 4] = [180, 40, 220, 60];
/// Outline drawn around each upcoming simplified-path waypoint tile so the
/// route a bot will follow is visible alongside its destination.
const PATH_NODE_COLOR: [u8; 4] = [60, 200, 255, 180];

/// `true` when a BlackBot is depleted or broken enough to skip brain ticks and
/// route overlays (matches the gate in `black_bot_brain`).
fn black_bot_offline(charge: Option<&Charge>, breakable: Option<&Breakable>) -> bool {
    if charge.is_some_and(Charge::is_depleted) {
        return true;
    }
    breakable.map_or(false, |b| b.movement_engine.broken || b.control_plane.broken)
}

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
    bots: Query<(&Brain, Option<&Charge>, Option<&Breakable>), With<BlackBotVisual>>,
) {
    let mut touched_chunks: HashSet<ChunkCoord> = HashSet::new();

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
    for (brain, charge, breakable) in &bots {
        if black_bot_offline(charge, breakable) {
            continue;
        }
        if let Some((path, idx)) = brain.path() {
            for &(tx, ty) in path.get(idx..).unwrap_or(&[]) {
                if let Some(c) = stamp_tile(&overlay, &mut images, tx, ty, PATH_NODE_COLOR, 1) {
                    touched_chunks.insert(c);
                }
            }
        }
    }

    // Paint each target as a highlighted tile with a 1-tile halo.
    for (brain, charge, breakable) in &bots {
        if black_bot_offline(charge, breakable) {
            continue;
        }
        let Some((wx, wy)) = brain.target_tile() else { continue };
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

fn black_bot_material(materials: &mut Assets<StandardMaterial>) -> Handle<StandardMaterial> {
    materials.add(StandardMaterial {
        base_color: BLACK_TINT,
        metallic: 1.0,
        perceptual_roughness: 0.05,
        reflectance: 1.0,
        ..default()
    })
}

/// Spawns the specialization-colored ring child rendered around a bot's sphere.
///
/// The ring is a flat (equatorial) [`Torus`] centered on the sphere; it is added
/// as a child of the actor root so `sync_black_bot_transforms` keeps its position
/// glued to the bot each frame. It carries no [`ActorPickMesh`] / [`Pickable`], so
/// it is ignored by picking and by the status recolor (which only touches the
/// pick mesh) — the ring keeps its specialization color for the bot's life.
fn spawn_bot_ring(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    center: Vec2,
    color: Color,
) -> Entity {
    let mesh = meshes.add(Torus {
        minor_radius: RING_TUBE_RADIUS,
        major_radius: RING_MAJOR_RADIUS,
    });
    // Matte, non-reflective: a black ring reads as flat black (no shiny
    // highlights), while a colored ring still glows via emissive.
    let material = materials.add(StandardMaterial {
        base_color: color,
        emissive: LinearRgba::from(color) * 2.0,
        metallic: 0.0,
        perceptual_roughness: 1.0,
        reflectance: 0.0,
        ..default()
    });
    commands
        .spawn((
            Name::new("BlackBot ring"),
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::from_xyz(center.x, SPHERE_RADIUS + HOVER_HEIGHT, center.y),
        ))
        .id()
}

/// Inserts the specialization marker (and, for `PATROL`, the lazily filled
/// [`Patrol`] route) onto a freshly spawned BlackBot root.
fn attach_specialization(commands: &mut Commands, parent: Entity, spec: BotSpecialization) {
    commands.entity(parent).insert(spec);
    if matches!(spec, BotSpecialization::Patrol) {
        commands.entity(parent).insert(Patrol::default());
    }
}

/// Spawns a BlackBot from a level snapshot (mesh + restored runtime state). The
/// brain is rebuilt fresh from `rng_seed` and re-plans from scratch.
pub fn spawn_black_bot_from_snapshot(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    name: &str,
    state: ActorState,
    rng_seed: u64,
    breakable: BreakableSnap,
    spec: BotSpecialization,
) -> Entity {
    let center = state.center;
    let bot = BlackBot::from_state(state);

    let mat = black_bot_material(materials);
    let mesh = meshes.add(Sphere::new(SPHERE_RADIUS).mesh().ico(3).unwrap());

    let parent = commands
        .spawn((
            Name::new(name.to_string()),
            ActorInspectable,
            Breakable::from_snapshot(breakable),
            BlackBotVisual {
                main_tile: None,
                collision_flash: 0.0,
                applied_color: None,
                offline_released: false,
            },
            spec.build_brain(rng_seed),
            ActorObject::new(Box::new(bot)),
            Transform::default(),
            Visibility::Inherited,
        ))
        .id();
    attach_specialization(commands, parent, spec);

    let mesh_child = commands
        .spawn((
            Name::new("BlackBot mesh"),
            ActorPickMesh,
            Pickable::default(),
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(center.x, SPHERE_RADIUS + HOVER_HEIGHT, center.y),
        ))
        .id();
    let ring_child = spawn_bot_ring(commands, meshes, materials, center, spec.ring_color());

    commands.entity(parent).add_children(&[mesh_child, ring_child]);
    parent
}

pub fn spawn_black_bot(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    rng: &mut StdRng,
    center: Vec2,
) -> Entity {
    let spec = BotSpecialization::roll(rng);
    let brain_seed: u64 = rng.gen_range(0..u64::MAX);
    let bot = BlackBot::new(center);

    let mat = black_bot_material(materials);
    let mesh = meshes.add(Sphere::new(SPHERE_RADIUS).mesh().ico(3).unwrap());

    let parent = commands
        .spawn((
            Name::new(random_actor_name()),
            ActorInspectable,
            Charge::random(rng),
            Breakable::new(),
            BlackBotVisual {
                main_tile: None,
                collision_flash: 0.0,
                applied_color: None,
                offline_released: false,
            },
            spec.build_brain(brain_seed),
            ActorObject::new(Box::new(bot)),
            Transform::default(),
            Visibility::Inherited,
        ))
        .id();
    attach_specialization(commands, parent, spec);

    let mesh_child = commands
        .spawn((
            Name::new("BlackBot mesh"),
            ActorPickMesh,
            Pickable::default(),
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(center.x, SPHERE_RADIUS + HOVER_HEIGHT, center.y),
        ))
        .id();
    let ring_child = spawn_bot_ring(commands, meshes, materials, center, spec.ring_color());

    commands.entity(parent).add_children(&[mesh_child, ring_child]);
    parent
}

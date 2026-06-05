//! Low-level actions — the concrete, per-frame thing a bot is doing right now.
//!
//! A high-level action ([`super::HighLevelAction`]) dictates the current
//! low-level action; the brain calls [`LowLevelAction::execute`] every frame to
//! turn it into movement intent ([`ActorState::move_buffer`]).
//!
//! Three kinds today:
//! - [`Idle`] — do nothing (the default between plans).
//! - [`Wait`] — hold position for a duration (e.g. dwelling on a charger).
//! - [`FollowPath`] — steer along a simplified waypoint path. This is where all
//!   of BlackBot's tuned movement *feel* lives: mass/inertia (finite
//!   accel/decel velocity steering), wall-momentum bleed, a stuck-repath safety
//!   net, and bot-on-bot reroute/wait — see [`FollowTuning`].

use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::Rng;

use crate::actor::{
    occupancy_collision_normal, reflect_velocity, ActorMoveBuffer, ActorMovementError, ActorState,
};
use crate::map::hypermap_pathfind::{astar_subtile_detour, world_tile_walkable};
use crate::map::passability::SUBTILE_COUNT;

use super::BrainContext;

/// Bounding-box margin (subtiles) added around the start/goal of a bot-on-bot
/// subtile detour search. Lets the route bulge out by ~1 tile to get around a
/// neighbour.
const DETOUR_PAD_SUBTILES: i32 = 6;
/// Maximum Manhattan span (subtiles) between the actor and the next path node
/// for which a subtile detour is attempted — keeps it a *short* local maneuver
/// (`40 subtiles = 8 tiles`).
const DETOUR_MAX_SPAN_SUBTILES: i32 = 40;
/// Hard cap on subtile A\* node expansions for a detour (safety bound).
const DETOUR_MAX_EXPANDED: usize = 1024;

/// Tuning for [`FollowPath`]. Defaults reproduce BlackBot's historical movement
/// feel so the planning refactor changes nothing about how a bot moves.
#[derive(Debug, Clone, Copy)]
pub struct FollowTuning {
    /// Maximum continuous travel speed in tiles per second (`1 tile = 5 subtiles`).
    pub max_speed: f32,
    /// Acceleration toward the target heading (tiles/s²) — gives apparent mass.
    pub accel: f32,
    /// Braking deceleration when slowing or stopping (tiles/s²); stronger than `accel`.
    pub decel: f32,
    /// Distance (tiles) within which a waypoint counts as reached.
    pub waypoint_eps: f32,
    /// Seconds without progress toward the active waypoint before abandoning the path.
    pub stuck_repath_secs: f32,
    /// Minimum closest-approach reduction (tiles) that resets the stuck timer.
    pub stuck_progress_eps: f32,
    /// Chance per bot-on-bot bump to detour around the other bot.
    pub bot_reroute_chance: f32,
    /// Chance per bot-on-bot bump to pause instead of pushing.
    pub bot_wait_chance: f32,
    /// How long a bot-on-bot pause lasts.
    pub bot_wait_secs: f32,
    /// Chance per bot-on-bot bump to compute a **subtile-level** detour that
    /// threads the actor's footprint around the blocker to the next path node.
    /// Defaults to the same odds as [`Self::bot_wait_chance`].
    pub bot_subtile_detour_chance: f32,
}

impl Default for FollowTuning {
    fn default() -> Self {
        Self {
            max_speed: 1.2,
            accel: 2.5,
            decel: 6.0,
            waypoint_eps: 0.05,
            stuck_repath_secs: 2.0,
            stuck_progress_eps: 0.05,
            bot_reroute_chance: 0.20,
            bot_wait_chance: 0.25,
            bot_wait_secs: 1.0,
            bot_subtile_detour_chance: 0.25,
        }
    }
}

/// The per-frame contract every low-level action implements.
pub trait LowLevelAction: Send + Sync {
    /// Advance internal state and write this frame's movement intent into
    /// `state.move_buffer` / `state.next_waypoint_hint`.
    fn execute(
        &mut self,
        state: &mut ActorState,
        ctx: &BrainContext,
        rng: &mut StdRng,
        tuning: &FollowTuning,
    );

    /// `true` once this action has nothing left to do — the high-level action
    /// observes this (next frame) to advance its plan (e.g. arrived / waited).
    fn is_finished(&self) -> bool;

    /// Bleed any momentum so a resumed action starts clean. Default no-op.
    fn halt(&mut self) {}

    /// Short label for the inspector.
    fn label(&self) -> String;

    /// Active path + cursor, if this action follows one (overlay / inspector).
    fn path(&self) -> Option<(&[(i32, i32)], usize)> {
        None
    }

    /// Current velocity (inspector). Default zero.
    fn velocity(&self) -> Vec2 {
        Vec2::ZERO
    }

    /// Seconds stuck against an obstacle (inspector). Default zero.
    fn stuck_timer(&self) -> f32 {
        0.0
    }

    /// `true` when this action is currently considered stuck and needs replanning.
    fn is_stuck(&self) -> bool {
        false
    }

    /// Final destination tile, if any (overlay / inspector).
    fn target_tile(&self) -> Option<(i32, i32)> {
        None
    }
}

// ---------------------------------------------------------------------------
// Idle
// ---------------------------------------------------------------------------

/// No-op action: clears movement intent and is always finished.
pub struct Idle;

impl LowLevelAction for Idle {
    fn execute(&mut self, state: &mut ActorState, _ctx: &BrainContext, _rng: &mut StdRng, _t: &FollowTuning) {
        state.move_buffer = ActorMoveBuffer::default();
        state.next_waypoint_hint = None;
    }
    fn is_finished(&self) -> bool {
        true
    }
    fn label(&self) -> String {
        "Idle".to_string()
    }
}

// ---------------------------------------------------------------------------
// Wait
// ---------------------------------------------------------------------------

/// Hold position for `remaining_s` seconds. Used for the charging dwell and as a
/// short retry delay when planning fails.
pub struct Wait {
    pub remaining_s: f32,
}

impl Wait {
    pub fn new(seconds: f32) -> Self {
        Self { remaining_s: seconds }
    }
}

impl LowLevelAction for Wait {
    fn execute(&mut self, state: &mut ActorState, ctx: &BrainContext, _rng: &mut StdRng, _t: &FollowTuning) {
        self.remaining_s -= ctx.dt;
        state.move_buffer = ActorMoveBuffer::default();
        state.next_waypoint_hint = None;
    }
    fn is_finished(&self) -> bool {
        self.remaining_s <= 0.0
    }
    fn label(&self) -> String {
        if self.remaining_s.is_finite() {
            format!("Wait ({:.1}s)", self.remaining_s.max(0.0))
        } else {
            "Wait".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// FollowPath
// ---------------------------------------------------------------------------

/// Steers the actor along a simplified waypoint path with mass/inertia. Reaches
/// `is_finished() == true` when the path is exhausted or abandoned (stuck), at
/// which point the owning high-level action re-plans.
pub struct FollowPath {
    pub path: Vec<(i32, i32)>,
    pub index: usize,
    /// Unit heading toward `path[index]`; recomputed every moving frame.
    direction: Vec2,
    /// Carries momentum between frames; steered under finite acceleration.
    velocity: Vec2,
    /// Last frame's center, used to bleed momentum lost to wall collisions.
    prev_center: Option<Vec2>,
    stuck_timer: f32,
    closest_approach: f32,
    stuck_waypoint_index: usize,
    /// Remaining bot-on-bot pause; movement is suppressed while `> 0`.
    contact_wait_s: f32,
    /// Active subtile-level detour: tile-space waypoint centers to thread around
    /// a blocking bot before resuming the tile path. Empty = no detour.
    detour: Vec<Vec2>,
    detour_index: usize,
    /// Time spent on the current detour; bounds it to `stuck_repath_secs`.
    detour_timer: f32,
    /// Set when the stuck timer fires; makes `is_finished` report `true`.
    abandoned: bool,
}

impl FollowPath {
    pub fn new(path: Vec<(i32, i32)>) -> Self {
        Self {
            path,
            index: 0,
            direction: Vec2::X,
            velocity: Vec2::ZERO,
            prev_center: None,
            stuck_timer: 0.0,
            closest_approach: f32::MAX,
            stuck_waypoint_index: 0,
            contact_wait_s: 0.0,
            detour: Vec::new(),
            detour_index: 0,
            detour_timer: 0.0,
            abandoned: false,
        }
    }

    /// Plans a short subtile-level route around a blocking bot to the next
    /// already-calculated path node. Returns tile-space waypoint centers
    /// (excluding the actor's current position), or `None` when avoidance data
    /// is unavailable, the node is too far for a *short* detour, or no
    /// footprint-clear route exists.
    fn plan_subtile_detour(&self, state: &ActorState, ctx: &BrainContext) -> Option<Vec<Vec2>> {
        let views = ctx.avoidance.as_ref()?;
        let goal_tile = *self.path.get(self.index)?;
        let radius = state.radius_subtiles;
        let sc = SUBTILE_COUNT as i32;
        let start = state
            .last_accepted_center_subtile
            .unwrap_or_else(|| float_subtile(state.center));
        // Center subtile of the goal tile (`sc / 2` lands on the middle column).
        let goal = IVec2::new(goal_tile.0 * sc + sc / 2, goal_tile.1 * sc + sc / 2);

        // Self-overlap is bypassed against the actor's *current* footprint so the
        // bot never treats its own body as an obstacle near the start.
        let previous = Some((start, radius));
        let dynamic = views.dynamic;
        let static_subtiles = views.static_subtiles;
        let blocked = views.blocked_flags;

        let subtile_path = astar_subtile_detour(
            start,
            goal,
            DETOUR_PAD_SUBTILES,
            DETOUR_MAX_SPAN_SUBTILES,
            DETOUR_MAX_EXPANDED,
            |sub| {
                dynamic
                    .probe_footprint(sub, radius, previous, blocked, static_subtiles)
                    .is_ok()
            },
        )?;
        if subtile_path.len() < 2 {
            return None;
        }
        let collapsed = collapse_collinear_subtiles(&subtile_path);
        Some(collapsed.into_iter().skip(1).map(subtile_center).collect())
    }

    fn current_waypoint_hint(&self) -> Option<Vec2> {
        if self.index < self.path.len() {
            Some(waypoint_center(self.path[self.index]))
        } else {
            None
        }
    }

    fn advance_past_reached(&mut self, center: Vec2, eps: f32) {
        while self.index < self.path.len() && reached_waypoint(center, self.path[self.index], eps) {
            self.index += 1;
        }
    }

    /// Reconcile momentum with reality, steer toward `desired` at `rate`, then
    /// emit the resulting displacement into `state.move_buffer`.
    fn drive(&mut self, state: &mut ActorState, center: Vec2, desired: Vec2, rate: f32, dt: f32) {
        if dt > 1e-6 {
            if let Some(prev) = self.prev_center {
                let achieved = (center - prev) / dt;
                if achieved.x.abs() < self.velocity.x.abs() * 0.8 {
                    self.velocity.x = achieved.x;
                }
                if achieved.y.abs() < self.velocity.y.abs() * 0.8 {
                    self.velocity.y = achieved.y;
                }
            }
        }
        self.velocity = approach_velocity(self.velocity, desired, rate, dt);
        let delta = self.velocity * dt;
        state.move_buffer.tile_delta = delta;
        state.move_buffer.subtile_shift = float_subtile(center + delta) - float_subtile(center);
        state.move_buffer.rotation_shift = 0.0;
        self.prev_center = Some(center);
    }

    fn pause_in_place(&mut self, state: &mut ActorState) {
        self.velocity = Vec2::ZERO;
        self.prev_center = None;
        state.move_buffer = ActorMoveBuffer::default();
        state.next_waypoint_hint = self.current_waypoint_hint();
    }
}

impl LowLevelAction for FollowPath {
    fn execute(&mut self, state: &mut ActorState, ctx: &BrainContext, rng: &mut StdRng, t: &FollowTuning) {
        let dt = ctx.dt;
        let center = state.center;

        // Bot-on-bot pause countdown.
        if self.contact_wait_s > 0.0 {
            self.contact_wait_s -= dt;
            self.pause_in_place(state);
            return;
        }

        // Bot-on-bot collisions bounce elastically around the contact normal.
        if let Some(ActorMovementError::BlockedByOccupancy {
            world_subtile_x,
            world_subtile_y,
        }) = state.last_movement_error.clone()
        {
            let normal = occupancy_collision_normal(center, world_subtile_x, world_subtile_y);
            self.velocity = reflect_velocity(self.velocity, normal);
            if self.velocity.length_squared() > 1e-8 {
                self.direction = self.velocity.normalize();
            } else {
                self.direction = -self.direction;
            }
            // Skip achieved-vs-planned clamping this frame so reflection is preserved.
            self.prev_center = None;

            // A fresh bump invalidates any in-progress subtile detour (the world
            // moved out from under it); drop it before rolling a new response.
            self.detour.clear();
            self.detour_index = 0;
            self.detour_timer = 0.0;

            // Roll one bot-on-bot response: a single-tile step-away, a short
            // pause, a size-aware subtile detour around the blocker, or (the
            // remaining probability) just the elastic bounce above.
            let roll = rng.gen_range(0.0_f32..1.0);
            if roll < t.bot_reroute_chance {
                let distance = rng.gen_range(1..=2) as f32;
                let d = self.direction;
                // Back, strafe-left, strafe-right — shuffled so each can win first.
                let mut candidates = [-d, Vec2::new(-d.y, d.x), Vec2::new(d.y, -d.x)];
                for i in (1..3).rev() {
                    let j = rng.gen_range(0..=i);
                    candidates.swap(i, j);
                }
                let cur = ctx.main_tile;
                for escape_dir in candidates {
                    let escape = (
                        (cur.x as f32 + escape_dir.x * distance).round() as i32,
                        (cur.y as f32 + escape_dir.y * distance).round() as i32,
                    );
                    if escape != (cur.x, cur.y) && world_tile_walkable(ctx.passability, escape.0, escape.1) {
                        let insert_idx = self.index.min(self.path.len());
                        self.path.insert(insert_idx, escape);
                        // Force heading recalculation toward the escape tile this frame.
                        self.stuck_waypoint_index = usize::MAX;
                        break;
                    }
                }
            } else if roll < t.bot_reroute_chance + t.bot_wait_chance {
                self.contact_wait_s = t.bot_wait_secs;
                self.pause_in_place(state);
                return;
            } else if roll < t.bot_reroute_chance + t.bot_wait_chance + t.bot_subtile_detour_chance {
                if let Some(detour) = self.plan_subtile_detour(state, ctx) {
                    self.detour = detour;
                    self.detour_index = 0;
                    self.detour_timer = 0.0;
                }
            }
        }

        // Follow an active subtile detour around a bot before resuming the tile
        // path. Bounded by `stuck_repath_secs` so a detour that stops making
        // progress is dropped and normal stuck handling can take over.
        if !self.detour.is_empty() {
            self.detour_timer += dt;
            while self.detour_index < self.detour.len()
                && (self.detour[self.detour_index] - center).length_squared()
                    <= t.waypoint_eps * t.waypoint_eps
            {
                self.detour_index += 1;
            }
            if self.detour_index < self.detour.len() && self.detour_timer <= t.stuck_repath_secs {
                let wp = self.detour[self.detour_index];
                let to_wp = wp - center;
                if to_wp.length_squared() > 1e-12 {
                    self.direction = to_wp.normalize();
                }
                let dist = to_wp.length();
                let brake_limited_speed = (2.0 * t.decel * dist).sqrt();
                let desired_speed = t.max_speed.min(brake_limited_speed);
                let desired = self.direction * desired_speed;
                let steer_rate = if self.velocity.length() > desired_speed {
                    t.decel
                } else {
                    t.accel
                };
                self.drive(state, center, desired, steer_rate, dt);
                state.next_waypoint_hint = Some(wp);
                return;
            }
            self.detour.clear();
            self.detour_index = 0;
            self.detour_timer = 0.0;
        }

        self.advance_past_reached(center, t.waypoint_eps);

        if self.index >= self.path.len() {
            // Exhausted: coast to a stop (the high-level re-plans next frame).
            self.drive(state, center, Vec2::ZERO, t.decel, dt);
            state.next_waypoint_hint = None;
            return;
        }

        let wp = waypoint_center(self.path[self.index]);
        let to_wp = wp - center;
        if to_wp.length_squared() > 1e-12 {
            self.direction = to_wp.normalize();
        }

        // Stuck detection: abandon the path if no progress toward the waypoint.
        if self.stuck_waypoint_index != self.index {
            self.stuck_timer = 0.0;
            self.closest_approach = f32::MAX;
            self.stuck_waypoint_index = self.index;
        }
        let dist = to_wp.length();
        if dist < self.closest_approach - t.stuck_progress_eps {
            self.closest_approach = dist;
            self.stuck_timer = 0.0;
        } else {
            self.stuck_timer += dt;
        }
        if self.stuck_timer >= t.stuck_repath_secs {
            self.abandoned = true;
            self.velocity = Vec2::ZERO;
            self.prev_center = None;
            state.move_buffer = ActorMoveBuffer::default();
            state.next_waypoint_hint = None;
            return;
        }

        // Braking profile: as we approach the waypoint, cap target speed to the
        // maximum speed that can stop within remaining distance (v^2 = 2 a d).
        // This prevents late, floaty overshoot and makes slowdown feel snappier.
        let brake_limited_speed = (2.0 * t.decel * dist).sqrt();
        let desired_speed = t.max_speed.min(brake_limited_speed);
        let desired = self.direction * desired_speed;
        let steer_rate = if self.velocity.length() > desired_speed {
            t.decel
        } else {
            t.accel
        };
        self.drive(state, center, desired, steer_rate, dt);
        state.next_waypoint_hint = Some(wp);
    }

    fn is_finished(&self) -> bool {
        self.abandoned || self.index >= self.path.len()
    }

    fn halt(&mut self) {
        self.velocity = Vec2::ZERO;
        self.prev_center = None;
    }

    fn label(&self) -> String {
        if self.detour.is_empty() {
            "FollowPath".to_string()
        } else {
            "FollowPath (detour)".to_string()
        }
    }

    fn path(&self) -> Option<(&[(i32, i32)], usize)> {
        Some((&self.path, self.index))
    }

    fn velocity(&self) -> Vec2 {
        self.velocity
    }

    fn stuck_timer(&self) -> f32 {
        self.stuck_timer
    }

    fn is_stuck(&self) -> bool {
        // "Stuck" means we abandoned an unfinished route because progress stalled.
        self.abandoned && self.index < self.path.len()
    }

    fn target_tile(&self) -> Option<(i32, i32)> {
        self.path.last().copied()
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Tile-space center of a waypoint tile.
#[inline]
pub fn waypoint_center(tile: (i32, i32)) -> Vec2 {
    Vec2::new(tile.0 as f32 + 0.5, tile.1 as f32 + 0.5)
}

/// `true` when `center` is within `eps` tiles of `tile`'s center.
#[inline]
pub fn reached_waypoint(center: Vec2, tile: (i32, i32), eps: f32) -> bool {
    (waypoint_center(tile) - center).length_squared() <= eps * eps
}

/// Subtile coordinate that contains `pos` (floor of `pos * SUBTILE_COUNT`).
#[inline]
pub fn float_subtile(pos: Vec2) -> IVec2 {
    let sc = SUBTILE_COUNT as f32;
    IVec2::new((pos.x * sc).floor() as i32, (pos.y * sc).floor() as i32)
}

/// Tile-space center of world subtile `sub` (inverse of [`float_subtile`] at the
/// subtile midpoint): `(sub + 0.5) / SUBTILE_COUNT`.
#[inline]
fn subtile_center(sub: IVec2) -> Vec2 {
    let sc = SUBTILE_COUNT as f32;
    Vec2::new((sub.x as f32 + 0.5) / sc, (sub.y as f32 + 0.5) / sc)
}

/// Drops interior subtiles that lie on a straight run, keeping only the
/// endpoints and direction-change corners. A 4-connected subtile path is a
/// staircase; collapsing collinear runs turns it into a few smooth waypoints so
/// the follower doesn't visit every single subtile.
fn collapse_collinear_subtiles(path: &[IVec2]) -> Vec<IVec2> {
    if path.len() <= 2 {
        return path.to_vec();
    }
    let mut out = Vec::with_capacity(path.len());
    out.push(path[0]);
    for i in 1..path.len() - 1 {
        let prev_dir = path[i] - path[i - 1];
        let next_dir = path[i + 1] - path[i];
        if prev_dir != next_dir {
            out.push(path[i]);
        }
    }
    out.push(path[path.len() - 1]);
    out
}

/// Steers `velocity` toward `desired` by at most `rate * dt`, snapping to
/// `desired` once within one step (so the bot settles instead of oscillating).
/// One `sqrt` per call (see `OPTIMIZATION.md`).
#[inline]
pub fn approach_velocity(velocity: Vec2, desired: Vec2, rate: f32, dt: f32) -> Vec2 {
    let dv = desired - velocity;
    let max_step = rate * dt;
    let len = dv.length();
    if len <= max_step {
        desired
    } else {
        velocity + dv * (max_step / len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::hypermap::Hypermap;
    use crate::map::interactive_entity::InteractiveEntityMap;
    use rand::SeedableRng;

    #[test]
    fn reached_waypoint_uses_center_not_tile_membership() {
        let tile = (3, 4);
        let wp = waypoint_center(tile);
        let eps = 0.05;
        assert!(!reached_waypoint(wp + Vec2::new(0.2, 0.0), tile, eps));
        assert!(reached_waypoint(wp, tile, eps));
        assert!(reached_waypoint(wp + Vec2::splat(eps * 0.5), tile, eps));
    }

    #[test]
    fn approach_velocity_ramps_up_capped_by_accel() {
        let v = approach_velocity(Vec2::ZERO, Vec2::new(10.0, 0.0), 4.0, 0.5);
        assert!((v.x - 2.0).abs() < 1e-5, "expected 2.0, got {}", v.x);
        assert_eq!(v.y, 0.0);
    }

    #[test]
    fn approach_velocity_snaps_when_within_one_step() {
        let v = approach_velocity(Vec2::new(1.0, 0.0), Vec2::new(1.2, 0.0), 4.0, 0.5);
        assert_eq!(v, Vec2::new(1.2, 0.0));
    }

    #[test]
    fn approach_velocity_decelerates_toward_zero() {
        let v = approach_velocity(Vec2::new(3.0, 0.0), Vec2::ZERO, 4.0, 0.5);
        assert!((v.x - 1.0).abs() < 1e-5, "expected 1.0, got {}", v.x);
        let v2 = approach_velocity(v, Vec2::ZERO, 4.0, 0.5);
        assert_eq!(v2, Vec2::ZERO);
    }

    #[test]
    fn empty_path_is_finished() {
        let fp = FollowPath::new(Vec::new());
        assert!(fp.is_finished());
    }

    #[test]
    fn follow_path_bounces_velocity_on_bot_collision() {
        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);

        let mut state = ActorState {
            center: Vec2::new(5.0, 5.0),
            radius_subtiles: 2,
            rotation: 0.0,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: Some(ActorMovementError::BlockedByOccupancy {
                world_subtile_x: 26,
                world_subtile_y: 25,
            }),
            last_accepted_center_subtile: Some(IVec2::new(25, 25)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            avoidance: None,
        };
        let mut rng = StdRng::seed_from_u64(1);
        fp.execute(&mut state, &ctx, &mut rng, &FollowTuning::default());

        assert!(
            fp.velocity.x < 0.0,
            "collision with blocker on +X should produce reflected X velocity"
        );
    }

    #[test]
    fn follow_path_contact_reroute_inserts_escape_waypoint() {
        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);

        let mut state = ActorState {
            center: Vec2::new(5.0, 5.0),
            radius_subtiles: 2,
            rotation: 0.0,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: Some(ActorMovementError::BlockedByOccupancy {
                world_subtile_x: 26,
                world_subtile_y: 25,
            }),
            last_accepted_center_subtile: Some(IVec2::new(25, 25)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            avoidance: None,
        };
        let mut rng = StdRng::seed_from_u64(2);
        let mut tuning = FollowTuning::default();
        tuning.bot_reroute_chance = 1.0;
        tuning.bot_wait_chance = 0.0;
        let original_len = fp.path.len();

        fp.execute(&mut state, &ctx, &mut rng, &tuning);

        assert!(
            fp.path.len() > original_len,
            "bot collision reroute should insert an escape waypoint"
        );
    }

    #[test]
    fn follow_path_brakes_near_waypoint_with_decel_rate() {
        let mut fp = FollowPath::new(vec![(1, 0)]);
        fp.velocity = Vec2::new(1.2, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);

        let mut state = ActorState {
            center: Vec2::new(1.45, 0.5), // within braking zone of waypoint center (1.5, 0.5)
            radius_subtiles: 2,
            rotation: 0.0,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(6, 2)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            main_tile: IVec2::new(1, 0),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            avoidance: None,
        };
        let mut rng = StdRng::seed_from_u64(7);

        fp.execute(&mut state, &ctx, &mut rng, &FollowTuning::default());

        assert!(
            fp.velocity.x < 1.2,
            "velocity should be reduced near waypoint to brake sooner"
        );
    }

    #[test]
    fn follow_path_sets_stuck_status_after_no_progress() {
        let mut fp = FollowPath::new(vec![(10, 10)]);
        let mut state = ActorState {
            center: Vec2::new(0.5, 0.5),
            radius_subtiles: 2,
            rotation: 0.0,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(2, 2)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let mut rng = StdRng::seed_from_u64(11);
        let tuning = FollowTuning {
            stuck_repath_secs: 0.3,
            ..FollowTuning::default()
        };

        for _ in 0..4 {
            let ctx = BrainContext {
                entity: Entity::PLACEHOLDER,
                dt: 0.1,
                center: state.center,
                main_tile: IVec2::new(0, 0),
                main_tile_changed: false,
                floor: 0,
                charge: 1.0,
                missing_charge_pct: 0.0,
                depleted: false,
                broken: false,
                passability: &passability,
                interactive: &interactive,
                avoidance: None,
            };
            fp.execute(&mut state, &ctx, &mut rng, &tuning);
            // Simulate being physically pinned: position never changes.
            state.move_buffer = ActorMoveBuffer::default();
        }

        assert!(fp.is_stuck(), "no-progress route should mark low-level action as stuck");
        assert!(fp.is_finished(), "stuck route must request replanning");
    }

    fn detour_state(center_subtile: IVec2, blocker_subtile: IVec2) -> ActorState {
        ActorState {
            center: Vec2::new(5.0, 5.0),
            radius_subtiles: 2,
            rotation: 0.0,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: Some(ActorMovementError::BlockedByOccupancy {
                world_subtile_x: blocker_subtile.x,
                world_subtile_y: blocker_subtile.y,
            }),
            last_accepted_center_subtile: Some(center_subtile),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
        }
    }

    #[test]
    fn follow_path_plans_subtile_detour_to_next_node_on_bot_bump() {
        use crate::actor::brain::AvoidanceViews;
        use crate::map::passability::{
            DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID,
        };

        let dynamic = DynamicPassabilityMap::new();
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);

        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);

        let mut state = detour_state(IVec2::new(25, 25), IVec2::new(27, 25));
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            avoidance: Some(AvoidanceViews {
                dynamic: &dynamic,
                static_subtiles: &static_subtiles,
                blocked_flags: FLAG_BLOCKED | FLAG_VOID,
            }),
        };
        let mut rng = StdRng::seed_from_u64(3);
        let tuning = FollowTuning {
            bot_reroute_chance: 0.0,
            bot_wait_chance: 0.0,
            bot_subtile_detour_chance: 1.0,
            ..FollowTuning::default()
        };

        fp.execute(&mut state, &ctx, &mut rng, &tuning);

        assert!(
            !fp.detour.is_empty(),
            "a clear map must yield a subtile detour toward the next node"
        );
        let goal_center = waypoint_center((8, 5));
        let last = *fp.detour.last().expect("detour has waypoints");
        assert!(
            (last - goal_center).length() < 0.11,
            "detour must terminate at the next path node center, got {last:?}"
        );
    }

    #[test]
    fn follow_path_makes_no_detour_when_next_node_is_walled_off() {
        use crate::actor::brain::AvoidanceViews;
        use crate::map::passability::{
            DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID,
        };

        let dynamic = DynamicPassabilityMap::new();
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);
        // Wall off the goal tile and every neighbour so no footprint reaches it.
        let mut solid = SubtilePassability::EMPTY;
        for r in 0..SUBTILE_COUNT {
            for c in 0..SUBTILE_COUNT {
                solid.or_flags(r, c, FLAG_BLOCKED);
            }
        }
        for tx in 6..=9 {
            for ty in 4..=6 {
                static_subtiles.set(tx, ty, solid);
            }
        }

        let mut fp = FollowPath::new(vec![(8, 5)]);
        let mut state = detour_state(IVec2::new(25, 25), IVec2::new(27, 25));
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            avoidance: Some(AvoidanceViews {
                dynamic: &dynamic,
                static_subtiles: &static_subtiles,
                blocked_flags: FLAG_BLOCKED | FLAG_VOID,
            }),
        };
        let mut rng = StdRng::seed_from_u64(4);
        let tuning = FollowTuning {
            bot_reroute_chance: 0.0,
            bot_wait_chance: 0.0,
            bot_subtile_detour_chance: 1.0,
            ..FollowTuning::default()
        };

        fp.execute(&mut state, &ctx, &mut rng, &tuning);

        assert!(
            fp.detour.is_empty(),
            "an unreachable next node must not produce a detour"
        );
    }
}

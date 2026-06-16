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
//!   net, and the bot-on-bot response — a deterministic elastic bounce + step
//!   back to the previously occupied cell, with a subtile detour as fallback —
//!   see [`FollowTuning`].

use bevy::prelude::*;
use crate::rng::{self, StdRng};

use crate::actor::{
    is_front_collision, occupancy_collision_normal, reflect_velocity, ActorMoveBuffer,
    ActorMovementError, ActorState,
};
use crate::map::hypermap_pathfind::world_tile_walkable;
use crate::map::passability::{FLAG_CREATURE, SUBTILE_COUNT};
use crate::map::pathfind_service::{PathKind, PathOutcome, PathfindReason, RequestId};

use super::path::PathNode;
use super::BrainContext;

/// Bounding-box margin (subtiles) added around the start/goal of a bot-on-bot
/// subtile detour search. Lets the route bulge out by ~1 tile to get around a
/// neighbour.
const DETOUR_PAD_SUBTILES: i32 = 6;
/// Maximum Manhattan span (subtiles) between the actor and the next path node
/// for which a subtile detour is attempted — keeps it a *short* local maneuver
/// (`140 subtiles = 28 tiles`).
const DETOUR_MAX_SPAN_SUBTILES: i32 = 140;
/// Hard cap on subtile A\* node expansions for a detour (safety bound).
const DETOUR_MAX_EXPANDED: usize = 1024;
/// Seconds [`FollowPath`] coasts/holds awaiting a queued subtile-detour result
/// before falling back to a step-aside. The detour is a local avoidance hint, so
/// it must resolve quickly or be abandoned.
const DETOUR_WAIT_S: f32 = 0.5;

/// Inclusive bounds (seconds) of the pause a bot holds after stepping aside from
/// a head-on bot-on-bot bump.
const STEP_BACK_WAIT_MIN_SECS: f32 = 0.5;
const STEP_BACK_WAIT_MAX_SECS: f32 = 1.5;
/// Speed (tiles/s) below which a post-step-aside hold counts as "stopped".
const CONTACT_WAIT_STOP_SPEED: f32 = 0.08;
/// Half-width (tiles) of the square scanned for a free cell when a stalled bot
/// tries to relocate before it reschedules. Kept small so the search is cheap
/// and the bot only backs out to a *nearby* clear cell.
const ESCAPE_SEARCH_TILES: i32 = 4;

/// Speed² (tiles/s)² below which a bot is too slow for proactive look-ahead
/// avoidance to bother probing (a near-stopped bot isn't about to collide).
const LOOKAHEAD_MIN_SPEED_SQ: f32 = 0.04;
/// The eight Moore-neighbour offsets, used by the on-collision step-aside probe.
const NEIGHBOR_DIRS: [(i32, i32); 8] = [
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
];

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
    /// Probability per **head-on** bot-on-bot bump of routing a subtile detour
    /// around the blocker; otherwise the bot steps aside into a free neighbour
    /// (preferring cells ahead of its heading), or **waits** if all eight
    /// neighbours are taken. (Rear bumps are ignored entirely.)
    pub bot_detour_chance: f32,
}

impl Default for FollowTuning {
    fn default() -> Self {
        Self {
            max_speed: 1.2,
            accel: 2.5,
            decel: 6.0,
            waypoint_eps: 0.05,
            stuck_repath_secs: 1.0,
            stuck_progress_eps: 0.05,
            bot_detour_chance: 0.5,
        }
    }
}

/// Discriminant for the concrete low-level action, so callers can branch on the
/// kind without comparing [`LowLevelAction::label`] strings (`label` is for the
/// inspector only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LowLevelKind {
    Idle,
    Wait,
    PendingPath,
    FollowPath,
}

/// The per-frame contract every low-level action implements.
pub trait LowLevelAction: Send + Sync {
    /// Which concrete action this is — typed, for control-flow branching.
    fn kind(&self) -> LowLevelKind;

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

    /// Active route + cursor, if this action follows one (overlay / inspector).
    /// Nodes are unified [`PathNode`]s — consumers read `center()` / `tile()`.
    fn route(&self) -> Option<(&[PathNode], usize)> {
        None
    }

    /// Current velocity (inspector). Default zero.
    fn velocity(&self) -> Vec2 {
        Vec2::ZERO
    }

    /// The bot's intended **movement direction** as a unit vector, published to
    /// [`ActorState::heading`] so other bots can read its facing. Distinct from
    /// [`velocity`](Self::velocity): when the bot is wedged or braking its
    /// velocity is ~zero, but it still *intends* to head toward its next path
    /// node — that intent is the heading. Default `Vec2::ZERO` (no direction).
    fn heading(&self) -> Vec2 {
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

    /// `true` while the action is intentionally holding instead of following a
    /// route — coasting on a pending async path, braked awaiting a detour, or
    /// paused after a step-aside (perf HUD `coast` gauge). Default `false`.
    fn is_awaiting_path(&self) -> bool {
        false
    }

    /// `true` while the action is running a **collision-recovery maneuver** —
    /// awaiting/threading a detour, stepping aside, pausing after a step, or
    /// escaping a jam. Collision pressure is suspended during these (they make
    /// the bot legitimately hold or move slowly), so a reset can't abort the
    /// recovery before it completes. Default `false`.
    fn is_recovering(&self) -> bool {
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
    fn kind(&self) -> LowLevelKind {
        LowLevelKind::Idle
    }
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
    /// When set, no displacement for `stuck_repath_secs` reports `is_stuck` so
    /// the high-level action can pick a different target (patrol / wander retry).
    detect_stall: bool,
    stall_timer: f32,
    stall_reference_pos: Option<Vec2>,
    stalled: bool,
}

impl Wait {
    pub fn new(seconds: f32) -> Self {
        Self {
            remaining_s: seconds,
            detect_stall: false,
            stall_timer: 0.0,
            stall_reference_pos: None,
            stalled: false,
        }
    }

    /// Retry delay that abandons when the bot has not moved for
    /// [`FollowTuning::stuck_repath_secs`].
    pub fn retry(seconds: f32) -> Self {
        Self {
            remaining_s: seconds,
            detect_stall: true,
            stall_timer: 0.0,
            stall_reference_pos: None,
            stalled: false,
        }
    }
}

impl LowLevelAction for Wait {
    fn kind(&self) -> LowLevelKind {
        LowLevelKind::Wait
    }
    fn execute(&mut self, state: &mut ActorState, ctx: &BrainContext, _rng: &mut StdRng, t: &FollowTuning) {
        self.remaining_s -= ctx.dt;
        state.move_buffer = ActorMoveBuffer::default();
        state.next_waypoint_hint = None;

        if self.detect_stall && !self.stalled {
            let center = state.center;
            let in_queue = ctx.interactive.is_in_any_queue(ctx.entity);
            tick_stall_timer(
                &mut self.stall_timer,
                &mut self.stall_reference_pos,
                center,
                ctx.dt,
                t.stuck_progress_eps,
                in_queue,
            );
            if self.stall_timer >= t.stuck_repath_secs {
                self.stalled = true;
            }
        }
    }
    fn is_finished(&self) -> bool {
        self.remaining_s <= 0.0 || self.stalled
    }
    fn is_stuck(&self) -> bool {
        self.stalled
    }
    fn stuck_timer(&self) -> f32 {
        self.stall_timer
    }
    fn label(&self) -> String {
        if self.stalled {
            return "Wait (stalled)".to_string();
        }
        if self.remaining_s.is_finite() {
            format!("Wait ({:.1}s)", self.remaining_s.max(0.0))
        } else {
            "Wait".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// PendingPath
// ---------------------------------------------------------------------------

/// "Waiting-for-path" hold: the bot has chosen a goal and enqueued a route
/// request but does not have a path yet. It parks in place with inertial braking
/// (so it doesn't snap-freeze mid-coast) until the owning high-level action reads
/// the result and swaps in a [`FollowPath`]. Never finishes on its own; the
/// high-level action owns the 3 s retry via [`super::high_level`].
pub struct PendingPath {
    velocity: Vec2,
    prev_center: Option<Vec2>,
}

impl PendingPath {
    pub fn new() -> Self {
        Self::with_velocity(Vec2::ZERO)
    }

    /// Carries the previous low-level action's velocity into the hold so the bot
    /// coasts to a stop under inertia instead of snapping still when its route
    /// finishes and it starts waiting for the next path.
    pub fn with_velocity(velocity: Vec2) -> Self {
        Self {
            velocity,
            prev_center: None,
        }
    }
}

impl Default for PendingPath {
    fn default() -> Self {
        Self::new()
    }
}

impl LowLevelAction for PendingPath {
    fn kind(&self) -> LowLevelKind {
        LowLevelKind::PendingPath
    }
    fn execute(&mut self, state: &mut ActorState, ctx: &BrainContext, _rng: &mut StdRng, t: &FollowTuning) {
        let dt = ctx.dt;
        let center = state.center;
        if dt > 1e-6 {
            if let Some(prev) = self.prev_center {
                let achieved = (center - prev) / dt;
                if achieved.x.abs() < self.velocity.x.abs() {
                    self.velocity.x = achieved.x;
                }
                if achieved.y.abs() < self.velocity.y.abs() {
                    self.velocity.y = achieved.y;
                }
            }
        }
        self.velocity = approach_velocity(self.velocity, Vec2::ZERO, t.decel, dt);
        let delta = self.velocity * dt;
        state.move_buffer.tile_delta = delta;
        state.move_buffer.subtile_shift = float_subtile(center + delta) - float_subtile(center);
        state.move_buffer.rotation_shift = 0.0;
        self.prev_center = Some(center);
        state.next_waypoint_hint = None;
    }
    fn is_finished(&self) -> bool {
        false
    }
    fn halt(&mut self) {
        self.velocity = Vec2::ZERO;
        self.prev_center = None;
    }
    fn label(&self) -> String {
        "PendingPath".to_string()
    }
    fn velocity(&self) -> Vec2 {
        self.velocity
    }
    fn heading(&self) -> Vec2 {
        // No route yet — the best available direction is the coasting velocity.
        self.velocity.normalize_or_zero()
    }
    fn is_awaiting_path(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// FollowPath
// ---------------------------------------------------------------------------

/// Steers the actor along a simplified waypoint path with mass/inertia. Reaches
/// `is_finished() == true` when the path is exhausted or abandoned (stuck), at
/// which point the owning high-level action re-plans.
pub struct FollowPath {
    /// Unified route: coarse [`PathNode::Cell`] waypoints from the tile A\*, with
    /// fine [`PathNode::Sub`] detour waypoints spliced in at the cursor when the
    /// bot routes around an obstacle. One list, one cursor.
    pub path: Vec<PathNode>,
    pub index: usize,
    /// Unit heading toward `path[index]`; recomputed every moving frame.
    direction: Vec2,
    /// Carries momentum between frames; steered under finite acceleration.
    velocity: Vec2,
    /// Last frame's center, used to bleed momentum lost to wall collisions.
    prev_center: Option<Vec2>,
    stuck_timer: f32,
    stuck_reference_pos: Option<Vec2>,
    /// Current main tile, tracked across frames so a bot-on-bot bump can retreat
    /// to [`prev_tile`](Self::prev_tile) — the cell it stood in just before.
    last_tile: Option<IVec2>,
    /// The distinct main tile occupied immediately before [`last_tile`](Self::last_tile).
    /// `None` until the bot has crossed at least one tile boundary on this path.
    prev_tile: Option<IVec2>,
    /// Remaining post-step-aside pause (seconds); position is held while `> 0`.
    contact_wait_s: f32,
    /// A queued step-aside pause: `(target tile, seconds)`. The pause begins once
    /// the bot reaches `target`, so it retreats *then* waits. `None` = no pending wait.
    pending_wait: Option<((i32, i32), f32)>,
    /// Time spent threading the current spliced `Sub` run; bounds it to
    /// `stuck_repath_secs` so a detour that stops making progress is dropped.
    detour_timer: f32,
    /// In-flight queued subtile-detour request id (head-on bump that chose to
    /// detour, or a stall splice-repair). While set, the bot holds and awaits it.
    detour_request: Option<RequestId>,
    /// Whether the in-flight `detour_request` is avoidance (fall back to a
    /// step-aside) or a stall repair (fall back to escape/abandon).
    detour_purpose: DetourPurpose,
    /// The path-node index the in-flight detour targets. On install the spliced
    /// `Sub` run **replaces** `index..detour_goal`, so an avoidance detour
    /// (`goal == index`) is a pure insert before the next node while a stall
    /// repair (`goal > index`) drops the wedged cell(s) it routed around.
    detour_goal: usize,
    /// Seconds awaiting `detour_request` before falling back.
    detour_wait_elapsed: f32,
    /// Step-aside tile used if an avoidance detour comes back `NoPath`/times out.
    detour_fallback: Option<(i32, i32)>,
    /// Blocker subtile of the last head-on occupancy bump we already reacted to.
    /// Suppresses re-bouncing / detour replanning every frame while two bots stay
    /// pressed together (mirrors the stuck-log rising edge in `black_bot_brain`).
    last_head_on_bump: Option<IVec2>,
    /// Index in [`path`](Self::path) of a step-aside cell inserted by the bump
    /// handler. Cleared once the bot reaches that cell or chooses a detour instead.
    step_aside_at: Option<usize>,
    /// When the stall timer fires, the bot first relocates to the center of the
    /// nearest free cell (`Some(tile)`) and only marks itself abandoned once it
    /// arrives — so it vacates a chokepoint before the high level reschedules.
    escape_target: Option<(i32, i32)>,
    /// Latches a single local splice-repair attempt per stall episode; reset when
    /// the bot makes progress (reaches a new node).
    repair_attempted: bool,
    /// Set when the stuck timer fires; makes `is_finished` report `true`.
    abandoned: bool,
}

/// Why an in-flight subtile detour was requested, which decides its fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetourPurpose {
    /// Bot-on-bot avoidance: on failure, step aside / wait.
    Avoidance,
    /// Stall splice-repair: on failure, escape to a free cell / abandon.
    Repair,
    /// Wall re-route: a subtile subpath around static geometry toward the next
    /// waypoint. On failure it never escalates to escape/abandon and never marks
    /// the bot stuck — walls are answered only by re-routing, so it just resets
    /// the stall state and keeps steering (the movement pipeline's wall-slide
    /// still applies). Re-armable every stall, unlike one-shot [`Repair`].
    WallRepair,
}

impl FollowPath {
    /// Builds a follower from a tile-level world route. The coarse cells become
    /// [`PathNode::Cell`]s — this is the install boundary where `PathOutcome::Route`
    /// is converted, so the async search layer never depends on [`PathNode`].
    pub fn new(path: Vec<(i32, i32)>) -> Self {
        Self::from_nodes(path.into_iter().map(|(x, y)| PathNode::cell(x, y)).collect())
    }

    /// Builds a follower from already-unified nodes.
    pub fn from_nodes(path: Vec<PathNode>) -> Self {
        Self {
            path,
            index: 0,
            direction: Vec2::X,
            velocity: Vec2::ZERO,
            prev_center: None,
            stuck_timer: 0.0,
            stuck_reference_pos: None,
            last_tile: None,
            prev_tile: None,
            contact_wait_s: 0.0,
            pending_wait: None,
            detour_timer: 0.0,
            detour_request: None,
            detour_purpose: DetourPurpose::Avoidance,
            detour_goal: 0,
            detour_wait_elapsed: 0.0,
            detour_fallback: None,
            last_head_on_bump: None,
            step_aside_at: None,
            escape_target: None,
            repair_attempted: false,
            abandoned: false,
        }
    }

    /// `true` while the cursor points at a spliced `Sub` node (threading a detour).
    fn on_sub_run(&self) -> bool {
        self.path.get(self.index).is_some_and(|n| !n.is_cell())
    }

    /// Drops the contiguous run of un-consumed `Sub` nodes at the cursor, so the
    /// bot rejoins the coarse cell path. The unified-path equivalent of clearing
    /// the old separate detour vec.
    fn drop_sub_run(&mut self) {
        let mut end = self.index;
        while end < self.path.len() && !self.path[end].is_cell() {
            end += 1;
        }
        if end > self.index {
            self.path.drain(self.index..end);
        }
        self.detour_timer = 0.0;
    }

    /// Drops a collision-inserted step-aside waypoint that has not been reached yet.
    fn clear_step_aside_insertion(&mut self) {
        let Some(at) = self.step_aside_at else {
            return;
        };
        if at < self.path.len() {
            self.path.remove(at);
            if self.index > at {
                self.index -= 1;
            }
        }
        self.step_aside_at = None;
    }

    fn clear_detour_request(&mut self) {
        self.detour_request = None;
        self.detour_purpose = DetourPurpose::Avoidance;
        self.detour_wait_elapsed = 0.0;
        self.detour_fallback = None;
    }

    /// Enqueues a short subtile-level detour search toward the node at
    /// `goal_node`, footprint-aware around other creatures. Returns the request
    /// id, or `None` when avoidance data / a pathfind queue is unavailable or
    /// there is no such node — in which case the caller falls back.
    fn enqueue_detour(
        &self,
        state: &ActorState,
        ctx: &BrainContext,
        goal_node: usize,
    ) -> Option<RequestId> {
        let views = ctx.avoidance.as_ref()?;
        let pf = ctx.pathfind.as_ref()?;
        let (goal_tx, goal_ty) = self.path.get(goal_node)?.tile();
        let radius = state.radius_subtiles;
        let sc = SUBTILE_COUNT as i32;
        let start = state
            .last_accepted_center_subtile
            .unwrap_or_else(|| float_subtile(state.center));
        // Center subtile of the goal tile (`sc / 2` lands on the middle column).
        let goal = IVec2::new(goal_tx * sc + sc / 2, goal_ty * sc + sc / 2);
        Some(pf.queue.enqueue(PathKind::SubtileDetour {
            start,
            goal,
            pad: DETOUR_PAD_SUBTILES,
            max_span: DETOUR_MAX_SPAN_SUBTILES,
            max_expanded: DETOUR_MAX_EXPANDED,
            radius,
            blocked_flags: views.blocked_flags,
        }, ctx.entity, PathfindReason::SubtileDetour))
    }

    /// Splices an arrived subtile detour (raw subtile path including the start)
    /// into the unified path, **replacing** `index..detour_goal`: collapse
    /// collinear runs, drop the start, convert to `Sub` nodes. For an avoidance
    /// detour (`goal == index`) this is a pure insert before the next node; for a
    /// stall repair (`goal > index`) it drops the wedged cell(s) it routed around.
    /// A trailing `Sub` coincident with the goal node is dropped to avoid a
    /// duplicate waypoint.
    fn install_detour(&mut self, subtiles: &[IVec2]) {
        // Drop any unreached step-aside cell first so its stored index stays
        // valid (the splice below shifts indices at/after the cursor).
        self.clear_step_aside_insertion();
        let goal = self.detour_goal.clamp(self.index, self.path.len());
        let collapsed = collapse_collinear_subtiles(subtiles);
        let mut nodes: Vec<PathNode> =
            collapsed.into_iter().skip(1).map(PathNode::Sub).collect();
        if let (Some(last), Some(goal_node)) = (nodes.last(), self.path.get(goal)) {
            if last.tile() == goal_node.tile() {
                nodes.pop();
            }
        }
        self.path.splice(self.index..goal, nodes);
        self.detour_timer = 0.0;
    }

    /// Steps aside to `target` (insert a waypoint) and arms a post-step pause.
    /// `None` target leaves the bot to continue (stuck handling will take over).
    fn begin_step_aside(&mut self, target: Option<(i32, i32)>, rng: &mut StdRng) {
        if let Some(target) = target {
            let insert_idx = self.index.min(self.path.len());
            self.path.insert(insert_idx, PathNode::cell(target.0, target.1));
            self.step_aside_at = Some(insert_idx);
            // Force heading recalculation toward the step-aside tile.
            self.stuck_timer = 0.0;
            self.stuck_reference_pos = None;
            let secs = rng::range(rng, STEP_BACK_WAIT_MIN_SECS..=STEP_BACK_WAIT_MAX_SECS);
            self.pending_wait = Some((target, secs));
        }
    }

    /// Holds in place (brakes + pauses) for a random step-aside duration. Used
    /// when a head-on bump finds **every** neighbouring cell taken, so the bot
    /// has nowhere to step — it just waits for the jam to clear.
    fn begin_wait_in_place(&mut self, rng: &mut StdRng) {
        self.pending_wait = None;
        self.contact_wait_s = rng::range(rng, STEP_BACK_WAIT_MIN_SECS..=STEP_BACK_WAIT_MAX_SECS);
    }

    /// Steps aside to `target` if one was found, otherwise waits in place — the
    /// "all neighbours taken" fallback shared by the bump and detour-timeout
    /// paths.
    fn step_aside_or_wait(&mut self, target: Option<(i32, i32)>, rng: &mut StdRng) {
        match target {
            Some(_) => self.begin_step_aside(target, rng),
            None => self.begin_wait_in_place(rng),
        }
    }

    /// Whether the bot's whole footprint fits in the tile `(tx, ty)` clear of
    /// static geometry and other creatures (its own current footprint bypassed).
    /// Without avoidance data (unit tests) it falls back to a static walkability
    /// test on the tile center.
    fn neighbor_free(&self, state: &ActorState, ctx: &BrainContext, tx: i32, ty: i32) -> bool {
        if !world_tile_walkable(ctx.passability, tx, ty) {
            return false;
        }
        let Some(views) = ctx.avoidance.as_ref() else {
            return true;
        };
        let sc = SUBTILE_COUNT as i32;
        let radius = state.radius_subtiles;
        let start = state
            .last_accepted_center_subtile
            .unwrap_or_else(|| float_subtile(state.center));
        let goal = IVec2::new(tx * sc + sc / 2, ty * sc + sc / 2);
        views
            .dynamic
            .probe_footprint(goal, radius, Some((start, radius)), views.blocked_flags, views.static_subtiles)
            .is_ok()
    }

    /// The tile to step aside into on a head-on bump: probe all **eight**
    /// neighbouring cells, keep the free ones, and pick one at random —
    /// **preferring cells ahead of `heading`** (positive dot product) so the bot
    /// tends to slip forward around the blocker rather than retreat. Returns
    /// `None` only when every neighbour is taken (caller then waits in place).
    fn step_target(
        &self,
        state: &ActorState,
        ctx: &BrainContext,
        heading: Vec2,
        rng: &mut StdRng,
    ) -> Option<(i32, i32)> {
        let cur = ctx.main_tile;
        let h = heading.normalize_or_zero();
        let mut front: [(i32, i32); 8] = [(0, 0); 8];
        let mut back: [(i32, i32); 8] = [(0, 0); 8];
        let (mut nf, mut nb) = (0usize, 0usize);
        for (dx, dy) in NEIGHBOR_DIRS {
            let (tx, ty) = (cur.x + dx, cur.y + dy);
            if !self.neighbor_free(state, ctx, tx, ty) {
                continue;
            }
            // Dot with heading without a per-candidate sqrt: integer offset · h.
            let ahead = h != Vec2::ZERO && (dx as f32 * h.x + dy as f32 * h.y) > 0.0;
            if ahead {
                front[nf] = (tx, ty);
                nf += 1;
            } else {
                back[nb] = (tx, ty);
                nb += 1;
            }
        }
        if nf > 0 {
            Some(front[rng::range(rng, 0..nf)])
        } else if nb > 0 {
            Some(back[rng::range(rng, 0..nb)])
        } else {
            None
        }
    }

    /// Center of the current path waypoint, used as the off-screen-resolution
    /// hint while the bot holds position.
    fn current_waypoint_hint(&self) -> Option<Vec2> {
        self.path.get(self.index).map(|n| n.center())
    }

    /// The nearest cell whose center the bot's whole footprint can occupy clear
    /// of other creatures and static geometry (its own current footprint is
    /// bypassed, so the cell it already stands in is eligible). Returns the tile,
    /// or `None` when avoidance data is unavailable or nothing is free in range.
    fn find_escape_cell(&self, state: &ActorState, ctx: &BrainContext) -> Option<(i32, i32)> {
        let views = ctx.avoidance.as_ref()?;
        let sc = SUBTILE_COUNT as i32;
        let radius = state.radius_subtiles;
        let start = state
            .last_accepted_center_subtile
            .unwrap_or_else(|| float_subtile(state.center));
        let previous = Some((start, radius));
        let dynamic = views.dynamic;
        let static_subtiles = views.static_subtiles;
        let blocked = views.blocked_flags;
        let here = ctx.main_tile;
        let center = state.center;

        let mut best: Option<((i32, i32), f32)> = None;
        for ring in 0..=ESCAPE_SEARCH_TILES {
            for_each_chebyshev_ring(ring, |dx, dy| {
                let tile = (here.x + dx, here.y + dy);
                let goal_center = IVec2::new(tile.0 * sc + sc / 2, tile.1 * sc + sc / 2);
                if dynamic
                    .probe_footprint(goal_center, radius, previous, blocked, static_subtiles)
                    .is_ok()
                {
                    let d2 = (waypoint_center(tile) - center).length_squared();
                    if best.map_or(true, |(_, bd)| d2 < bd) {
                        best = Some((tile, d2));
                    }
                }
            });
            if let Some((_, bd)) = best {
                if ring < ESCAPE_SEARCH_TILES
                    && min_dist2_to_chebyshev_ring(center, here, ring + 1) > bd
                {
                    break;
                }
            }
        }
        best.map(|(t, _)| t)
    }

    /// Drives toward the active [`escape_target`](Self::escape_target). On arrival
    /// the bot is abandoned (so the high level reschedules from a clean, centered
    /// cell); if it cannot make progress toward the free cell either it abandons
    /// anyway as a safety valve.
    fn run_escape(&mut self, state: &mut ActorState, ctx: &BrainContext, t: &FollowTuning, dt: f32) {
        let center = state.center;
        let Some(target) = self.escape_target else {
            return;
        };

        if reached_waypoint(center, target, t.waypoint_eps) {
            ctx.trace(format!("escape reached {target:?} → replan"));
            self.finish_escape(state);
            return;
        }

        let in_queue = ctx.interactive.is_in_any_queue(ctx.entity);
        tick_stall_timer(
            &mut self.stuck_timer,
            &mut self.stuck_reference_pos,
            center,
            dt,
            t.stuck_progress_eps,
            in_queue,
        );
        if self.stuck_timer >= t.stuck_repath_secs {
            ctx.trace(format!(
                "escape to {target:?} STALLED ({:.2},{:.2}) → abandon",
                center.x, center.y
            ));
            self.finish_escape(state);
            return;
        }

        let target_pos = waypoint_center(target);
        let to_wp = target_pos - center;
        let dist = to_wp.length();
        if dist > 1e-6 {
            self.direction = to_wp / dist;
        }
        let brake_limited_speed = (2.0 * t.decel * dist).sqrt();
        let desired_speed = t.max_speed.min(brake_limited_speed);
        let desired = self.direction * desired_speed;
        let steer_rate = if self.velocity.length() > desired_speed {
            t.decel
        } else {
            t.accel
        };
        self.drive(state, center, desired, steer_rate, dt);
        state.next_waypoint_hint = Some(target_pos);
    }

    /// Ends an escape maneuver and marks the route abandoned so the owning
    /// high-level action replans on the next tick.
    fn finish_escape(&mut self, state: &mut ActorState) {
        self.escape_target = None;
        self.abandoned = true;
        self.drop_sub_run();
        self.repair_attempted = false;
        self.last_head_on_bump = None;
        self.velocity = Vec2::ZERO;
        self.prev_center = None;
        state.move_buffer = ActorMoveBuffer::default();
        state.next_waypoint_hint = None;
    }

    /// Records the bot's current main tile, shifting the previous one into
    /// [`prev_tile`](Self::prev_tile) whenever it crosses a tile boundary.
    fn track_tiles(&mut self, main_tile: IVec2) {
        match self.last_tile {
            Some(t) if t != main_tile => {
                self.prev_tile = Some(t);
                self.last_tile = Some(main_tile);
            }
            None => self.last_tile = Some(main_tile),
            _ => {}
        }
    }

    fn advance_past_reached(&mut self, center: Vec2, eps: f32) {
        while self.index < self.path.len() && node_reached(center, self.path[self.index], eps) {
            self.index += 1;
        }
    }

    /// Last-resort stall recovery, shared by the stuck tail and a failed
    /// splice-repair: relocate to the nearest free cell (vacating the chokepoint
    /// before rescheduling), or abandon in place when no free cell / avoidance
    /// data is available (headless tests).
    fn escape_or_abandon(&mut self, state: &mut ActorState, ctx: &BrainContext, t: &FollowTuning, dt: f32) {
        let center = state.center;
        if let Some(target) = self.find_escape_cell(state, ctx) {
            ctx.trace(format!(
                "stall → escape to tile {target:?} (center {:.2},{:.2}, main {:?})",
                center.x, center.y, ctx.main_tile
            ));
            self.escape_target = Some(target);
            self.drop_sub_run();
            self.last_head_on_bump = None;
            self.pending_wait = None;
            self.stuck_timer = 0.0;
            self.stuck_reference_pos = Some(center);
            self.run_escape(state, ctx, t, dt);
        } else {
            ctx.trace(format!(
                "stall, NO free escape cell → abandon in place (main {:?})",
                ctx.main_tile
            ));
            self.abandoned = true;
            self.drop_sub_run();
            self.last_head_on_bump = None;
            self.velocity = Vec2::ZERO;
            self.prev_center = None;
            state.move_buffer = ActorMoveBuffer::default();
            state.next_waypoint_hint = None;
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
}

impl LowLevelAction for FollowPath {
    fn execute(&mut self, state: &mut ActorState, ctx: &BrainContext, rng: &mut StdRng, t: &FollowTuning) {
        let dt = ctx.dt;
        let center = state.center;
        self.track_tiles(ctx.main_tile);

        if !matches!(
            state.last_movement_error,
            Some(ActorMovementError::BlockedByOccupancy { .. })
        ) {
            self.last_head_on_bump = None;
        }

        // Relocating to a free cell after a stall takes over the whole tick: the
        // bot vacates whatever chokepoint it was wedged in before it reschedules.
        if self.escape_target.is_some() {
            self.run_escape(state, ctx, t, dt);
            return;
        }

        // Awaiting a queued subtile detour: hold (inertial brake) until the result
        // lands, then splice it into the path; on NoPath / timeout fall back per
        // purpose — avoidance steps aside, a stall repair escapes / abandons.
        if let Some(id) = self.detour_request {
            let outcome = ctx.pathfind.as_ref().and_then(|pf| pf.results.take(id));
            let failed = match outcome {
                Some(PathOutcome::Detour(subtiles)) => {
                    self.clear_detour_request();
                    self.install_detour(&subtiles);
                    false
                }
                Some(_) => true,
                None => {
                    self.detour_wait_elapsed += dt;
                    if self.detour_wait_elapsed >= DETOUR_WAIT_S {
                        true
                    } else {
                        self.drive(state, center, Vec2::ZERO, t.decel, dt);
                        state.next_waypoint_hint = self.current_waypoint_hint();
                        return;
                    }
                }
            };
            if failed {
                let purpose = self.detour_purpose;
                let fallback = self.detour_fallback;
                self.clear_detour_request();
                match purpose {
                    DetourPurpose::Avoidance => self.step_aside_or_wait(fallback, rng),
                    DetourPurpose::Repair => {
                        self.escape_or_abandon(state, ctx, t, dt);
                        return;
                    }
                    DetourPurpose::WallRepair => {
                        // A wall re-route could not be planned/landed: never escape
                        // or abandon for a wall. Reset the stall state and keep
                        // steering toward the waypoint (wall-slide still applies);
                        // the bot re-routes again next stall if still wedged.
                        self.stuck_timer = 0.0;
                        self.stuck_reference_pos = Some(center);
                    }
                }
            }
        }

        // Holding position after a step-aside: brake to a stop, then count down the
        // pause. The timer only runs once velocity has settled so the bot does not
        // snap frozen mid-coast.
        if self.contact_wait_s > 0.0 {
            self.drive(state, center, Vec2::ZERO, t.decel, dt);
            state.next_waypoint_hint = self.current_waypoint_hint();
            if self.velocity.length_squared()
                <= CONTACT_WAIT_STOP_SPEED * CONTACT_WAIT_STOP_SPEED
            {
                self.velocity = Vec2::ZERO;
                self.prev_center = None;
                self.contact_wait_s -= dt;
            }
            return;
        }

        // Proactive look-ahead avoidance (on-screen only). Before a collision
        // even happens, advance the bot's **whole footprint** one subtile along
        // its heading and test it for another creature: a subcell within the
        // bot's radius of an occupied subcell is in fact impassable, so a wide
        // bot must probe its leading arc, not a single cell. The bot's own
        // current footprint is exempted (`previous`), so only the newly-entered
        // leading crescent is checked. If a creature sits there, route a subtile
        // detour toward the next path node and hold — steering around instead of
        // pressing into the jam. Off-screen bots advance without occupancy, so
        // this is skipped for them.
        if ctx.on_screen
            && !self.on_sub_run()
            && self.detour_request.is_none()
            && self.pending_wait.is_none()
            && self.step_aside_at.is_none()
            && self.velocity.length_squared() > LOOKAHEAD_MIN_SPEED_SQ
        {
            if let Some(views) = ctx.avoidance.as_ref() {
                let radius = state.radius_subtiles;
                let start = state
                    .last_accepted_center_subtile
                    .unwrap_or_else(|| float_subtile(center));
                // One subtile forward along the heading (reuse the maintained unit
                // `direction` — no per-tick sqrt).
                let lead = IVec2::new(
                    self.direction.x.round() as i32,
                    self.direction.y.round() as i32,
                );
                let advanced = start + lead;
                // Footprint-aware creature test: `FLAG_CREATURE` never appears in
                // static geometry, so this trips only on another bot's body in the
                // leading footprint, leaving walls to the slide / `WallRepair` path.
                let creature_ahead = lead != IVec2::ZERO
                    && views
                        .dynamic
                        .probe_footprint(
                            advanced,
                            radius,
                            Some((start, radius)),
                            FLAG_CREATURE,
                            views.static_subtiles,
                        )
                        .is_err();
                if creature_ahead {
                    if let Some(id) = self.enqueue_detour(state, ctx, self.index) {
                        ctx.trace(format!("look-ahead: creature in leading footprint at {advanced:?} → detour"));
                        self.detour_request = Some(id);
                        self.detour_purpose = DetourPurpose::Avoidance;
                        self.detour_goal = self.index;
                        self.detour_wait_elapsed = 0.0;
                        self.detour_fallback = self.step_target(state, ctx, self.direction, rng);
                        self.drive(state, center, Vec2::ZERO, t.decel, dt);
                        state.next_waypoint_hint = self.current_waypoint_hint();
                        return;
                    }
                }
            }
        }

        // Bot-on-bot bump response. Only **head-on** (front/side) contacts
        // provoke a reaction — a rear bump (blocker behind the heading) is
        // ignored, no bounce/step/detour. On a head-on bump the bot bounces, then
        // either routes a subtile detour (`bot_detour_chance`) or steps aside
        // into a free neighbour (front-preferred), or waits if all are taken.
        if let Some(ActorMovementError::BlockedByOccupancy {
            world_subtile_x,
            world_subtile_y,
        }) = state.last_movement_error.clone()
        {
            let heading = if self.velocity.length_squared() > 1e-8 {
                self.velocity
            } else {
                self.direction
            };
            let blocker = IVec2::new(world_subtile_x, world_subtile_y);
            if is_front_collision(center, heading, world_subtile_x, world_subtile_y) {
                // Rising edge only: while two bots stay pressed together the
                // movement error persists every frame, but we must not bounce or
                // replan a detour on each one — finish the maneuver in flight.
                if self.last_head_on_bump != Some(blocker) {
                    self.last_head_on_bump = Some(blocker);

                    let normal = occupancy_collision_normal(center, world_subtile_x, world_subtile_y);
                    self.velocity = reflect_velocity(self.velocity, normal);
                    if self.velocity.length_squared() > 1e-8 {
                        self.direction = self.velocity.normalize();
                    } else {
                        self.direction = -self.direction;
                    }
                    // Skip achieved-vs-planned clamping this frame so reflection is preserved.
                    self.prev_center = None;

                    // A fresh bump invalidates any in-progress detour, pending pause,
                    // unreached step-aside insertion, or awaited detour before re-deciding.
                    self.drop_sub_run();
                    self.clear_detour_request();
                    self.pending_wait = None;
                    self.clear_step_aside_insertion();

                    // Pick the step-aside cell (front-preferred free neighbour;
                    // `None` = every neighbour taken → wait). Roll the detour vs
                    // step-aside response with the unchanged `bot_detour_chance`.
                    let step = self.step_target(state, ctx, heading, rng);
                    let want_detour = rng::chance(rng, t.bot_detour_chance);
                    // A detour is planned off-thread: enqueue the request and hold;
                    // when it lands the bot threads it, otherwise it steps aside / waits.
                    let enqueued = want_detour
                        .then(|| self.enqueue_detour(state, ctx, self.index))
                        .flatten();
                    match enqueued {
                        Some(id) => {
                            ctx.trace(format!("bump at {blocker:?} → detour (fallback {step:?})"));
                            self.detour_request = Some(id);
                            self.detour_purpose = DetourPurpose::Avoidance;
                            self.detour_goal = self.index;
                            self.detour_wait_elapsed = 0.0;
                            self.detour_fallback = step;
                        }
                        None => match step {
                            Some(c) => {
                                ctx.trace(format!("bump at {blocker:?} → step aside to {c:?}"));
                                self.begin_step_aside(step, rng);
                            }
                            // Genuine wedge: no free immediate neighbour. A bare
                            // wait-in-place here would suspend BOTH the stuck-timer
                            // escape (skipped by the early return) and the
                            // collision-pressure relocate (gated off while
                            // `is_recovering`), so two big bots pressed together in
                            // open space deadlock forever. Try the wider escape
                            // search (nearest fully-free cell, radius
                            // `ESCAPE_SEARCH_TILES`) first; only wait if even that
                            // finds nothing (genuinely boxed in / no avoidance data).
                            None => {
                                if let Some(target) = self.find_escape_cell(state, ctx) {
                                    ctx.trace(format!(
                                        "bump at {blocker:?} → neighbours taken, escape to {target:?}"
                                    ));
                                    self.escape_target = Some(target);
                                    self.drop_sub_run();
                                    self.last_head_on_bump = None;
                                    self.pending_wait = None;
                                    self.stuck_timer = 0.0;
                                    self.stuck_reference_pos = Some(center);
                                    self.run_escape(state, ctx, t, dt);
                                    return;
                                }
                                ctx.trace(format!(
                                    "bump at {blocker:?} → neighbours taken, no escape → wait"
                                ));
                                self.begin_wait_in_place(rng);
                            }
                        },
                    }
                }
            }
        }

        // Bound a spliced `Sub` run: while threading detour subcells, accumulate a
        // timer and drop the remaining run once it stops making progress, so the
        // bot rejoins its coarse cells and normal stuck handling can take over.
        if self.on_sub_run() {
            self.detour_timer += dt;
            if self.detour_timer > t.stuck_repath_secs {
                self.drop_sub_run();
            }
        } else {
            self.detour_timer = 0.0;
        }

        let prev_index = self.index;
        self.advance_past_reached(center, t.waypoint_eps);
        // Reaching a new node is progress — re-arm the one-shot stall repair.
        if self.index != prev_index {
            self.repair_attempted = false;
        }

        // Arm the post-step-aside pause once the bot has reached the cell it
        // retreated/strafed into; decelerate with mass/inertia before the timer
        // starts counting (handled in the `contact_wait_s` block above).
        if let Some((cell, secs)) = self.pending_wait {
            if reached_waypoint(center, cell, t.waypoint_eps) {
                self.step_aside_at = None;
                self.contact_wait_s = secs;
                self.pending_wait = None;
                self.drive(state, center, Vec2::ZERO, t.decel, dt);
                state.next_waypoint_hint = self.current_waypoint_hint();
                return;
            }
        }

        if self.index >= self.path.len() {
            // Exhausted: coast to a stop (the high-level re-plans next frame).
            self.drive(state, center, Vec2::ZERO, t.decel, dt);
            state.next_waypoint_hint = None;
            return;
        }

        let wp = self.path[self.index].center();
        let to_wp = wp - center;
        if to_wp.length_squared() > 1e-12 {
            self.direction = to_wp.normalize();
        }
        let dist_to_wp = to_wp.length();

        // Stuck detection: abandon the path when the bot is not in a charger queue
        // and has not moved more than `stuck_progress_eps` from its reference
        // position for `stuck_repath_secs` (near waypoints included).
        let in_queue = ctx.interactive.is_in_any_queue(ctx.entity);
        tick_stall_timer(
            &mut self.stuck_timer,
            &mut self.stuck_reference_pos,
            center,
            dt,
            t.stuck_progress_eps,
            in_queue,
        );

        if self.stuck_timer >= t.stuck_repath_secs {
            // A wall (static) wedge is treated differently from a bot wedge: it
            // never builds collision pressure, never marks the bot stuck, and is
            // **always** answered with a fresh subtile subpath toward the next
            // waypoint (re-armable), never the escape/abandon last resort.
            let wall_blocked = matches!(
                state.last_movement_error,
                Some(ActorMovementError::BlockedByStatic { .. })
            );

            // Improve recalculation: a *local* splice-repair — a footprint-aware
            // subtile detour around the obstacle to a node a bit further along,
            // spliced inline. For a bot stall it is one-shot (`repair_attempted`);
            // for a wall it re-arms every stall so the bot keeps re-routing.
            if !self.on_sub_run() && (wall_blocked || !self.repair_attempted) {
                let goal_node = (self.index + 1).min(self.path.len() - 1);
                if let Some(id) = self.enqueue_detour(state, ctx, goal_node) {
                    ctx.trace(format!(
                        "stuck {:.1}s ({}) → subtile re-route toward node {goal_node}",
                        self.stuck_timer,
                        if wall_blocked { "wall" } else { "bot" },
                    ));
                    if !wall_blocked {
                        self.repair_attempted = true;
                    }
                    self.detour_request = Some(id);
                    self.detour_purpose =
                        if wall_blocked { DetourPurpose::WallRepair } else { DetourPurpose::Repair };
                    self.detour_goal = goal_node;
                    self.detour_wait_elapsed = 0.0;
                    self.detour_fallback = None;
                    self.stuck_timer = 0.0;
                    self.stuck_reference_pos = Some(center);
                    self.drive(state, center, Vec2::ZERO, t.decel, dt);
                    state.next_waypoint_hint = self.current_waypoint_hint();
                    return;
                }
            }
            if wall_blocked {
                // No subtile re-route available (no avoidance / pathfind data):
                // a wall must still never abandon or mark the bot stuck. Reset and
                // keep steering toward the waypoint; the movement pipeline's
                // wall-slide carries the bot along the wall.
                self.stuck_timer = 0.0;
                self.stuck_reference_pos = Some(center);
            } else {
                self.escape_or_abandon(state, ctx, t, dt);
                return;
            }
        }

        // Braking profile: as we approach the waypoint, cap target speed to the
        // maximum speed that can stop within remaining distance (v^2 = 2 a d).
        // This prevents late, floaty overshoot and makes slowdown feel snappier.
        let brake_limited_speed = (2.0 * t.decel * dist_to_wp).sqrt();
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

    fn kind(&self) -> LowLevelKind {
        LowLevelKind::FollowPath
    }

    fn is_finished(&self) -> bool {
        self.abandoned || self.index >= self.path.len()
    }

    fn halt(&mut self) {
        self.velocity = Vec2::ZERO;
        self.prev_center = None;
        self.contact_wait_s = 0.0;
        self.pending_wait = None;
        self.escape_target = None;
        self.drop_sub_run();
        self.repair_attempted = false;
        self.clear_detour_request();
        self.last_head_on_bump = None;
    }

    fn label(&self) -> String {
        if self.escape_target.is_some() {
            "FollowPath (escaping)".to_string()
        } else if self.detour_request.is_some() {
            "FollowPath (awaiting detour)".to_string()
        } else if self.on_sub_run() {
            "FollowPath (detour)".to_string()
        } else {
            "FollowPath".to_string()
        }
    }

    fn route(&self) -> Option<(&[PathNode], usize)> {
        Some((&self.path, self.index))
    }

    fn velocity(&self) -> Vec2 {
        self.velocity
    }

    fn heading(&self) -> Vec2 {
        // Moving: the actual velocity direction. Stalled / braking (velocity ~0
        // against a blocker): the maintained unit `direction` toward the next
        // node — so a wedged bot still reports the way it is *trying* to go.
        if self.velocity.length_squared() > 1e-8 {
            self.velocity.normalize()
        } else {
            self.direction.normalize_or_zero()
        }
    }

    fn stuck_timer(&self) -> f32 {
        self.stuck_timer
    }

    fn is_stuck(&self) -> bool {
        // "Stuck" means we abandoned an unfinished route because progress stalled.
        self.abandoned && self.index < self.path.len()
    }

    fn target_tile(&self) -> Option<(i32, i32)> {
        // The final *cell* destination; trailing spliced subcells don't change it.
        self.path
            .iter()
            .rev()
            .find(|n| n.is_cell())
            .or_else(|| self.path.last())
            .map(|n| n.tile())
    }

    fn is_awaiting_path(&self) -> bool {
        // Braked awaiting an async detour, or paused after a step-aside — in
        // both holding states the bot is intentionally not following its route.
        self.detour_request.is_some() || self.contact_wait_s > 0.0
    }

    fn is_recovering(&self) -> bool {
        // Any active collision-recovery maneuver: detour (awaited or threading a
        // spliced sub-run), step-aside (pending move or post-step pause), or escape.
        self.detour_request.is_some()
            || self.on_sub_run()
            || self.contact_wait_s > 0.0
            || self.pending_wait.is_some()
            || self.step_aside_at.is_some()
            || self.escape_target.is_some()
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

/// `true` when `center` is within `eps` tiles of a path node's center (works for
/// both cell and subcell nodes).
#[inline]
fn node_reached(center: Vec2, node: PathNode, eps: f32) -> bool {
    (node.center() - center).length_squared() <= eps * eps
}

/// Advance a no-progress timer; reset when `center` moves past `progress_eps` or
/// when `disabled` (e.g. bot is legitimately waiting in a charger queue).
fn tick_stall_timer(
    timer: &mut f32,
    reference_pos: &mut Option<Vec2>,
    center: Vec2,
    dt: f32,
    progress_eps: f32,
    disabled: bool,
) {
    if disabled {
        *timer = 0.0;
        *reference_pos = Some(center);
        return;
    }
    if let Some(ref_pos) = *reference_pos {
        if (center - ref_pos).length() > progress_eps {
            *reference_pos = Some(center);
            *timer = 0.0;
        } else {
            *timer += dt;
        }
    } else {
        *reference_pos = Some(center);
        *timer = 0.0;
    }
}

/// Visits each `(dx, dy)` on the Chebyshev ring `ring` around the origin.
fn for_each_chebyshev_ring(ring: i32, mut f: impl FnMut(i32, i32)) {
    if ring == 0 {
        f(0, 0);
        return;
    }
    for dy in -ring..=ring {
        for dx in -ring..=ring {
            if dx.abs().max(dy.abs()) == ring {
                f(dx, dy);
            }
        }
    }
}

/// Minimum squared distance from `center` to any tile center on Chebyshev ring
/// `ring` around `here`. Used to stop the escape search once a free cell on an
/// inner ring is closer than every unsearched outer ring can be.
fn min_dist2_to_chebyshev_ring(center: Vec2, here: IVec2, ring: i32) -> f32 {
    let mut min_d2 = f32::MAX;
    for_each_chebyshev_ring(ring, |dx, dy| {
        let tile = (here.x + dx, here.y + dy);
        let d2 = (waypoint_center(tile) - center).length_squared();
        if d2 < min_d2 {
            min_d2 = d2;
        }
    });
    min_d2
}

/// Subtile coordinate that contains `pos` (floor of `pos * SUBTILE_COUNT`).
#[inline]
pub fn float_subtile(pos: Vec2) -> IVec2 {
    let sc = SUBTILE_COUNT as f32;
    IVec2::new((pos.x * sc).floor() as i32, (pos.y * sc).floor() as i32)
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
    fn chebyshev_ring_zero_is_anchor_only() {
        let mut count = 0;
        for_each_chebyshev_ring(0, |_, _| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn chebyshev_ring_one_is_eight_neighbors() {
        let mut count = 0;
        for_each_chebyshev_ring(1, |_, _| count += 1);
        assert_eq!(count, 8);
    }

    #[test]
    fn min_dist2_to_chebyshev_ring_picks_closest_tile_center() {
        let here = IVec2::ZERO;
        let center = Vec2::new(0.5, 0.5);
        let d2 = min_dist2_to_chebyshev_ring(center, here, 1);
        assert!(
            (d2 - 1.0).abs() < 1e-5,
            "axis neighbor center is 1 tile away, got {d2}"
        );
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
    fn follow_path_settles_on_final_waypoint_without_orbiting() {
        use crate::actor::brain::test_support::{ctx_with_charge, test_state};

        // Open space (no collision): integrate the emitted `tile_delta` straight
        // into the center each frame and confirm the bot reaches the goal tile
        // rather than circling it. Without arrival braking a lone bot orbits ~0.5
        // tiles out and only ever stops by abandoning the path (an orbit, not an
        // arrival), so we assert it lands *on* the goal.
        let goal = (5, 0);
        let mut fp = FollowPath::new(vec![(0, 0), goal]);
        let tuning = FollowTuning::default();
        let ctx = ctx_with_charge(1.0);
        let mut rng = rng::seeded(0);
        let mut state = test_state(); // center (0.5, 0.5)

        for _ in 0..1200 {
            fp.execute(&mut state, &ctx, &mut rng, &tuning);
            state.center += state.move_buffer.tile_delta;
            if fp.is_finished() {
                break;
            }
        }

        assert!(fp.is_finished(), "path never completed — bot is still orbiting");
        let miss = (state.center - waypoint_center(goal)).length();
        assert!(
            miss < tuning.waypoint_eps * 2.0,
            "settled {miss} tiles from the goal — that is an orbit, not an arrival",
        );
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
            heading: Vec2::ZERO,
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
            shadow: crate::actor::ActorShadow::default(),
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            radius_subtiles: 2,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            on_screen: true,
            trace: None,
            avoidance: None,
            patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
        };
        let mut rng = rng::seeded(1);
        fp.execute(&mut state, &ctx, &mut rng, &FollowTuning::default());

        assert!(
            fp.velocity.x < 0.0,
            "collision with blocker on +X should produce reflected X velocity"
        );
    }

    #[test]
    fn follow_path_heading_prefers_velocity_then_falls_back_to_direction() {
        let mut fp = FollowPath::new(vec![(8, 5)]);
        // Moving: heading is the (normalized) velocity direction.
        fp.velocity = Vec2::new(2.0, 0.0);
        fp.direction = Vec2::new(0.0, 1.0);
        assert!((fp.heading() - Vec2::X).length() < 1e-6, "moving → velocity dir");

        // Wedged: zero velocity, but still intends to head toward the next node.
        fp.velocity = Vec2::ZERO;
        assert!(
            (fp.heading() - Vec2::Y).length() < 1e-6,
            "stalled → maintained `direction` toward next node, not zero",
        );
    }

    #[test]
    fn track_tiles_remembers_previous_distinct_cell() {
        let mut fp = FollowPath::new(vec![(9, 5)]);
        fp.track_tiles(IVec2::new(4, 5));
        assert_eq!(fp.prev_tile, None, "first observation has no predecessor");
        fp.track_tiles(IVec2::new(4, 5));
        assert_eq!(fp.prev_tile, None, "staying in the same cell does not shift");
        fp.track_tiles(IVec2::new(5, 5));
        assert_eq!(fp.prev_tile, Some(IVec2::new(4, 5)), "crossing a boundary records the cell left");
        assert_eq!(fp.last_tile, Some(IVec2::new(5, 5)));
    }

    #[test]
    fn follow_path_steps_to_free_front_neighbor_on_bot_bump() {
        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);
        fp.prev_tile = Some(IVec2::new(4, 5));

        let mut state = ActorState {
            center: Vec2::new(5.0, 5.0),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
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
            shadow: crate::actor::ActorShadow::default(),
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            radius_subtiles: 2,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            on_screen: true,
            trace: None,
            avoidance: None,
            patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
        };
        let mut rng = rng::seeded(2);
        // No detour, so the bot steps aside. With every neighbour statically
        // walkable (avoidance None), it prefers a cell ahead of its +X heading.
        let tuning = FollowTuning {
            bot_detour_chance: 0.0,
            ..FollowTuning::default()
        };
        let original_len = fp.path.len();

        fp.execute(&mut state, &ctx, &mut rng, &tuning);

        assert!(
            fp.path.len() > original_len,
            "bot bump should insert a step-aside waypoint"
        );
        let stepped = fp.path[fp.index].tile();
        assert_eq!(stepped.0, 6, "front-preferred step is ahead (+X) of (5,5), got {stepped:?}");
        assert!((4..=6).contains(&stepped.1), "step stays an immediate neighbour, got {stepped:?}");
    }

    /// Bump context with a clear avoidance map and a valid step-back cell, so the
    /// chance roll is the only thing deciding detour vs. step-back.
    fn detour_or_stepback_fixture() -> (FollowPath, ActorState) {
        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);
        fp.prev_tile = Some(IVec2::new(4, 5)); // a valid step-back cell exists
        (fp, detour_state(IVec2::new(25, 25), IVec2::new(27, 25)))
    }

    #[test]
    fn pending_path_inherits_velocity_and_coasts() {
        use crate::actor::brain::test_support::{ctx_with_charge, test_state};

        let mut pp = PendingPath::with_velocity(Vec2::new(3.0, 0.0));
        let mut state = test_state();
        let ctx = ctx_with_charge(1.0);
        let tuning = FollowTuning::default();
        pp.execute(&mut state, &ctx, &mut rng::seeded(0), &tuning);
        assert!(
            state.move_buffer.tile_delta.x > 0.0,
            "PendingPath must keep moving from inherited velocity"
        );
        assert!(pp.velocity().x < 3.0, "PendingPath must decelerate under inertia");
    }

    #[test]
    fn follow_path_enqueues_subtile_detour_request_on_bot_bump() {
        use crate::actor::brain::AvoidanceViews;
        use crate::map::passability::{
            DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID,
        };
        use crate::map::pathfind_service::{PathfindQueue, PathfindResults};

        let dynamic = DynamicPassabilityMap::new();
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);
        let queue = PathfindQueue::default();
        let results = PathfindResults::default();
        let (mut fp, mut state) = detour_or_stepback_fixture();
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            radius_subtiles: 2,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            on_screen: true,
            trace: None,
            avoidance: Some(AvoidanceViews {
                dynamic: &dynamic,
                static_subtiles: &static_subtiles,
                blocked_flags: FLAG_BLOCKED | FLAG_VOID,
            }),
            patrol_loop: None,
            pathfind: Some(crate::actor::brain::PathfindAccess {
                queue: &queue,
                results: &results,
            }),
            fixer: None,
            dynamic_repath: false,
        };
        let mut rng = rng::seeded(5);
        let tuning = FollowTuning {
            bot_detour_chance: 1.0,
            ..FollowTuning::default()
        };

        fp.execute(&mut state, &ctx, &mut rng, &tuning);

        assert!(
            fp.detour_request.is_some(),
            "a full-chance bump must enqueue a subtile detour request"
        );
        let pending = queue.drain_pending();
        assert_eq!(pending.len(), 1);
        assert!(matches!(pending[0].1, PathKind::SubtileDetour { .. }));
        assert!(!fp.on_sub_run(), "detour subcells must not be spliced synchronously");
    }

    #[test]
    fn follow_path_steps_back_on_bot_bump_at_zero_chance() {
        use crate::actor::brain::AvoidanceViews;
        use crate::map::passability::{
            DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID,
        };

        let dynamic = DynamicPassabilityMap::new();
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);
        let (mut fp, mut state) = detour_or_stepback_fixture();
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            radius_subtiles: 2,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            on_screen: true,
            trace: None,
            avoidance: Some(AvoidanceViews {
                dynamic: &dynamic,
                static_subtiles: &static_subtiles,
                blocked_flags: FLAG_BLOCKED | FLAG_VOID,
            }),
            patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
        };
        let mut rng = rng::seeded(5);
        let original_len = fp.path.len();
        // chance 0.0: a bump must step aside (into a free neighbour), never detour.
        let tuning = FollowTuning {
            bot_detour_chance: 0.0,
            ..FollowTuning::default()
        };

        fp.execute(&mut state, &ctx, &mut rng, &tuning);

        assert!(!fp.on_sub_run(), "zero-chance bump must not detour");
        assert!(fp.path.len() > original_len, "zero-chance bump must step aside");
        let stepped = fp.path[fp.index].tile();
        let cur = (5, 5);
        assert!(
            (stepped.0 - cur.0).abs() <= 1 && (stepped.1 - cur.1).abs() <= 1 && stepped != cur,
            "step-aside targets an immediate free neighbour, got {stepped:?}"
        );
    }

    /// Builds a bump `BrainContext`/`ActorState` with the bot heading +X and the
    /// blocker at `blocker_subtile`. `avoidance` is `None`, so the only response
    /// available is a step (no detour) — convenient for asserting front/back gating.
    fn bump_ctx_state(
        blocker_subtile: IVec2,
    ) -> (ActorState, Hypermap<f32>, InteractiveEntityMap) {
        let state = ActorState {
            center: Vec2::new(5.0, 5.0),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: Some(ActorMovementError::BlockedByOccupancy {
                world_subtile_x: blocker_subtile.x,
                world_subtile_y: blocker_subtile.y,
            }),
            last_accepted_center_subtile: Some(IVec2::new(25, 25)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        (state, Hypermap::new(1.0), InteractiveEntityMap::new())
    }

    fn bump_ctx<'a>(
        state: &ActorState,
        passability: &'a Hypermap<f32>,
        interactive: &'a InteractiveEntityMap,
    ) -> BrainContext<'a> {
        BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            radius_subtiles: 2,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability,
            interactive,
            avoidance: None,
            on_screen: true,
            trace: None,
            patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
        }
    }

    #[test]
    fn follow_path_ignores_rear_bot_collision() {
        // Heading +X, blocker just behind on -X → a rear bump, which must be
        // ignored entirely: no bounce, no step, no detour — even with a valid
        // step-back cell available.
        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);
        fp.prev_tile = Some(IVec2::new(4, 5));

        let (mut state, passability, interactive) = bump_ctx_state(IVec2::new(24, 25));
        let ctx = bump_ctx(&state, &passability, &interactive);
        let mut rng = rng::seeded(7);
        let original_len = fp.path.len();

        fp.execute(&mut state, &ctx, &mut rng, &FollowTuning::default());

        assert_eq!(fp.path.len(), original_len, "rear bump must not insert a step");
        assert!(!fp.on_sub_run(), "rear bump must not detour");
        assert!(fp.velocity.x > 0.0, "rear bump must not reflect forward velocity");
    }

    #[test]
    fn follow_path_waits_in_place_when_fully_boxed_in() {
        // Front bump with every neighbour footprint blocked **and** no free cell
        // anywhere in the escape-search radius (fully boxed in) and no pathfind
        // handle → no step cell, no escape, no detour: the bot must just wait in
        // place (arm contact_wait_s, insert no waypoint).
        use crate::actor::brain::AvoidanceViews;
        use crate::map::passability::{DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED};

        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);
        fp.prev_tile = Some(IVec2::new(4, 5));

        let (mut state, passability, interactive) = bump_ctx_state(IVec2::new(26, 25));
        let dynamic = DynamicPassabilityMap::new();
        // Block every tile across the whole escape-search radius (and a margin) so
        // not just the neighbours but also `find_escape_cell` find nothing free.
        // NB: a *missing* chunk reads as 0 flags (not the map default), so the
        // blocked tiles must be written explicitly, not set as the default.
        let blocked_tile = SubtilePassability { cells: [FLAG_BLOCKED; SUBTILE_COUNT * SUBTILE_COUNT] };
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);
        for ty in 0..=10 {
            for tx in 0..=10 {
                static_subtiles.set(tx, ty, blocked_tile);
            }
        }
        let original_len = fp.path.len();
        let mut ctx = bump_ctx(&state, &passability, &interactive);
        ctx.avoidance = Some(AvoidanceViews {
            dynamic: &dynamic,
            static_subtiles: &static_subtiles,
            blocked_flags: FLAG_BLOCKED,
        });
        let mut rng = rng::seeded(8);
        let tuning = FollowTuning {
            bot_detour_chance: 0.0,
            ..FollowTuning::default()
        };

        // Every neighbour footprint is blocked → no free step cell.
        for (dx, dy) in NEIGHBOR_DIRS {
            assert!(
                !fp.neighbor_free(&state, &ctx, 5 + dx, 5 + dy),
                "neighbour ({},{}) must be blocked",
                5 + dx,
                5 + dy
            );
        }
        assert_eq!(
            fp.step_target(&state, &ctx, Vec2::new(1.0, 0.0), &mut rng),
            None,
            "all neighbour footprints blocked → step_target yields None"
        );

        // End-to-end: the bump then waits in place (no waypoint, no detour).
        fp.execute(&mut state, &ctx, &mut rng, &tuning);
        assert_eq!(fp.path.len(), original_len, "all-taken bump inserts no step waypoint");
        assert!(!fp.on_sub_run(), "no pathfind handle → no detour");
        assert!(
            fp.contact_wait_s > 0.0,
            "fully boxed in → bot waits in place, got {}",
            fp.contact_wait_s
        );
        assert!(fp.escape_target.is_none(), "no free cell anywhere → no escape");
    }

    #[test]
    fn follow_path_escapes_when_wedged_but_free_cell_beyond_neighbours() {
        // Front bump where every *immediate* neighbour footprint is blocked but a
        // free cell exists further out (open space with one big neighbour). Rather
        // than wait in place forever — which would suspend both the stuck-escape
        // and the collision-pressure relocate and deadlock two bots — the bot must
        // begin an escape to the nearest free cell.
        use crate::actor::brain::AvoidanceViews;
        use crate::map::passability::{DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED};

        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);
        fp.prev_tile = Some(IVec2::new(4, 5));

        let (mut state, passability, interactive) = bump_ctx_state(IVec2::new(26, 25));
        let dynamic = DynamicPassabilityMap::new();
        // Block only the 3×3 immediate-neighbour band (tiles 4..=6); everything
        // beyond stays free, so `step_target` yields None but `find_escape_cell`
        // finds a reachable free cell.
        let blocked_tile = SubtilePassability { cells: [FLAG_BLOCKED; SUBTILE_COUNT * SUBTILE_COUNT] };
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);
        for ty in 4..=6 {
            for tx in 4..=6 {
                static_subtiles.set(tx, ty, blocked_tile);
            }
        }
        let mut ctx = bump_ctx(&state, &passability, &interactive);
        ctx.avoidance = Some(AvoidanceViews {
            dynamic: &dynamic,
            static_subtiles: &static_subtiles,
            blocked_flags: FLAG_BLOCKED,
        });
        let mut rng = rng::seeded(8);
        let tuning = FollowTuning {
            bot_detour_chance: 0.0,
            ..FollowTuning::default()
        };

        assert_eq!(
            fp.step_target(&state, &ctx, Vec2::new(1.0, 0.0), &mut rng),
            None,
            "all immediate neighbours blocked → no step cell"
        );

        fp.execute(&mut state, &ctx, &mut rng, &tuning);
        assert!(
            fp.escape_target.is_some(),
            "wedged with a free cell beyond the neighbours → escape, not wait"
        );
        assert_eq!(fp.contact_wait_s, 0.0, "escaping, not waiting in place");
    }

    #[test]
    fn follow_path_waits_after_reaching_step_aside_cell() {
        // Front bump → step aside into a free neighbour with a pending pause. The
        // pause only starts once the bot actually reaches that cell, then it holds.
        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);
        fp.prev_tile = Some(IVec2::new(4, 5));

        let (mut state, passability, interactive) = bump_ctx_state(IVec2::new(26, 25));
        let mut rng = rng::seeded(9);
        let tuning = FollowTuning {
            bot_detour_chance: 0.0,
            ..FollowTuning::default()
        };

        // Bump frame: insert the step-aside cell, arm the pending pause (not started yet).
        let ctx = bump_ctx(&state, &passability, &interactive);
        fp.execute(&mut state, &ctx, &mut rng, &tuning);
        assert!(fp.contact_wait_s <= 0.0, "pause must not start before arrival");
        let cell = fp.path[fp.index].tile();

        // Arrive at the chosen step-aside cell: the pause arms but the bot coasts
        // down with decel before the timer runs.
        state.center = waypoint_center(cell);
        state.last_movement_error = None;
        let ctx = bump_ctx(&state, &passability, &interactive);
        fp.execute(&mut state, &ctx, &mut rng, &tuning);
        assert!(
            (STEP_BACK_WAIT_MIN_SECS..=STEP_BACK_WAIT_MAX_SECS).contains(&fp.contact_wait_s),
            "reaching the step-aside cell arms a 1-3s pause, got {}",
            fp.contact_wait_s
        );
        assert!(
            state.move_buffer.tile_delta.length_squared() > 1e-8,
            "arrival must not snap velocity to zero"
        );

        for _ in 0..80 {
            fp.execute(&mut state, &ctx, &mut rng, &tuning);
            state.center += state.move_buffer.tile_delta;
            if fp.velocity.length_squared() <= CONTACT_WAIT_STOP_SPEED * CONTACT_WAIT_STOP_SPEED {
                break;
            }
        }
        assert!(
            fp.velocity.length_squared() <= CONTACT_WAIT_STOP_SPEED * CONTACT_WAIT_STOP_SPEED,
            "pause phase must brake to a stop"
        );
        let wait_before = fp.contact_wait_s;
        fp.execute(&mut state, &ctx, &mut rng, &tuning);
        assert!(
            fp.contact_wait_s < wait_before,
            "hold timer must not run until the bot has settled"
        );
        assert_eq!(state.move_buffer.tile_delta, Vec2::ZERO, "bot holds position once stopped");
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
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(6, 2)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            radius_subtiles: 2,
            main_tile: IVec2::new(1, 0),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            on_screen: true,
            trace: None,
            avoidance: None,
            patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
        };
        let mut rng = rng::seeded(7);

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
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(2, 2)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let mut rng = rng::seeded(11);
        let tuning = FollowTuning {
            stuck_repath_secs: 0.3,
            ..FollowTuning::default()
        };

        for _ in 0..4 {
            let ctx = BrainContext {
                entity: Entity::PLACEHOLDER,
                dt: 0.1,
                center: state.center,
                radius_subtiles: 2,
                main_tile: IVec2::new(0, 0),
                main_tile_changed: false,
                floor: 0,
                charge: 1.0,
                missing_charge_pct: 0.0,
                depleted: false,
                broken: false,
                passability: &passability,
                interactive: &interactive,
                on_screen: true,
                trace: None,
                avoidance: None,
                patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
            };
            fp.execute(&mut state, &ctx, &mut rng, &tuning);
            // Simulate being physically pinned: position never changes.
            state.move_buffer = ActorMoveBuffer::default();
        }

        assert!(fp.is_stuck(), "no-progress route should mark low-level action as stuck");
        assert!(fp.is_finished(), "stuck route must request replanning");
    }

    #[test]
    fn follow_path_stuck_when_near_waypoint_has_no_progress() {
        let mut fp = FollowPath::new(vec![(1, 0)]);
        let mut state = ActorState {
            center: Vec2::new(0.55, 0.5),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(2, 2)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let mut rng = rng::seeded(19);
        let tuning = FollowTuning {
            stuck_repath_secs: 0.3,
            ..FollowTuning::default()
        };

        for _ in 0..4 {
            let ctx = BrainContext {
                entity: Entity::PLACEHOLDER,
                dt: 0.1,
                center: state.center,
                radius_subtiles: 2,
                main_tile: IVec2::new(0, 0),
                main_tile_changed: false,
                floor: 0,
                charge: 1.0,
                missing_charge_pct: 0.0,
                depleted: false,
                broken: false,
                passability: &passability,
                interactive: &interactive,
                on_screen: true,
                trace: None,
                avoidance: None,
                patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
            };
            fp.execute(&mut state, &ctx, &mut rng, &tuning);
            state.move_buffer = ActorMoveBuffer::default();
        }

        assert!(
            fp.is_stuck(),
            "waypoint within 1 tile must still trigger stuck when pinned"
        );
    }

    #[test]
    fn follow_path_relocates_to_free_cell_before_abandoning() {
        use crate::actor::brain::AvoidanceViews;
        use crate::map::passability::{
            DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID,
        };

        // Everything is free, so the nearest free cell is the bot's own tile (0,0).
        let dynamic = DynamicPassabilityMap::new();
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let tuning = FollowTuning {
            stuck_repath_secs: 0.3,
            ..FollowTuning::default()
        };

        // Start wedged off-center, heading for a far waypoint, and pinned so the
        // stall timer fires.
        let mut fp = FollowPath::new(vec![(10, 0)]);
        let mut state = ActorState {
            center: Vec2::new(0.2, 0.2),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(1, 1)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        let mut rng = rng::seeded(23);

        let make_ctx = |center: Vec2| BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center,
            radius_subtiles: 2,
            main_tile: IVec2::new(0, 0),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            on_screen: true,
            trace: None,
            avoidance: Some(AvoidanceViews {
                dynamic: &dynamic,
                static_subtiles: &static_subtiles,
                blocked_flags: FLAG_BLOCKED | FLAG_VOID,
            }),
            patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
        };

        for _ in 0..4 {
            let ctx = make_ctx(state.center);
            fp.execute(&mut state, &ctx, &mut rng, &tuning);
            state.move_buffer = ActorMoveBuffer::default();
        }

        assert_eq!(
            fp.escape_target,
            Some((0, 0)),
            "a stalled bot should target the nearest free cell instead of abandoning"
        );
        assert!(!fp.abandoned, "escape must not reschedule until the free cell is reached");
        assert!(!fp.is_stuck());

        // Arrive at the free cell center: now the route is abandoned for replanning.
        state.center = Vec2::new(0.5, 0.5);
        let ctx = make_ctx(state.center);
        fp.execute(&mut state, &ctx, &mut rng, &tuning);

        assert_eq!(fp.escape_target, None, "reaching the free cell ends the escape");
        assert!(fp.abandoned, "after relocating, the route is abandoned to replan");
        assert!(fp.is_stuck());
    }

    #[test]
    fn wait_retry_reports_stuck_after_no_progress() {
        let mut wait = Wait::retry(10.0);
        let mut state = ActorState {
            center: Vec2::new(3.2, 4.1),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(16, 20)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let tuning = FollowTuning {
            stuck_repath_secs: 0.25,
            ..FollowTuning::default()
        };

        for _ in 0..4 {
            let ctx = BrainContext {
                entity: Entity::PLACEHOLDER,
                dt: 0.1,
                center: state.center,
                radius_subtiles: 2,
                main_tile: IVec2::new(3, 4),
                main_tile_changed: false,
                floor: 0,
                charge: 1.0,
                missing_charge_pct: 0.0,
                depleted: false,
                broken: false,
                passability: &passability,
                interactive: &interactive,
                on_screen: true,
                trace: None,
                avoidance: None,
                patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
            };
            wait.execute(&mut state, &ctx, &mut rng::seeded(1), &tuning);
        }

        assert!(wait.is_stuck());
        assert!(wait.is_finished());
        assert!(wait.remaining_s > 0.0, "stall should pre-empt the retry timer");
    }

    fn detour_state(center_subtile: IVec2, blocker_subtile: IVec2) -> ActorState {
        ActorState {
            center: Vec2::new(5.0, 5.0),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
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
            shadow: crate::actor::ActorShadow::default(),
        }
    }

    #[test]
    fn install_detour_splices_subcell_nodes_at_cursor() {
        // A bending subtile staircase (corner at (25,30)) splices `Sub` nodes into
        // the unified path *before* the current cell, so the cursor now threads
        // subcells and then arrives at the preserved coarse cell.
        let mut fp = FollowPath::new(vec![(5, 5), (8, 5)]);
        fp.index = 1; // next coarse cell is (8, 5)
        let subs = vec![IVec2::new(25, 25), IVec2::new(25, 30), IVec2::new(40, 30)];

        let before = fp.path.len();
        fp.install_detour(&subs);

        assert!(fp.path.len() > before, "detour subcells must be spliced in");
        assert!(fp.on_sub_run(), "cursor now points at a spliced Sub node");
        assert!(matches!(fp.path[fp.index], PathNode::Sub(_)));
        assert!(
            fp.path.iter().any(|n| *n == PathNode::cell(8, 5)),
            "the coarse cell after the detour is preserved"
        );
    }

    #[test]
    fn install_detour_repair_replaces_wedged_cell() {
        // A stall repair targets the node *after* the wedged cell. Installing it
        // must replace `index..goal` — dropping the wedged cell — so the bot
        // doesn't thread the detour and then U-turn back into the obstacle.
        let mut fp = FollowPath::new(vec![(5, 5), (6, 5), (9, 5)]);
        fp.index = 1; // wedged trying to reach (6, 5)
        fp.detour_goal = 2; // route around it to (9, 5)
        let subs = vec![IVec2::new(27, 25), IVec2::new(27, 30), IVec2::new(47, 30)];

        fp.install_detour(&subs);

        assert!(
            !fp.path.contains(&PathNode::cell(6, 5)),
            "the wedged cell must be dropped, not left behind the detour"
        );
        assert!(
            fp.path.contains(&PathNode::cell(9, 5)),
            "the repair goal cell is preserved"
        );
        assert!(matches!(fp.path[fp.index], PathNode::Sub(_)), "cursor now threads subcells");
    }

    #[test]
    fn stall_attempts_local_repair_before_escape() {
        // A pinned bot with avoidance + a pathfind handle must first enqueue a
        // local splice-repair `SubtileDetour` (purpose Repair) — not jump straight
        // to relocate/abandon.
        use crate::actor::brain::{AvoidanceViews, PathfindAccess};
        use crate::map::passability::{
            DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID,
        };
        use crate::map::pathfind_service::{PathfindQueue, PathfindResults};

        let dynamic = DynamicPassabilityMap::new();
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);
        let queue = PathfindQueue::default();
        let results = PathfindResults::default();
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let tuning = FollowTuning {
            stuck_repath_secs: 0.3,
            ..FollowTuning::default()
        };

        let mut fp = FollowPath::new(vec![(10, 0)]);
        let mut state = ActorState {
            center: Vec2::new(0.2, 0.2),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(1, 1)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        let mut rng = rng::seeded(31);

        let make_ctx = |center: Vec2| BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center,
            radius_subtiles: 2,
            main_tile: IVec2::new(0, 0),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            on_screen: true,
            trace: None,
            avoidance: Some(AvoidanceViews {
                dynamic: &dynamic,
                static_subtiles: &static_subtiles,
                blocked_flags: FLAG_BLOCKED | FLAG_VOID,
            }),
            patrol_loop: None,
            pathfind: Some(PathfindAccess { queue: &queue, results: &results }),
            fixer: None,
            dynamic_repath: false,
        };

        for _ in 0..4 {
            let ctx = make_ctx(state.center);
            fp.execute(&mut state, &ctx, &mut rng, &tuning);
            state.move_buffer = ActorMoveBuffer::default();
        }

        assert!(fp.detour_request.is_some(), "stall must enqueue a local splice-repair first");
        assert_eq!(fp.detour_purpose, DetourPurpose::Repair);
        assert!(!fp.abandoned, "repair must be tried before abandoning");
        assert!(fp.escape_target.is_none(), "repair is tried before the escape relocate");
        let pending = queue.drain_pending();
        assert!(
            pending.iter().any(|(_, k)| matches!(k, PathKind::SubtileDetour { .. })),
            "repair enqueues a subtile detour search"
        );
    }

    #[test]
    fn wall_stall_reroutes_with_subtiles_and_never_abandons() {
        // A bot wedged against a **wall** (BlockedByStatic) must re-route with a
        // subtile subpath toward the next waypoint and **never** abandon / mark
        // itself stuck — unlike a bot-on-bot wedge, which can escape/abandon.
        use crate::actor::brain::{AvoidanceViews, PathfindAccess};
        use crate::map::passability::{
            DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID,
        };
        use crate::map::pathfind_service::{PathfindQueue, PathfindResults};

        let dynamic = DynamicPassabilityMap::new();
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);
        let queue = PathfindQueue::default();
        let results = PathfindResults::default();
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let tuning = FollowTuning {
            stuck_repath_secs: 0.3,
            ..FollowTuning::default()
        };

        let mut fp = FollowPath::new(vec![(10, 0)]);
        let mut state = ActorState {
            center: Vec2::new(0.2, 0.2),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(1, 1)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        let mut rng = rng::seeded(31);

        let make_ctx = |center: Vec2| BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center,
            radius_subtiles: 2,
            main_tile: IVec2::new(0, 0),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            on_screen: true,
            trace: None,
            avoidance: Some(AvoidanceViews {
                dynamic: &dynamic,
                static_subtiles: &static_subtiles,
                blocked_flags: FLAG_BLOCKED | FLAG_VOID,
            }),
            patrol_loop: None,
            pathfind: Some(PathfindAccess { queue: &queue, results: &results }),
            fixer: None,
            dynamic_repath: false,
        };

        for _ in 0..4 {
            // The movement pipeline reports a wall block every tick.
            state.last_movement_error = Some(ActorMovementError::BlockedByStatic {
                world_subtile_x: 10,
                world_subtile_y: 1,
            });
            let ctx = make_ctx(state.center);
            fp.execute(&mut state, &ctx, &mut rng, &tuning);
            state.move_buffer = ActorMoveBuffer::default();
        }

        assert!(fp.detour_request.is_some(), "wall stall must enqueue a subtile re-route");
        assert_eq!(
            fp.detour_purpose,
            DetourPurpose::WallRepair,
            "a wall wedge re-routes with the wall purpose, not the escape/abandon Repair"
        );
        assert!(!fp.abandoned, "a wall must never abandon");
        assert!(!fp.is_stuck(), "a wall must never mark the bot stuck");
        assert!(fp.escape_target.is_none(), "a wall never triggers the escape relocate");
    }

    #[test]
    fn lookahead_is_footprint_aware_not_single_cell() {
        // A creature offset to the **side** of the heading — within the bot's
        // radius of its leading footprint, but not on the center axis — must
        // trigger a look-ahead detour. The old single-cell probe (straight ahead)
        // would have missed it; a size-aware footprint probe catches it.
        use crate::actor::brain::{AvoidanceViews, PathfindAccess};
        use crate::map::passability::{
            DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID,
        };
        use crate::map::pathfind_service::{PathfindQueue, PathfindResults};

        let dynamic = DynamicPassabilityMap::new();
        // Creature in the leading footprint after advancing +X by one subtile
        // (advanced center (26,25), radius 2): (26,27) is in that circle but NOT
        // in the bot's current footprint at (25,25), and is off the +X center
        // axis the old single-cell probe checked.
        dynamic.write_footprint(&[IVec2::new(26, 27)]);
        dynamic.flush();
        let static_subtiles: Hypermap<SubtilePassability> = Hypermap::new(SubtilePassability::EMPTY);
        let queue = PathfindQueue::default();
        let results = PathfindResults::default();
        let passability = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();

        let mut fp = FollowPath::new(vec![(8, 5)]);
        fp.velocity = Vec2::new(1.0, 0.0);
        fp.direction = Vec2::new(1.0, 0.0);
        let mut state = ActorState {
            center: Vec2::new(5.0, 5.0),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: Some(IVec2::new(25, 25)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        let ctx = BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 0.1,
            center: state.center,
            radius_subtiles: 2,
            main_tile: IVec2::new(5, 5),
            main_tile_changed: false,
            floor: 0,
            charge: 1.0,
            missing_charge_pct: 0.0,
            depleted: false,
            broken: false,
            passability: &passability,
            interactive: &interactive,
            on_screen: true,
            trace: None,
            avoidance: Some(AvoidanceViews {
                dynamic: &dynamic,
                static_subtiles: &static_subtiles,
                blocked_flags: FLAG_BLOCKED | FLAG_VOID,
            }),
            patrol_loop: None,
            pathfind: Some(PathfindAccess { queue: &queue, results: &results }),
            fixer: None,
            dynamic_repath: false,
        };
        let mut rng = rng::seeded(41);

        fp.execute(&mut state, &ctx, &mut rng, &FollowTuning::default());

        assert!(
            fp.detour_request.is_some(),
            "an off-axis creature inside the leading footprint must trigger a look-ahead detour"
        );
        assert_eq!(fp.detour_purpose, DetourPurpose::Avoidance);
        let pending = queue.drain_pending();
        assert!(pending.iter().any(|(_, k)| matches!(k, PathKind::SubtileDetour { .. })));
    }
}

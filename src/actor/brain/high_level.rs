//! High-level actions — the single exclusive task a bot is pursuing.
//!
//! The brain selects one high-level action from the dominant
//! [`Priority`](super::Priority) each tick; that action [`update`](HighLevelAction::update)s
//! the bot's low-level action (`Wait` / `FollowPath`) and may request side
//! effects ([`BrainEffects`]). When an action reports
//! [`HighLevelStatus::Done`] the brain drops it and re-plans next tick.

use bevy::ecs::entity::Entity;
use bevy::math::{IVec2, Vec2};
use crate::rng::{self, StdRng};

use crate::actor::black_bot::nearest_reachable_depot;
use crate::actor::brain::memory::IntegerMemoryId;
use crate::actor::dispatch::{RepairPart, RepairRequest, FIXER_TASK_COOLDOWN_S};
use crate::map::hypermap::{world_to_chunk_local, ChunkCoord, Hypermap, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_pathfind::{
    astar_shortest_world_path, manhattan, world_tile_walkable, HypermapPathResult,
    HypermapSearchLimits,
};
use crate::map::interactive_entity::{EntityCoordinates, InteractiveEntityMap};
use crate::map::pathfind_service::{PathKind, PathOutcome, PathfindReason, RequestId};

use super::low_level::{FollowPath, LowLevelAction, LowLevelKind, PendingPath, Wait};
use super::priority::PriorityKind;
use super::{BrainContext, BrainEffects, BrainLogEvent, FixerContext, PathfindAccess};
use super::MAX_REMEMBER_UNREACHABLE_PER_TICK;

/// Wander radius (tiles) for [`GoToRandomPoints`].
const WANDER_RADIUS: f32 = 15.0;
/// Random-target sampling attempts before giving up for this tick.
const MAX_TARGET_ATTEMPTS: u32 = 8;
/// Tiles kept on each side of a bend during path simplification.
const PATH_CORNER_BUFFER: usize = 1;
/// Retry delay when no wander target / charger could be found.
const RETRY_S: f32 = 0.25;
/// A* expansion cap for a wander / patrol-leg world route.
const WANDER_SEARCH_LIMIT: usize = 2000;
/// Seconds a bot waits on a queued route request before reissuing it (the
/// "waiting-for-path" timeout from the async pathfinding design).
const PATH_WAIT_RETRY_S: f32 = 3.0;
/// Seconds allowed per tile of initial Manhattan distance while following a
/// wander or patrol leg before the high level abandons it.
const LEG_TIMEOUT_PER_TILE_S: f32 = 3.0;

/// `true` on the first frame the low-level action reports stuck or finished
/// since the previous tick it did not (rising edge). Prevents re-running
/// expensive A\* / charger scans every frame while `Wait::retry` stays stalled.
fn low_level_needs_replan(low: &dyn LowLevelAction, prev_stuck: bool, prev_finished: bool) -> bool {
    let stuck = low.is_stuck();
    let finished = low.is_finished();
    (stuck && !prev_stuck) || (finished && !prev_finished)
}

fn leg_timeout_secs(start: (i32, i32), goal: (i32, i32)) -> f32 {
    manhattan(start, goal) as f32 * LEG_TIMEOUT_PER_TILE_S
}

fn is_follow_path(low: &dyn LowLevelAction) -> bool {
    low.kind() == LowLevelKind::FollowPath
}

/// Travel budget for the current wander/patrol leg (Manhattan start→goal × 3 s).
#[derive(Clone, Copy, Debug)]
struct LegDeadline {
    goal: (i32, i32),
    timeout_s: f32,
    elapsed_s: f32,
}

impl LegDeadline {
    fn new(start: (i32, i32), goal: (i32, i32)) -> Self {
        Self {
            goal,
            timeout_s: leg_timeout_secs(start, goal),
            elapsed_s: 0.0,
        }
    }

    fn reset_elapsed(&mut self) {
        self.elapsed_s = 0.0;
    }

    /// Advances the timer; returns `true` when the leg has exceeded its budget.
    fn tick(&mut self, dt: f32) -> bool {
        self.elapsed_s += dt;
        self.elapsed_s >= self.timeout_s
    }
}
/// Number of waypoints in a freshly generated [`GoToPatrol`] loop.
const PATROL_LOOP_LEN: usize = 5;
/// Radius (tiles) within which patrol-loop waypoints are sampled around the anchor.
const PATROL_RADIUS: f32 = 12.0;
/// Sampling attempts before accepting a (possibly shorter) patrol loop.
const PATROL_GEN_ATTEMPTS: u32 = 64;
/// Maximum distinct candidate tiles to enqueue per patrol-loop generation pass.
/// Twice the loop length leaves headroom for unreachable candidates without
/// flooding the pathfind queue.
const PATROL_CANDIDATES: usize = PATROL_LOOP_LEN * 2;
/// A* expansion cap for charger routes.
const SEARCH_LIMIT: usize = 5000;

/// Charge gained per second while docked (infinite station — charger stored
/// energy is intentionally ignored).
pub const RECHARGE_PER_S: f32 = 0.05;
/// Charge level treated as "full" (undock threshold).
const CHARGE_FULL: f32 = 0.999;
/// Retry delay while seeking a charger that isn't currently reachable/free.
const CHARGE_RETRY_S: f32 = 0.5;
/// Enter waiting queue once Manhattan distance to the station is < 5.
const WAITING_QUEUE_ENTER_DISTANCE: i32 = 4;
/// Random backoff while holding a waiting-queue slot near a station.
const WAITING_RECHECK_MIN_S: f32 = 0.1;
const WAITING_RECHECK_MAX_S: f32 = 0.4;

/// Maximum charger candidates to route-test in one seek pass. Capped to avoid
/// flooding the pathfind queue (one A* per candidate) when many chargers are
/// nearby — pick the N closest by Manhattan distance.
const MAX_CHARGER_CANDIDATES: usize = 5;

/// While already committed to a charger (Traveling / WaitingQueue), bail to a
/// different station once **more than** this many *other* bots are waiting at it.
/// Higher than the `< 2` selection preference so a bot doesn't bounce off a
/// charger the instant a second bot arrives — only a genuinely crowded queue
/// (3+ others) forces a reroute.
const CHARGER_REROUTE_WAITING_LIMIT: usize = 2;

/// Result of a [`HighLevelAction::update`].
pub enum HighLevelStatus {
    Running,
    Done,
}

pub struct HighLevelOutcome {
    pub status: HighLevelStatus,
    pub effects: BrainEffects,
}

impl HighLevelOutcome {
    fn running() -> Self {
        Self { status: HighLevelStatus::Running, effects: BrainEffects::default() }
    }
    fn running_with(effects: BrainEffects) -> Self {
        Self { status: HighLevelStatus::Running, effects }
    }
    fn done(effects: BrainEffects) -> Self {
        Self { status: HighLevelStatus::Done, effects }
    }
}

/// A bot's single, exclusive high-level task.
pub trait HighLevelAction: Send + Sync {
    /// Which priority kind this action serves (used by the brain to decide when
    /// a different wish should pre-empt it).
    fn kind(&self) -> PriorityKind;

    /// Short label for the inspector.
    fn label(&self) -> String;

    /// Advance the plan: set/replace the low-level action and request effects.
    fn update(
        &mut self,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
    ) -> HighLevelOutcome;

    /// Release world side effects when this action is dropped without a normal
    /// [`HighLevelStatus::Done`] (priority pre-emption, plan cleared, etc.).
    fn preempt(&mut self, _ctx: &BrainContext) -> BrainEffects {
        BrainEffects::default()
    }

    /// Station-queue coordinates this action is currently holding a slot in, if
    /// any. Returned every tick so the liveness watchdog
    /// ([`InteractiveEntityMap::refresh_queue`]) knows the bot is still pursuing
    /// it. `None` (the default) means "not waiting on any station queue".
    fn active_queue(&self) -> Option<EntityCoordinates> {
        None
    }
}

/// Default mapping from a priority kind to the action that serves it. A brain
/// may supply a different factory, but this covers BlackBot.
pub fn make_high_level(kind: PriorityKind) -> Box<dyn HighLevelAction> {
    match kind {
        PriorityKind::RandomWalking => Box::new(GoToRandomPoints::default()),
        PriorityKind::Patrolling => Box::new(GoToPatrol::new()),
        PriorityKind::Fixing => Box::new(GoFixBots::new()),
        PriorityKind::Cleaning => Box::new(GoClean::default()),
        PriorityKind::RechargeYourself => Box::new(GoToChargeStation::new()),
    }
}

// ---------------------------------------------------------------------------
// GoToRandomPoints
// ---------------------------------------------------------------------------

/// Perpetual wander: whenever the current path finishes, pick a new random
/// reachable target and follow it. Never reports `Done`.
///
/// Routing is asynchronous: the action samples a goal, enqueues a `WorldRoute`,
/// and parks the bot in a [`PendingPath`] hold until the result lands (or the
/// 3 s retry fires), then installs a [`FollowPath`].
#[derive(Default)]
pub struct GoToRandomPoints {
    /// In-flight route request id while awaiting a path.
    awaiting: Option<RequestId>,
    /// Seconds awaited so far (drives the [`PATH_WAIT_RETRY_S`] reissue).
    await_elapsed: f32,
    /// Active leg travel budget while [`FollowPath`] is driving toward a goal.
    leg: Option<LegDeadline>,
    prev_low_stuck: bool,
    prev_low_finished: bool,
}

impl GoToRandomPoints {
    /// Samples a fresh wander goal, enqueues a route request, and parks the bot.
    /// Falls back to a short retry `Wait` when no candidate / queue is available.
    fn request_target(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
        reason: PathfindReason,
    ) {
        let here = (ctx.main_tile.x, ctx.main_tile.y);
        match sample_wander_goal(rng, here, ctx.passability) {
            Some(goal) => {
                let id = pf.queue.enqueue(PathKind::WorldRoute {
                    start: here,
                    goal,
                    max_expanded: WANDER_SEARCH_LIMIT,
                    simplify_buffer: PATH_CORNER_BUFFER,
                    include_dynamic: ctx.dynamic_repath,
                    radius: ctx.radius_subtiles,
                }, ctx.entity, reason);
                self.awaiting = Some(id);
                self.await_elapsed = 0.0;
                self.leg = Some(LegDeadline::new(here, goal));
                *low = Box::new(PendingPath::with_velocity(low.velocity()));
            }
            None => {
                self.awaiting = None;
                self.leg = None;
                *low = Box::new(Wait::retry(RETRY_S));
            }
        }
    }
}

impl HighLevelAction for GoToRandomPoints {
    fn kind(&self) -> PriorityKind {
        PriorityKind::RandomWalking
    }
    fn label(&self) -> String {
        "GoToRandomPoints".to_string()
    }
    fn update(
        &mut self,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
    ) -> HighLevelOutcome {
        let Some(pf) = ctx.pathfind else {
            return HighLevelOutcome::running();
        };

        if let Some(id) = self.awaiting {
            self.await_elapsed += ctx.dt;
            if let Some(outcome) = pf.results.take(id) {
                self.awaiting = None;
                match outcome {
                    PathOutcome::Route { path, raw_len } if raw_len > 1 => {
                        if let Some(leg) = &mut self.leg {
                            leg.reset_elapsed();
                        }
                        *low = Box::new(FollowPath::new(path));
                    }
                    _ => self.request_target(pf, ctx, low, rng, PathfindReason::WanderPathFailed),
                }
            } else if self.await_elapsed >= PATH_WAIT_RETRY_S {
                self.request_target(pf, ctx, low, rng, PathfindReason::WanderRetry);
            }
            self.prev_low_stuck = low.is_stuck();
            self.prev_low_finished = low.is_finished();
            return HighLevelOutcome::running();
        }

        if let Some(leg) = &mut self.leg {
            if is_follow_path(low.as_ref()) && !low.is_finished() && leg.tick(ctx.dt) {
                let goal = leg.goal;
                self.leg = None;
                low.halt();
                self.request_target(pf, ctx, low, rng, PathfindReason::WanderLegTimedOut);
                self.prev_low_stuck = low.is_stuck();
                self.prev_low_finished = low.is_finished();
                return HighLevelOutcome::running_with(BrainEffects {
                    log: Some(BrainLogEvent::WanderDestinationTimedOut { goal }),
                    ..BrainEffects::default()
                });
            }
        }

        if low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished) {
            self.leg = None;
            self.request_target(pf, ctx, low, rng, PathfindReason::WanderNewGoal);
        }
        self.prev_low_stuck = low.is_stuck();
        self.prev_low_finished = low.is_finished();
        HighLevelOutcome::running()
    }
}

// ---------------------------------------------------------------------------
// GoClean
// ---------------------------------------------------------------------------

/// Half-width (tiles) of the square a cleaner scans for the dirtiest cell.
const CLEAN_SCAN_RADIUS: i32 = 6;
/// Minimum dirt (`0.0..=1.0`) a cell must carry to be worth cleaning. Below this
/// the cleaner relocates instead of scrubbing.
const CLEAN_THRESHOLD: f32 = 0.3;
/// Speed multiplier while driving a cleaning leg (−50% speed).
const CLEAN_SPEED_SCALE: f32 = 0.5;
/// Inclusive ring (tiles) a cleaner relocates into when its surroundings are clean.
const RELOCATE_MIN_TILES: f32 = 10.0;
const RELOCATE_MAX_TILES: f32 = 30.0;

/// Picks a random *walkable* tile in the annulus `min..=max` (Euclidean, tiles)
/// around `center`, or `None` if none was found this tick. Used by a cleaner to
/// relocate to a fresh area to scan.
pub fn sample_ring_goal(
    rng: &mut StdRng,
    center: (i32, i32),
    min_radius: f32,
    max_radius: f32,
    passability: &Hypermap<f32>,
) -> Option<(i32, i32)> {
    for _ in 0..MAX_TARGET_ATTEMPTS {
        let dx: f32 = rng::range(rng, -max_radius..max_radius);
        let dy: f32 = rng::range(rng, -max_radius..max_radius);
        let d2 = dx * dx + dy * dy;
        if d2 > max_radius * max_radius || d2 < min_radius * min_radius {
            continue;
        }
        let target = (center.0 + dx.round() as i32, center.1 + dy.round() as i32);
        if target == center {
            continue;
        }
        if world_tile_walkable(passability, target.0, target.1) {
            return Some(target);
        }
    }
    None
}

/// Finds the dirtiest walkable tile within [`CLEAN_SCAN_RADIUS`] of `here` whose
/// dirt exceeds [`CLEAN_THRESHOLD`]. The current tile is excluded (a cleaning leg
/// scrubs the cell the bot already stands on). `None` when the dirt field is
/// unavailable or nothing nearby is dirty enough.
fn dirtiest_cell(ctx: &BrainContext, here: (i32, i32)) -> Option<(i32, i32)> {
    let dirt = ctx.dirt?;
    let mut best: Option<((i32, i32), f32)> = None;
    for dy in -CLEAN_SCAN_RADIUS..=CLEAN_SCAN_RADIUS {
        for dx in -CLEAN_SCAN_RADIUS..=CLEAN_SCAN_RADIUS {
            let tile = (here.0 + dx, here.1 + dy);
            if tile == here || !world_tile_walkable(ctx.passability, tile.0, tile.1) {
                continue;
            }
            let value = dirt.get(tile.0, tile.1);
            if value <= CLEAN_THRESHOLD {
                continue;
            }
            if best.map_or(true, |(_, b)| value > b) {
                best = Some((tile, value));
            }
        }
    }
    best.map(|(tile, _)| tile)
}

/// Perpetual cleaning duty: scan a [`CLEAN_SCAN_RADIUS`]-tile window for the
/// dirtiest cell; if one exceeds [`CLEAN_THRESHOLD`], crawl to it at half speed
/// ([`CLEAN_SPEED_SCALE`]) in **cleaning mode** (the floor it crosses is zeroed by
/// [`dirt_actor_interaction`](crate::map::field_interactions) and it glows teal);
/// otherwise relocate [`RELOCATE_MIN_TILES`]–[`RELOCATE_MAX_TILES`] tiles away and
/// scan again. Routing is asynchronous, mirroring [`GoToRandomPoints`]. Never
/// reports `Done`.
#[derive(Default)]
pub struct GoClean {
    /// In-flight route request id while awaiting a path.
    awaiting: Option<RequestId>,
    await_elapsed: f32,
    /// `true` when the current/awaited leg heads to a dirty cell (cleaning leg);
    /// `false` for a relocation leg.
    cleaning_leg: bool,
    /// Active leg travel budget while [`FollowPath`] is driving.
    leg: Option<LegDeadline>,
    prev_low_stuck: bool,
    prev_low_finished: bool,
}

impl GoClean {
    /// Scans for the dirtiest nearby cell and routes to it (cleaning leg); if the
    /// surroundings are clean, relocates to a fresh area; falls back to a short
    /// retry `Wait` when nothing can be sampled this tick.
    fn scan_and_request(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
        reason: PathfindReason,
    ) {
        let here = (ctx.main_tile.x, ctx.main_tile.y);
        let (goal, cleaning) = match dirtiest_cell(ctx, here) {
            Some(dirty) => (Some(dirty), true),
            None => (
                sample_ring_goal(rng, here, RELOCATE_MIN_TILES, RELOCATE_MAX_TILES, ctx.passability),
                false,
            ),
        };
        match goal {
            Some(goal) => {
                let id = pf.queue.enqueue(PathKind::WorldRoute {
                    start: here,
                    goal,
                    max_expanded: WANDER_SEARCH_LIMIT,
                    simplify_buffer: PATH_CORNER_BUFFER,
                    include_dynamic: ctx.dynamic_repath,
                    radius: ctx.radius_subtiles,
                }, ctx.entity, reason);
                self.awaiting = Some(id);
                self.await_elapsed = 0.0;
                self.cleaning_leg = cleaning;
                self.leg = Some(LegDeadline::new(here, goal));
                *low = Box::new(PendingPath::with_velocity(low.velocity()));
            }
            None => {
                self.awaiting = None;
                self.leg = None;
                self.cleaning_leg = false;
                *low = Box::new(Wait::retry(RETRY_S));
            }
        }
    }

    /// `true` while the bot is actively driving a cleaning leg (so the owning
    /// system scrubs the floor and lights the teal glow this tick).
    fn cleaning_now(&self, low: &dyn LowLevelAction) -> bool {
        self.cleaning_leg && is_follow_path(low) && !low.is_finished()
    }

    fn running(&self, low: &dyn LowLevelAction) -> HighLevelOutcome {
        HighLevelOutcome::running_with(BrainEffects {
            set_cleaning: Some(self.cleaning_now(low)),
            ..BrainEffects::default()
        })
    }
}

impl HighLevelAction for GoClean {
    fn kind(&self) -> PriorityKind {
        PriorityKind::Cleaning
    }
    fn label(&self) -> String {
        "GoClean".to_string()
    }
    fn preempt(&mut self, _ctx: &BrainContext) -> BrainEffects {
        // Leaving cleaning duty (e.g. for a recharge): drop cleaning mode so the
        // bot stops scrubbing / glowing.
        BrainEffects { set_cleaning: Some(false), ..BrainEffects::default() }
    }
    fn update(
        &mut self,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
    ) -> HighLevelOutcome {
        let Some(pf) = ctx.pathfind else {
            return HighLevelOutcome::running();
        };

        if let Some(id) = self.awaiting {
            self.await_elapsed += ctx.dt;
            if let Some(outcome) = pf.results.take(id) {
                self.awaiting = None;
                match outcome {
                    PathOutcome::Route { path, raw_len } if raw_len > 1 => {
                        if let Some(leg) = &mut self.leg {
                            leg.reset_elapsed();
                        }
                        let follow = FollowPath::new(path);
                        *low = Box::new(if self.cleaning_leg {
                            follow.with_speed_scale(CLEAN_SPEED_SCALE)
                        } else {
                            follow
                        });
                    }
                    _ => self.scan_and_request(pf, ctx, low, rng, PathfindReason::WanderPathFailed),
                }
            } else if self.await_elapsed >= PATH_WAIT_RETRY_S {
                self.scan_and_request(pf, ctx, low, rng, PathfindReason::WanderRetry);
            }
            self.prev_low_stuck = low.is_stuck();
            self.prev_low_finished = low.is_finished();
            return self.running(low.as_ref());
        }

        if let Some(leg) = &mut self.leg {
            if is_follow_path(low.as_ref()) && !low.is_finished() && leg.tick(ctx.dt) {
                self.leg = None;
                low.halt();
                self.scan_and_request(pf, ctx, low, rng, PathfindReason::WanderLegTimedOut);
                self.prev_low_stuck = low.is_stuck();
                self.prev_low_finished = low.is_finished();
                return self.running(low.as_ref());
            }
        }

        if low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished) {
            self.leg = None;
            self.scan_and_request(pf, ctx, low, rng, PathfindReason::WanderNewGoal);
        }
        self.prev_low_stuck = low.is_stuck();
        self.prev_low_finished = low.is_finished();
        self.running(low.as_ref())
    }
}

// ---------------------------------------------------------------------------
// GoToPatrol
// ---------------------------------------------------------------------------

/// Perpetual patrol of a *fixed* loop of cells — the [`Patrol`] route stored on
/// the entity and surfaced through [`BrainContext::patrol_loop`]. The bot walks
/// the loop in order forever.
///
/// The action itself is transient (the brain rebuilds it whenever `Patrolling`
/// becomes dominant again, e.g. after a recharge pre-emption), but the loop is
/// not: it lives on the [`Patrol`] component. On (re)creation the action snaps
/// its [`cursor`](Self::cursor) to the loop waypoint nearest the bot, so it
/// "gets back from where it stopped" after a recharge detour. Never reports
/// `Done`.
///
/// [`Patrol`]: crate::actor::black_bot::Patrol
pub struct GoToPatrol {
    /// Index of the loop waypoint the bot is currently heading to. `None` until
    /// the first tick snaps it to the nearest waypoint.
    cursor: Option<usize>,
    /// `false` until a route to the current waypoint has been installed. Gates
    /// the "advance to the next waypoint on arrival" step so the first leg heads
    /// to the nearest waypoint instead of skipping past it.
    engaged: bool,
    /// In-flight route request id while awaiting a leg's path.
    awaiting: Option<RequestId>,
    await_elapsed: f32,
    /// Consecutive unreachable legs tried this round (bounds the async retry).
    legs_tried: usize,
    /// Active leg travel budget while [`FollowPath`] is driving toward a waypoint.
    leg: Option<LegDeadline>,
    prev_low_stuck: bool,
    prev_low_finished: bool,
}

impl GoToPatrol {
    pub fn new() -> Self {
        Self {
            cursor: None,
            engaged: false,
            awaiting: None,
            await_elapsed: 0.0,
            legs_tried: 0,
            leg: None,
            prev_low_stuck: false,
            prev_low_finished: false,
        }
    }

    /// Enqueues a route to the next loop waypoint that isn't the bot's own tile,
    /// advancing `cursor` to it and parking the bot. Falls back to a retry `Wait`
    /// when every waypoint is the current tile.
    fn request_leg(
        &mut self,
        pf: PathfindAccess,
        loop_tiles: &[(i32, i32)],
        here: (i32, i32),
        cursor: &mut usize,
        low: &mut Box<dyn LowLevelAction>,
        entity: Entity,
        reason: PathfindReason,
        include_dynamic: bool,
        radius: i32,
    ) {
        let len = loop_tiles.len();
        for _ in 0..len {
            let target = loop_tiles[*cursor];
            if target != here {
                let id = pf.queue.enqueue(PathKind::WorldRoute {
                    start: here,
                    goal: target,
                    max_expanded: WANDER_SEARCH_LIMIT,
                    simplify_buffer: PATH_CORNER_BUFFER,
                    include_dynamic,
                    radius,
                }, entity, reason);
                self.awaiting = Some(id);
                self.await_elapsed = 0.0;
                self.leg = Some(LegDeadline::new(here, target));
                *low = Box::new(PendingPath::with_velocity(low.velocity()));
                return;
            }
            *cursor = (*cursor + 1) % len;
        }
        self.awaiting = None;
        self.leg = None;
        self.engaged = false;
        *low = Box::new(Wait::retry(RETRY_S));
    }
}

impl Default for GoToPatrol {
    fn default() -> Self {
        Self::new()
    }
}

impl HighLevelAction for GoToPatrol {
    fn kind(&self) -> PriorityKind {
        PriorityKind::Patrolling
    }
    fn label(&self) -> String {
        "GoToPatrol".to_string()
    }
    fn update(
        &mut self,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        _rng: &mut StdRng,
    ) -> HighLevelOutcome {
        // No usable route yet (loop still generating, or not a patrol bot): hold.
        let Some(loop_tiles) = ctx.patrol_loop.filter(|l| !l.is_empty()) else {
            if low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished) {
                *low = Box::new(Wait::retry(RETRY_S));
            }
            self.prev_low_stuck = low.is_stuck();
            self.prev_low_finished = low.is_finished();
            return HighLevelOutcome::running();
        };
        let Some(pf) = ctx.pathfind else {
            return HighLevelOutcome::running();
        };

        let here = (ctx.main_tile.x, ctx.main_tile.y);
        let len = loop_tiles.len();
        let mut cursor = match self.cursor {
            Some(c) => c % len,
            None => {
                // Fresh action (spawn, or returning from recharge): resume at the
                // loop waypoint nearest the bot's current position.
                self.engaged = false;
                nearest_loop_index(loop_tiles, here)
            }
        };

        if let Some(id) = self.awaiting {
            self.await_elapsed += ctx.dt;
            if let Some(outcome) = pf.results.take(id) {
                self.awaiting = None;
                match outcome {
                    PathOutcome::Route { path, raw_len } if raw_len > 1 => {
                        if let Some(leg) = &mut self.leg {
                            leg.reset_elapsed();
                        }
                        *low = Box::new(FollowPath::new(path));
                        self.engaged = true;
                        self.legs_tried = 0;
                    }
                    _ => {
                        // This leg is unreachable: advance and try the next, bounded
                        // by the loop length so we don't spin forever.
                        self.legs_tried += 1;
                        if self.legs_tried >= len {
                            self.legs_tried = 0;
                            self.engaged = false;
                            self.awaiting = None;
                            *low = Box::new(Wait::retry(RETRY_S));
                        } else {
                            cursor = (cursor + 1) % len;
                            self.request_leg(pf, loop_tiles, here, &mut cursor, low, ctx.entity, PathfindReason::PatrolLegUnreachable, ctx.dynamic_repath, ctx.radius_subtiles);
                        }
                    }
                }
            } else if self.await_elapsed >= PATH_WAIT_RETRY_S {
                self.request_leg(pf, loop_tiles, here, &mut cursor, low, ctx.entity, PathfindReason::PatrolLegRetry, ctx.dynamic_repath, ctx.radius_subtiles);
            }
            self.prev_low_stuck = low.is_stuck();
            self.prev_low_finished = low.is_finished();
            self.cursor = Some(cursor);
            return HighLevelOutcome::running();
        }

        if let Some(leg) = &mut self.leg {
            if is_follow_path(low.as_ref()) && !low.is_finished() && leg.tick(ctx.dt) {
                let waypoint = leg.goal;
                self.leg = None;
                low.halt();
                cursor = (cursor + 1) % len;
                self.engaged = true;
                self.legs_tried = 0;
                self.request_leg(pf, loop_tiles, here, &mut cursor, low, ctx.entity, PathfindReason::PatrolLegTimedOut, ctx.dynamic_repath, ctx.radius_subtiles);
                self.prev_low_stuck = low.is_stuck();
                self.prev_low_finished = low.is_finished();
                self.cursor = Some(cursor);
                return HighLevelOutcome::running_with(BrainEffects {
                    log: Some(BrainLogEvent::PatrolWaypointSkipped { waypoint }),
                    ..BrainEffects::default()
                });
            }
        }

        if low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished) {
            // Once we have reached (or abandoned) the waypoint we were heading to,
            // move on to the next; the first engaged leg keeps the nearest one.
            self.leg = None;
            if self.engaged {
                cursor = (cursor + 1) % len;
            }
            self.legs_tried = 0;
            self.request_leg(pf, loop_tiles, here, &mut cursor, low, ctx.entity, PathfindReason::PatrolLeg, ctx.dynamic_repath, ctx.radius_subtiles);
        }

        self.prev_low_stuck = low.is_stuck();
        self.prev_low_finished = low.is_finished();
        self.cursor = Some(cursor);
        HighLevelOutcome::running()
    }
}

/// Index of the loop waypoint closest (squared tile distance) to `here`.
fn nearest_loop_index(loop_tiles: &[(i32, i32)], here: (i32, i32)) -> usize {
    loop_tiles
        .iter()
        .enumerate()
        .min_by_key(|&(_, &(x, y))| {
            let dx = (x - here.0) as i64;
            let dy = (y - here.1) as i64;
            dx * dx + dy * dy
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Samples up to [`PATROL_CANDIDATES`] distinct walkable tiles within
/// [`PATROL_RADIUS`] of `anchor` and enqueues an `anchor -> tile` reachability
/// route for each. Returns the `(request id, tile)` pairs in sample order; the
/// caller resolves them and assembles the loop with [`assemble_patrol_loop`].
///
/// Reachability from a common anchor keeps consecutive waypoints mutually
/// reachable within the connected region, so the cycle never strands the bot.
pub fn enqueue_patrol_candidates(
    rng: &mut StdRng,
    anchor: (i32, i32),
    passability: &Hypermap<f32>,
    queue: &crate::map::pathfind_service::PathfindQueue,
    entity: Entity,
) -> Vec<(RequestId, (i32, i32))> {
    let mut tiles: Vec<(i32, i32)> = Vec::new();
    for _ in 0..PATROL_GEN_ATTEMPTS {
        if tiles.len() >= PATROL_CANDIDATES {
            break;
        }
        let dx: f32 = rng::range(rng, -PATROL_RADIUS..PATROL_RADIUS);
        let dy: f32 = rng::range(rng, -PATROL_RADIUS..PATROL_RADIUS);
        if dx * dx + dy * dy > PATROL_RADIUS * PATROL_RADIUS {
            continue;
        }
        let tile = (anchor.0 + dx.round() as i32, anchor.1 + dy.round() as i32);
        if tile == anchor || tiles.contains(&tile) {
            continue;
        }
        if world_tile_walkable(passability, tile.0, tile.1) {
            tiles.push(tile);
        }
    }
    tiles
        .into_iter()
        .map(|tile| {
            let id = queue.enqueue(PathKind::WorldRoute {
                start: anchor,
                goal: tile,
                max_expanded: WANDER_SEARCH_LIMIT,
                simplify_buffer: PATH_CORNER_BUFFER,
                include_dynamic: false,
                radius: 0,
            }, entity, PathfindReason::PatrolLoopGen);
            (id, tile)
        })
        .collect()
}

/// Builds the fixed patrol loop from resolved candidates `(tile, reachable)` in
/// sample order: keeps up to [`PATROL_LOOP_LEN`] distinct reachable tiles.
pub fn assemble_patrol_loop(resolved: &[((i32, i32), bool)]) -> Vec<(i32, i32)> {
    let mut loop_tiles: Vec<(i32, i32)> = Vec::new();
    for &(tile, reachable) in resolved {
        if loop_tiles.len() >= PATROL_LOOP_LEN {
            break;
        }
        if reachable && !loop_tiles.contains(&tile) {
            loop_tiles.push(tile);
        }
    }
    loop_tiles
}

// ---------------------------------------------------------------------------
// GoFixBots
// ---------------------------------------------------------------------------

/// Manhattan radius (tiles) around the home depot within which a fixer loiters
/// and watches the dispatch queue. Outside it, the fixer heads back and ignores
/// the queue.
const FIXER_LOITER_RADIUS: i32 = 10;
/// Squared tile distance at which a fixer is "close enough" to a stranded bot to
/// repair it — near but not touching (avoids a collision with the target).
const FIX_REACH_SQ: f32 = 2.25; // 1.5 tiles
/// Inclusive charge level a delivered [`Battery`](RepairPart::Battery) restores a
/// discharged bot to — a partial top-up (it must still seek a charger for the
/// rest), rolled per delivery from the fixer's seeded RNG.
const BATTERY_RECHARGE_MIN: f32 = 0.5;
const BATTERY_RECHARGE_MAX: f32 = 0.7;

/// Phase of the perpetual fixer routine.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FixPhase {
    /// No task: loiter near the home depot and watch the dispatch queue.
    Loiter,
    /// Claimed a task: travel to the home depot to pick up the part.
    FetchPart,
    /// Carrying the part: travel to the stranded bot and repair on contact.
    Deliver,
    /// Delivered: travel back to the home depot, then loiter.
    ReturnHome,
    /// Carrying a part with no active claim (gave up / abandoned the task):
    /// travel to the nearest depot and drop the part there, then loiter.
    DropPart,
}

/// Outcome of polling an in-flight fixer route.
enum RoutePoll {
    /// No result yet.
    Waiting,
    /// Route arrived (more than one tile to walk).
    Route(Vec<(i32, i32)>),
    /// Already standing on the goal tile (`raw_len <= 1`).
    AtGoal,
    /// Search failed (no path / limit) or timed out.
    Failed,
}

/// Perpetual fixer routine: loiter near the home parts depot watching the
/// [`DispatchQueue`](crate::actor::dispatch::DispatchQueue); on a claimed request,
/// fetch the part from the depot into the bot's
/// [`BotInventory`](crate::actor::dispatch::BotInventory), drive to the stranded
/// bot, repair it on contact, then return home. Never reports `Done`.
///
/// Like [`GoToPatrol`] the action is transient (rebuilt whenever `Fixing` becomes
/// dominant again after a recharge); the home depot lives on the bot's
/// [`Fixer`](crate::actor::black_bot::Fixer) component and the claim lives on the
/// shared dispatch board, so a recharge detour resumes cleanly.
pub struct GoFixBots {
    phase: FixPhase,
    /// The repair request this fixer claimed (its broken bot, part, location).
    claim: Option<RepairRequest>,
    awaiting: Option<RequestId>,
    await_elapsed: f32,
    leg: Option<LegDeadline>,
    prev_low_stuck: bool,
    prev_low_finished: bool,
}

impl GoFixBots {
    pub fn new() -> Self {
        Self {
            phase: FixPhase::Loiter,
            claim: None,
            awaiting: None,
            await_elapsed: 0.0,
            leg: None,
            prev_low_stuck: false,
            prev_low_finished: false,
        }
    }

    /// Enqueues a world route to `goal` and parks the bot in [`PendingPath`].
    fn start_route(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        goal: (i32, i32),
        reason: PathfindReason,
    ) {
        let here = (ctx.main_tile.x, ctx.main_tile.y);
        let id = pf.queue.enqueue(
            PathKind::WorldRoute {
                start: here,
                goal,
                max_expanded: WANDER_SEARCH_LIMIT,
                simplify_buffer: PATH_CORNER_BUFFER,
                include_dynamic: ctx.dynamic_repath,
                radius: ctx.radius_subtiles,
            },
            ctx.entity,
            reason,
        );
        self.awaiting = Some(id);
        self.await_elapsed = 0.0;
        self.leg = Some(LegDeadline::new(here, goal));
        *low = Box::new(PendingPath::with_velocity(low.velocity()));
    }

    /// Polls the in-flight route request, consuming it when it lands.
    fn poll_route(&mut self, pf: PathfindAccess, dt: f32) -> RoutePoll {
        let Some(id) = self.awaiting else {
            return RoutePoll::Failed;
        };
        self.await_elapsed += dt;
        if let Some(outcome) = pf.results.take(id) {
            self.awaiting = None;
            match outcome {
                PathOutcome::Route { path, raw_len } if raw_len > 1 => RoutePoll::Route(path),
                PathOutcome::Route { .. } => RoutePoll::AtGoal,
                _ => RoutePoll::Failed,
            }
        } else if self.await_elapsed >= PATH_WAIT_RETRY_S {
            self.awaiting = None;
            RoutePoll::Failed
        } else {
            RoutePoll::Waiting
        }
    }

    /// Picks a fresh loiter destination: a random walkable tile within the loiter
    /// radius of `home` (never the depot tile itself). Fixers outside the zone
    /// path toward such a tile instead of routing to the depot first.
    fn start_loiter_wander(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
        home: (i32, i32),
    ) {
        match sample_tile_within_radius(rng, home, FIXER_LOITER_RADIUS as f32, ctx.passability) {
            Some(goal) => self.start_route(pf, ctx, low, goal, PathfindReason::FixerLoiter),
            None => {
                self.awaiting = None;
                self.leg = None;
                *low = Box::new(Wait::retry(RETRY_S));
            }
        }
    }

    /// Drops the current claim back to the pool — used when the fixer cannot
    /// reach the stranded bot and must give up the task. Inventory is not cleared;
    /// the carried part persists until the next successful delivery overwrites it.
    fn abandon_claim(&mut self, fx: FixerContext, entity: Entity) {
        if self.claim.take().is_some() {
            // Give-up: bar re-claim for a cooldown so the fixer doesn't immediately
            // re-fetch a target it just failed to reach (depot pickup/drop churn).
            fx.dispatch.release_with_cooldown(entity, FIXER_TASK_COOLDOWN_S);
        }
    }

    fn push_remember_unreachable(effects: &mut BrainEffects, location: IVec2) {
        let n = effects.remember_unreachable_len as usize;
        if n < MAX_REMEMBER_UNREACHABLE_PER_TICK {
            effects.remember_unreachable[n] = location;
            effects.remember_unreachable_len += 1;
        }
    }

    fn stranded_bot_ignored(fx: FixerContext, location: IVec2) -> bool {
        fx.ignored_unreachable.contains(&location)
    }

    fn fixer_can_reach_stranded(ctx: &BrainContext, goal: IVec2) -> bool {
        let start = (ctx.main_tile.x, ctx.main_tile.y);
        let goal = (goal.x, goal.y);
        matches!(
            astar_shortest_world_path(
                ctx.passability,
                start,
                goal,
                HypermapSearchLimits {
                    max_expanded: WANDER_SEARCH_LIMIT,
                },
            ),
            HypermapPathResult::Found { .. }
        )
    }

    /// Picks a random open task the fixer has not blacklisted and can path to.
    /// Unreachable candidates are remembered and left unclaimed for other fixers.
    fn try_claim_reachable_task(
        &mut self,
        pf: PathfindAccess,
        fx: FixerContext,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
        home_tile: (i32, i32),
    ) -> Option<HighLevelOutcome> {
        let mut candidates: Vec<RepairRequest> = fx
            .dispatch
            .open_requests()
            .into_iter()
            .filter(|req| !Self::stranded_bot_ignored(fx, req.location))
            .collect();
        if candidates.is_empty() {
            return None;
        }

        let mut effects = BrainEffects::default();
        let mut logged_unreachable = false;
        while !candidates.is_empty() {
            let pick = rng::range(rng, 0..candidates.len());
            let req = candidates.swap_remove(pick);
            if Self::fixer_can_reach_stranded(ctx, req.location) {
                let claimed = fx.dispatch.claim_bot(ctx.entity, req.broken_bot)?;
                self.claim = Some(claimed);
                self.phase = FixPhase::FetchPart;
                self.leg = None;
                low.halt();
                self.start_route(pf, ctx, low, home_tile, PathfindReason::FixerFetchPart);
                effects.integer_memory_write = Some((IntegerMemoryId::HelpFailuresCount, 0));
                return Some(HighLevelOutcome::running_with(effects));
            }
            Self::push_remember_unreachable(&mut effects, req.location);
            if !logged_unreachable {
                effects.log = Some(BrainLogEvent::FixerTargetUnreachable {
                    target: (req.location.x, req.location.y),
                });
                logged_unreachable = true;
            }
        }

        Some(HighLevelOutcome::running_with(effects))
    }
}

impl Default for GoFixBots {
    fn default() -> Self {
        Self::new()
    }
}

impl HighLevelAction for GoFixBots {
    fn kind(&self) -> PriorityKind {
        PriorityKind::Fixing
    }
    fn label(&self) -> String {
        match self.phase {
            FixPhase::Loiter => "GoFixBots (loiter)".to_string(),
            FixPhase::FetchPart => "GoFixBots (fetch part)".to_string(),
            FixPhase::Deliver => "GoFixBots (deliver)".to_string(),
            FixPhase::ReturnHome => "GoFixBots (return)".to_string(),
            FixPhase::DropPart => "GoFixBots (drop part)".to_string(),
        }
    }
    fn preempt(&mut self, _ctx: &BrainContext) -> BrainEffects {
        // Keep the dispatch claim and inventory — the fixer resumes its assigned
        // task after recharging. GoFixBots::update recovers the claim via claim_of
        // when Fixing becomes dominant again.
        self.phase = FixPhase::Loiter;
        self.awaiting = None;
        self.leg = None;
        BrainEffects::default()
    }
    fn update(
        &mut self,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
    ) -> HighLevelOutcome {
        let (Some(pf), Some(fx)) = (ctx.pathfind, ctx.fixer) else {
            return HighLevelOutcome::running();
        };
        // No reachable home depot located yet: hold until one is found.
        let Some(home) = fx.home_depot else {
            *low = Box::new(Wait::retry(RETRY_S));
            return HighLevelOutcome::running();
        };
        let here = (ctx.main_tile.x, ctx.main_tile.y);
        let home_tile = (home.x, home.y);

        // Safety net against an **orphaned `PendingPath`**. A route request that
        // fails or times out clears `awaiting`, and several phase transitions hand
        // the next phase a `PendingPath` that is no longer backed by an in-flight
        // request. `PendingPath` never reports stuck or finished, so a phase's
        // `low_level_needs_replan` gate can never fire to recover it — the bot
        // coasts in `PendingPath` forever (observed on loitering fixers). If we
        // hold one with nothing actually awaiting, drop to a short retry so the
        // phase handler's replan logic runs next tick.
        if self.awaiting.is_none() && low.kind() == LowLevelKind::PendingPath {
            *low = Box::new(Wait::retry(RETRY_S));
        }

        // Claim recovery / part return after a brain reset or recharge
        // pre-emption. When the brain is wiped, GoFixBots is re-created in Loiter
        // with no local claim.
        if self.phase == FixPhase::Loiter && self.claim.is_none() {
            if let Some(req) = fx.dispatch.claim_of(ctx.entity) {
                // The dispatch board still holds this fixer's claim — re-attach it
                // and resume, skipping the fetch if inventory is already loaded.
                self.claim = Some(req);
                self.leg = None;
                low.halt();
                if fx.carried.is_some() {
                    self.phase = FixPhase::Deliver;
                    self.start_route(
                        pf,
                        ctx,
                        low,
                        (req.location.x, req.location.y),
                        PathfindReason::FixerDeliver,
                    );
                } else {
                    self.phase = FixPhase::FetchPart;
                    self.start_route(pf, ctx, low, home_tile, PathfindReason::FixerFetchPart);
                }
                self.prev_low_stuck = low.is_stuck();
                self.prev_low_finished = low.is_finished();
                return HighLevelOutcome::running();
            } else if fx.carried.is_some() {
                // No claim but still carrying a part (gave up after too many help
                // failures, or abandoned an unreachable target): return the part
                // to the nearest depot instead of hauling it around forever.
                self.phase = FixPhase::DropPart;
                self.leg = None;
                low.halt();
                let depot = drop_off_depot(ctx, fx).unwrap_or(home_tile);
                self.start_route(pf, ctx, low, depot, PathfindReason::FixerDropPart);
                self.prev_low_stuck = low.is_stuck();
                self.prev_low_finished = low.is_finished();
                return HighLevelOutcome::running();
            }
        }

        let outcome = match self.phase {
            FixPhase::Loiter => self.update_loiter(pf, fx, ctx, low, rng, here, home_tile),
            FixPhase::FetchPart => self.update_fetch(pf, fx, ctx, low, here, home_tile),
            FixPhase::Deliver => self.update_deliver(pf, fx, ctx, low, here, home_tile, rng),
            FixPhase::ReturnHome => self.update_return(pf, ctx, low, here, home_tile),
            FixPhase::DropPart => self.update_drop_part(pf, fx, ctx, low),
        };
        self.prev_low_stuck = low.is_stuck();
        self.prev_low_finished = low.is_finished();
        outcome
    }
}

impl GoFixBots {
    fn update_loiter(
        &mut self,
        pf: PathfindAccess,
        fx: FixerContext,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
        here: (i32, i32),
        home_tile: (i32, i32),
    ) -> HighLevelOutcome {
        // Resolve any in-flight loiter route first.
        if self.awaiting.is_some() {
            match self.poll_route(pf, ctx.dt) {
                RoutePoll::Waiting => return HighLevelOutcome::running(),
                RoutePoll::Route(path) => {
                    if let Some(leg) = &mut self.leg {
                        leg.reset_elapsed();
                    }
                    *low = Box::new(FollowPath::new(path));
                    return HighLevelOutcome::running();
                }
                RoutePoll::AtGoal | RoutePoll::Failed => self.leg = None,
            }
        }

        // Watch the dispatch queue only while near the home depot.
        if manhattan(here, home_tile) as i32 <= FIXER_LOITER_RADIUS {
            if let Some(outcome) =
                self.try_claim_reachable_task(pf, fx, ctx, low, rng, home_tile)
            {
                return outcome;
            }
        }

        // Wander leg travel budget.
        if let Some(leg) = &mut self.leg {
            if is_follow_path(low.as_ref()) && !low.is_finished() && leg.tick(ctx.dt) {
                self.leg = None;
                low.halt();
                self.start_loiter_wander(pf, ctx, low, rng, home_tile);
                return HighLevelOutcome::running();
            }
        }

        // Pick a new loiter destination when idle / arrived / stuck.
        if low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished) {
            self.start_loiter_wander(pf, ctx, low, rng, home_tile);
        }
        HighLevelOutcome::running()
    }

    fn update_fetch(
        &mut self,
        pf: PathfindAccess,
        fx: FixerContext,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        here: (i32, i32),
        home_tile: (i32, i32),
    ) -> HighLevelOutcome {
        let Some(req) = self.claim else {
            self.phase = FixPhase::Loiter;
            return HighLevelOutcome::running();
        };

        if self.awaiting.is_some() {
            match self.poll_route(pf, ctx.dt) {
                RoutePoll::Waiting => return HighLevelOutcome::running(),
                RoutePoll::Route(path) => {
                    if let Some(leg) = &mut self.leg {
                        leg.reset_elapsed();
                    }
                    *low = Box::new(FollowPath::new(path));
                    return HighLevelOutcome::running();
                }
                RoutePoll::AtGoal => {
                    // Standing on the depot: pick up the part and head to the bot.
                    return self.pick_up_and_deliver(pf, ctx, low, req);
                }
                RoutePoll::Failed => {
                    *low = Box::new(Wait::retry(RETRY_S));
                    return HighLevelOutcome::running();
                }
            }
        }

        // Reached the depot tile? Pick up.
        if manhattan(here, home_tile) == 0 {
            return self.pick_up_and_deliver(pf, ctx, low, req);
        }

        // Leg budget / arrival / stuck handling: re-route to the depot.
        let leg_expired = self
            .leg
            .as_mut()
            .map(|leg| is_follow_path(low.as_ref()) && !low.is_finished() && leg.tick(ctx.dt))
            .unwrap_or(false);
        if leg_expired
            || low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished)
        {
            self.leg = None;
            low.halt();
            self.start_route(pf, ctx, low, home_tile, PathfindReason::FixerFetchPart);
        }
        let _ = fx;
        HighLevelOutcome::running()
    }

    /// At the depot with a claim: load the part into inventory and route to the
    /// stranded bot.
    fn pick_up_and_deliver(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        req: RepairRequest,
    ) -> HighLevelOutcome {
        self.phase = FixPhase::Deliver;
        self.leg = None;
        low.halt();
        self.start_route(pf, ctx, low, (req.location.x, req.location.y), PathfindReason::FixerDeliver);
        HighLevelOutcome::running_with(BrainEffects {
            pickup_part: Some(req.part),
            ..BrainEffects::default()
        })
    }

    fn update_deliver(
        &mut self,
        pf: PathfindAccess,
        fx: FixerContext,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        _here: (i32, i32),
        home_tile: (i32, i32),
        rng: &mut StdRng,
    ) -> HighLevelOutcome {
        let Some(req) = self.claim else {
            self.phase = FixPhase::ReturnHome;
            return HighLevelOutcome::running();
        };

        // Close enough to service? Act on proximity, before colliding with the bot.
        let target_center = Vec2::new(req.location.x as f32 + 0.5, req.location.y as f32 + 0.5);
        if (ctx.center - target_center).length_squared() <= FIX_REACH_SQ {
            low.halt();
            fx.dispatch.complete(req.broken_bot);
            self.claim = None;
            self.phase = FixPhase::ReturnHome;
            self.leg = None;
            self.start_route(pf, ctx, low, home_tile, PathfindReason::FixerReturnHome);
            // A battery recharges the discharged bot; any other part is a repair.
            // Task succeeded: clear the per-task help-failure counter.
            let mut effects = BrainEffects {
                clear_inventory: true,
                integer_memory_write: Some((IntegerMemoryId::HelpFailuresCount, 0)),
                ..BrainEffects::default()
            };
            if req.part == RepairPart::Battery {
                let level = rng::range(rng, BATTERY_RECHARGE_MIN..=BATTERY_RECHARGE_MAX);
                effects.recharge_target = Some((req.broken_bot, level));
            } else {
                effects.repair_target = Some((req.broken_bot, req.part));
            }
            return HighLevelOutcome::running_with(effects);
        }

        if self.awaiting.is_some() {
            match self.poll_route(pf, ctx.dt) {
                RoutePoll::Waiting => return HighLevelOutcome::running(),
                RoutePoll::Route(path) => {
                    if let Some(leg) = &mut self.leg {
                        leg.reset_elapsed();
                    }
                    *low = Box::new(FollowPath::new(path));
                    return HighLevelOutcome::running();
                }
                // Can't reach the bot's tile (it's occupied / boxed in): give up,
                // return home so the task frees for another fixer. Inventory kept.
                RoutePoll::AtGoal | RoutePoll::Failed => {
                    let target = (req.location.x, req.location.y);
                    self.abandon_claim(fx, ctx.entity);
                    self.phase = FixPhase::ReturnHome;
                    self.leg = None;
                    self.start_route(pf, ctx, low, home_tile, PathfindReason::FixerReturnHome);
                    let mut effects = BrainEffects {
                        log: Some(BrainLogEvent::FixerTargetUnreachable { target }),
                        ..BrainEffects::default()
                    };
                    Self::push_remember_unreachable(&mut effects, req.location);
                    return HighLevelOutcome::running_with(effects);
                }
            }
        }

        let leg_expired = self
            .leg
            .as_mut()
            .map(|leg| is_follow_path(low.as_ref()) && !low.is_finished() && leg.tick(ctx.dt))
            .unwrap_or(false);
        if leg_expired
            || low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished)
        {
            self.leg = None;
            low.halt();
            self.start_route(pf, ctx, low, (req.location.x, req.location.y), PathfindReason::FixerDeliver);
        }
        HighLevelOutcome::running()
    }

    fn update_return(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        here: (i32, i32),
        home_tile: (i32, i32),
    ) -> HighLevelOutcome {
        if self.awaiting.is_some() {
            match self.poll_route(pf, ctx.dt) {
                RoutePoll::Waiting => return HighLevelOutcome::running(),
                RoutePoll::Route(path) => {
                    if let Some(leg) = &mut self.leg {
                        leg.reset_elapsed();
                    }
                    *low = Box::new(FollowPath::new(path));
                    return HighLevelOutcome::running();
                }
                RoutePoll::AtGoal | RoutePoll::Failed => {
                    self.phase = FixPhase::Loiter;
                    self.leg = None;
                    return HighLevelOutcome::running();
                }
            }
        }

        if manhattan(here, home_tile) as i32 <= FIXER_LOITER_RADIUS {
            // Back within the loiter zone: resume loitering.
            self.phase = FixPhase::Loiter;
            self.leg = None;
            return HighLevelOutcome::running();
        }

        let leg_expired = self
            .leg
            .as_mut()
            .map(|leg| is_follow_path(low.as_ref()) && !low.is_finished() && leg.tick(ctx.dt))
            .unwrap_or(false);
        if leg_expired
            || low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished)
        {
            self.leg = None;
            low.halt();
            self.start_route(pf, ctx, low, home_tile, PathfindReason::FixerReturnHome);
        }
        HighLevelOutcome::running()
    }

    /// Carrying a part with no claim: travel to the nearest reachable depot and
    /// drop the part there (`clear_inventory`), then resume loitering. Reached
    /// when the give-up logic released the claim but kept the part.
    fn update_drop_part(
        &mut self,
        pf: PathfindAccess,
        fx: FixerContext,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
    ) -> HighLevelOutcome {
        let here = (ctx.main_tile.x, ctx.main_tile.y);
        let Some(depot) = drop_off_depot(ctx, fx) else {
            // No depot reachable at all: drop in place rather than stall forever.
            self.phase = FixPhase::Loiter;
            self.leg = None;
            return HighLevelOutcome::running_with(BrainEffects {
                clear_inventory: true,
                ..BrainEffects::default()
            });
        };

        // Standing on the depot: drop the part and resume loitering.
        if here == depot {
            self.phase = FixPhase::Loiter;
            self.leg = None;
            low.halt();
            return HighLevelOutcome::running_with(BrainEffects {
                clear_inventory: true,
                ..BrainEffects::default()
            });
        }

        if self.awaiting.is_some() {
            match self.poll_route(pf, ctx.dt) {
                RoutePoll::Waiting => return HighLevelOutcome::running(),
                RoutePoll::Route(path) => {
                    if let Some(leg) = &mut self.leg {
                        leg.reset_elapsed();
                    }
                    *low = Box::new(FollowPath::new(path));
                    return HighLevelOutcome::running();
                }
                RoutePoll::AtGoal => {
                    self.phase = FixPhase::Loiter;
                    self.leg = None;
                    low.halt();
                    return HighLevelOutcome::running_with(BrainEffects {
                        clear_inventory: true,
                        ..BrainEffects::default()
                    });
                }
                RoutePoll::Failed => {
                    *low = Box::new(Wait::retry(RETRY_S));
                    return HighLevelOutcome::running();
                }
            }
        }

        let leg_expired = self
            .leg
            .as_mut()
            .map(|leg| is_follow_path(low.as_ref()) && !low.is_finished() && leg.tick(ctx.dt))
            .unwrap_or(false);
        if leg_expired
            || low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished)
        {
            self.leg = None;
            low.halt();
            self.start_route(pf, ctx, low, depot, PathfindReason::FixerDropPart);
        }
        HighLevelOutcome::running()
    }
}

/// Nearest reachable parts depot to drop a carried part at: the closest one to
/// the bot's current tile, falling back to its home depot. `None` only when the
/// fixer has neither (it then drops in place).
fn drop_off_depot(ctx: &BrainContext, fx: FixerContext) -> Option<(i32, i32)> {
    nearest_reachable_depot(ctx.interactive, ctx.passability, ctx.main_tile)
        .or(fx.home_depot)
        .map(|c| (c.x, c.y))
}

// ---------------------------------------------------------------------------
// GoToChargeStation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum ChargePhase {
    Seeking,
    Traveling,
    WaitingQueue,
    Charging,
}

/// In-flight charger-selection scan: a `WorldRoute` request per candidate
/// charger, accumulating resolved routes until all return (or the wait times
/// out), then ranked by [`rank_charger_candidates`].
struct ChargerSeek {
    pending: Vec<(EntityCoordinates, RequestId)>,
    resolved: Vec<ChargerCandidate>,
    elapsed: f32,
}

/// Path to the nearest accessible, unoccupied charger, dock, charge to full,
/// then report `Done`. All route searches are queued through the async
/// pathfinding service; the bot parks in a [`PendingPath`] while awaiting them.
pub struct GoToChargeStation {
    phase: ChargePhase,
    charger: Option<EntityCoordinates>,
    queued_wanting: Option<EntityCoordinates>,
    queued_waiting: Option<EntityCoordinates>,
    prev_low_stuck: bool,
    /// Active multi-route charger scan (Seeking phase).
    seek: Option<ChargerSeek>,
    /// In-flight dock-approach route id (WaitingQueue → dock / re-approach).
    dock_route: Option<RequestId>,
    dock_elapsed: f32,
}

impl GoToChargeStation {
    pub fn new() -> Self {
        Self {
            phase: ChargePhase::Seeking,
            charger: None,
            queued_wanting: None,
            queued_waiting: None,
            prev_low_stuck: false,
            seek: None,
            dock_route: None,
            dock_elapsed: 0.0,
        }
    }

    /// Starts a charger scan: gather candidates, enqueue a route to each, and
    /// park the bot. Falls back to a retry `Wait` when there are no candidates.
    ///
    /// `exclude`, when set, drops that charger from the candidate list so a
    /// reroute (stuck / overcrowded queue) genuinely picks a *different* station
    /// — unless it is the only candidate, in which case it is kept rather than
    /// stranding the bot with no charger at all.
    fn begin_seek_into(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        effects: &mut BrainEffects,
        exclude: Option<EntityCoordinates>,
    ) {
        self.phase = ChargePhase::Seeking;
        self.seek = None;
        let here = (ctx.main_tile.x, ctx.main_tile.y);
        let candidates = gather_charger_candidates(ctx, exclude);
        if candidates.is_empty() {
            self.clear_queues(effects);
            self.charger = None;
            *low = Box::new(Wait::new(CHARGE_RETRY_S));
            return;
        }
        let mut pending = Vec::with_capacity(candidates.len());
        for coords in candidates {
            let id = pf.queue.enqueue(PathKind::WorldRoute {
                start: here,
                goal: (coords.x, coords.y),
                max_expanded: SEARCH_LIMIT,
                simplify_buffer: PATH_CORNER_BUFFER,
                include_dynamic: ctx.dynamic_repath,
                radius: ctx.radius_subtiles,
            }, ctx.entity, PathfindReason::ChargerSeek);
            pending.push((coords, id));
        }
        self.seek = Some(ChargerSeek {
            pending,
            resolved: Vec::new(),
            elapsed: 0.0,
        });
        *low = Box::new(PendingPath::with_velocity(low.velocity()));
    }

    /// Polls outstanding charger-scan routes; once all resolve (or the wait
    /// times out) ranks them and installs the winning route / a retry `Wait`.
    fn poll_seek(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
    ) -> HighLevelOutcome {
        let mut seek = match self.seek.take() {
            Some(s) => s,
            None => return HighLevelOutcome::running(),
        };
        seek.elapsed += ctx.dt;
        let mut still = Vec::new();
        for (coords, id) in seek.pending.drain(..) {
            if let Some(outcome) = pf.results.take(id) {
                if let PathOutcome::Route { path, raw_len } = outcome {
                    let waiting_len = ctx.interactive.waiting_len(coords);
                    seek.resolved.push((coords, path, waiting_len, raw_len));
                }
            } else {
                still.push((coords, id));
            }
        }
        seek.pending = still;

        let timed_out = seek.elapsed >= PATH_WAIT_RETRY_S;
        if !seek.pending.is_empty() && !timed_out {
            self.seek = Some(seek);
            return HighLevelOutcome::running();
        }

        let mut effects = BrainEffects::default();
        match rank_charger_candidates(std::mem::take(&mut seek.resolved)) {
            Some((coords, path)) => {
                self.retarget(coords, &mut effects);
                *low = Box::new(FollowPath::new(path));
            }
            None => {
                self.clear_queues(&mut effects);
                self.charger = None;
                *low = Box::new(Wait::new(CHARGE_RETRY_S));
            }
        }
        self.seek = None;
        HighLevelOutcome::running_with(effects)
    }

    fn clear_queues(&mut self, effects: &mut BrainEffects) {
        if let Some(coords) = self.queued_wanting.take() {
            effects.queue_unwant = Some(coords);
        }
        if let Some(coords) = self.queued_waiting.take() {
            effects.queue_unwait = Some(coords);
        }
    }

    /// Abandons the current charger and restarts the scan, dropping `exclude`
    /// from the candidate set so the bot reroutes to a *different* station.
    /// Used by both reroute triggers (stuck, overcrowded queue).
    fn reseek_excluding(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        exclude: Option<EntityCoordinates>,
    ) -> BrainEffects {
        let mut effects = BrainEffects::default();
        self.clear_queues(&mut effects);
        self.charger = None;
        self.dock_route = None;
        self.begin_seek_into(pf, ctx, low, &mut effects, exclude);
        effects
    }

    fn retarget(&mut self, coords: EntityCoordinates, effects: &mut BrainEffects) {
        if self.queued_wanting != Some(coords) {
            if let Some(old) = self.queued_wanting.replace(coords) {
                effects.queue_unwant = Some(old);
            }
            effects.queue_want = Some(coords);
        }
        if let Some(old_wait) = self.queued_waiting.take() {
            effects.queue_unwait = Some(old_wait);
        }
        self.charger = Some(coords);
        self.phase = ChargePhase::Traveling;
    }

    fn enter_waiting_queue(&mut self, coords: EntityCoordinates, effects: &mut BrainEffects) {
        if self.queued_waiting != Some(coords) {
            if let Some(old_wait) = self.queued_waiting.replace(coords) {
                effects.queue_unwait = Some(old_wait);
            }
            effects.queue_wait = Some(coords);
        }
        if self.queued_wanting == Some(coords) {
            self.queued_wanting = None;
        }
        self.phase = ChargePhase::WaitingQueue;
    }
}

impl Default for GoToChargeStation {
    fn default() -> Self {
        Self::new()
    }
}

impl HighLevelAction for GoToChargeStation {
    fn kind(&self) -> PriorityKind {
        PriorityKind::RechargeYourself
    }
    fn label(&self) -> String {
        match self.phase {
            ChargePhase::Seeking => "GoToChargeStation (seeking)".to_string(),
            ChargePhase::Traveling => "GoToChargeStation (traveling)".to_string(),
            ChargePhase::WaitingQueue => "GoToChargeStation (waiting queue)".to_string(),
            ChargePhase::Charging => "GoToChargeStation (charging)".to_string(),
        }
    }
    fn preempt(&mut self, _ctx: &BrainContext) -> BrainEffects {
        let mut effects = BrainEffects::default();
        if self.phase == ChargePhase::Charging {
            effects.undock = self.charger;
        }
        self.clear_queues(&mut effects);
        self.charger = None;
        self.seek = None;
        self.dock_route = None;
        self.phase = ChargePhase::Seeking;
        effects
    }
    fn active_queue(&self) -> Option<EntityCoordinates> {
        self.queued_waiting.or(self.queued_wanting)
    }
    fn update(
        &mut self,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
    ) -> HighLevelOutcome {
        let Some(pf) = ctx.pathfind else {
            return HighLevelOutcome::running();
        };

        let low_stuck = low.is_stuck();
        if low_stuck && !self.prev_low_stuck && self.phase != ChargePhase::Charging {
            // Handler for "need charge but got stuck": reroute to a *different*
            // charger on the rising edge only — not every frame while stalled.
            let effects = self.reseek_excluding(pf, ctx, low, self.charger);
            self.prev_low_stuck = low.is_stuck();
            return HighLevelOutcome::running_with(effects);
        }
        self.prev_low_stuck = low_stuck;

        // Bail to a less crowded charger when the chosen one's waiting queue has
        // grown past the limit (counting other bots only). Applies once we are
        // committed to a station and heading for / waiting at it; the in-flight
        // dock approach is dropped by `reseek_excluding`.
        if matches!(self.phase, ChargePhase::Traveling | ChargePhase::WaitingQueue) {
            if let Some(charger) = self.charger {
                if ctx.interactive.waiting_len_excluding(charger, ctx.entity)
                    > CHARGER_REROUTE_WAITING_LIMIT
                {
                    let effects = self.reseek_excluding(pf, ctx, low, Some(charger));
                    return HighLevelOutcome::running_with(effects);
                }
            }
        }

        match self.phase {
            ChargePhase::Seeking => {
                if self.seek.is_some() {
                    return self.poll_seek(pf, ctx, low);
                }
                if !ctx.main_tile_changed && !low.is_finished() {
                    return HighLevelOutcome::running();
                }
                let mut effects = BrainEffects::default();
                self.begin_seek_into(pf, ctx, low, &mut effects, None);
                HighLevelOutcome::running_with(effects)
            }
            ChargePhase::Traveling => {
                let Some(charger) = self.charger else {
                    self.phase = ChargePhase::Seeking;
                    return HighLevelOutcome::running();
                };

                // Join the waiting queue once, on first arrival in the zone. Once
                // we are already in this charger's waiting queue we have been
                // cleared by `WaitingQueue` to approach and dock, so we must keep
                // traveling — re-entering here on every tile boundary inside the
                // zone is what makes the bot stop-and-go at each step near the
                // charger.
                if ctx.main_tile_changed
                    && in_waiting_zone(ctx, charger)
                    && self.queued_waiting != Some(charger)
                {
                    let mut effects = BrainEffects::default();
                    self.enter_waiting_queue(charger, &mut effects);
                    *low = Box::new(Wait::new(short_wait_recheck_s(rng)));
                    return HighLevelOutcome::running_with(effects);
                }

                if low.is_finished() {
                    if dock_allowed_for(ctx, self.queued_waiting, charger) {
                        self.phase = ChargePhase::Charging;
                        *low = Box::new(Wait::new(f32::INFINITY));
                        let mut effects = BrainEffects::default();
                        effects.queue_unwait = self.queued_waiting.take();
                        effects.dock = Some(charger);
                        return HighLevelOutcome::running_with(effects);
                    }
                    if self.queued_waiting.is_some() {
                        self.phase = ChargePhase::WaitingQueue;
                        *low = Box::new(Wait::new(short_wait_recheck_s(rng)));
                    } else {
                        self.phase = ChargePhase::Seeking;
                        self.charger = None;
                    }
                }
                HighLevelOutcome::running()
            }
            ChargePhase::WaitingQueue => {
                let Some(charger) = self.charger else {
                    self.phase = ChargePhase::Seeking;
                    return HighLevelOutcome::running();
                };

                // Awaiting the queued dock-approach route.
                if let Some(id) = self.dock_route {
                    self.dock_elapsed += ctx.dt;
                    if let Some(outcome) = pf.results.take(id) {
                        self.dock_route = None;
                        match outcome {
                            PathOutcome::Route { raw_len, .. } if raw_len <= 1 => {
                                // Already on the charger tile: dock.
                                self.phase = ChargePhase::Charging;
                                *low = Box::new(Wait::new(f32::INFINITY));
                                let mut effects = BrainEffects::default();
                                effects.queue_unwait = self.queued_waiting.take();
                                effects.dock = Some(charger);
                                return HighLevelOutcome::running_with(effects);
                            }
                            PathOutcome::Route { path, .. } => {
                                self.phase = ChargePhase::Traveling;
                                *low = Box::new(FollowPath::new(path));
                            }
                            _ => {
                                self.phase = ChargePhase::Seeking;
                                self.charger = None;
                            }
                        }
                    } else if self.dock_elapsed >= PATH_WAIT_RETRY_S {
                        // The approach route never came back: drop it and recheck.
                        self.dock_route = None;
                        *low = Box::new(Wait::new(short_wait_recheck_s(rng)));
                    }
                    return HighLevelOutcome::running();
                }

                if low.is_finished() {
                    if dock_allowed_for(ctx, self.queued_waiting, charger) {
                        let here = (ctx.main_tile.x, ctx.main_tile.y);
                        let id = pf.queue.enqueue(PathKind::WorldRoute {
                            start: here,
                            goal: (charger.x, charger.y),
                            max_expanded: SEARCH_LIMIT,
                            simplify_buffer: PATH_CORNER_BUFFER,
                            include_dynamic: ctx.dynamic_repath,
                            radius: ctx.radius_subtiles,
                        }, ctx.entity, PathfindReason::ChargerDockApproach);
                        self.dock_route = Some(id);
                        self.dock_elapsed = 0.0;
                        *low = Box::new(PendingPath::with_velocity(low.velocity()));
                    } else {
                        *low = Box::new(Wait::new(short_wait_recheck_s(rng)));
                    }
                }
                HighLevelOutcome::running()
            }
            ChargePhase::Charging => {
                if ctx.charge >= CHARGE_FULL {
                    let mut e = BrainEffects::default();
                    e.queue_unwait = self.queued_waiting.take();
                    e.queue_unwant = self.queued_wanting.take();
                    e.undock = self.charger;
                    return HighLevelOutcome::done(e);
                }
                let mut e = BrainEffects::default();
                e.recharge = RECHARGE_PER_S * ctx.dt;
                HighLevelOutcome::running_with(e)
            }
        }
    }
}

/// Picks a random *walkable* tile within [`WANDER_RADIUS`] of `current_tile` to
/// route to, or `None` when no candidate was found this tick. Reachability is
/// validated asynchronously by the route request the caller enqueues for the
/// returned goal — a candidate that turns out unreachable just yields `NoPath`
/// and the caller samples again.
pub fn sample_wander_goal(
    rng: &mut StdRng,
    current_tile: (i32, i32),
    passability: &Hypermap<f32>,
) -> Option<(i32, i32)> {
    for _ in 0..MAX_TARGET_ATTEMPTS {
        let dx: f32 = rng::range(rng, -WANDER_RADIUS..WANDER_RADIUS);
        let dy: f32 = rng::range(rng, -WANDER_RADIUS..WANDER_RADIUS);
        if dx * dx + dy * dy > WANDER_RADIUS * WANDER_RADIUS {
            continue;
        }
        let target = (current_tile.0 + dx.round() as i32, current_tile.1 + dy.round() as i32);
        if target == current_tile {
            continue;
        }
        if world_tile_walkable(passability, target.0, target.1) {
            return Some(target);
        }
    }
    None
}

/// Picks a random *walkable* tile within `radius` (Euclidean, tiles) of `center`,
/// or `None` when none was found this tick. Used by a loitering fixer to wander
/// near its home depot.
pub fn sample_tile_within_radius(
    rng: &mut StdRng,
    center: (i32, i32),
    radius: f32,
    passability: &Hypermap<f32>,
) -> Option<(i32, i32)> {
    for _ in 0..MAX_TARGET_ATTEMPTS {
        let dx: f32 = rng::range(rng, -radius..radius);
        let dy: f32 = rng::range(rng, -radius..radius);
        if dx * dx + dy * dy > radius * radius {
            continue;
        }
        let target = (center.0 + dx.round() as i32, center.1 + dy.round() as i32);
        if target == center {
            continue;
        }
        if world_tile_walkable(passability, target.0, target.1) {
            return Some(target);
        }
    }
    None
}

/// Coordinates of every charger in the nearest 4 hypertiles on the bot's floor.
/// The reachability / path-cost ranking happens later from the async route
/// results (see [`rank_charger_candidates`]).
fn gather_charger_candidates(
    ctx: &BrainContext,
    exclude: Option<EntityCoordinates>,
) -> Vec<EntityCoordinates> {
    let here = (ctx.main_tile.x, ctx.main_tile.y);
    let nearby_chunks = nearest_hypertiles_4(here);
    let mut out = Vec::new();
    for entry in ctx.interactive.iter().filter(|e| e.coordinates.floor == ctx.floor) {
        let (chunk, _) = world_to_chunk_local(entry.coordinates.x, entry.coordinates.y);
        if !nearby_chunks.contains(&chunk) {
            continue;
        }
        if entry.entity.as_charger().is_none() {
            continue;
        }
        out.push(entry.coordinates);
    }
    // Drop the charger we're bailing from — but only if at least one other
    // candidate remains, so a lone charger is never excluded into starvation.
    if let Some(skip) = exclude {
        if out.iter().any(|&c| c != skip) {
            out.retain(|&c| c != skip);
        }
    }
    // Keep only the N nearest by Manhattan distance to avoid flooding the
    // pathfind queue — one A* request is enqueued per candidate.
    if out.len() > MAX_CHARGER_CANDIDATES {
        out.sort_by_key(|c| (c.x - here.0).abs() + (c.y - here.1).abs());
        out.truncate(MAX_CHARGER_CANDIDATES);
    }
    out
}

/// One resolved charger candidate: its coordinates, the (simplified) route to
/// it, the route cost (raw tile-path length), and the station's waiting-queue
/// length.
type ChargerCandidate = (EntityCoordinates, Vec<(i32, i32)>, usize, usize);

/// Queue-aware charger selection over resolved candidates: prefer the cheapest
/// route whose waiting queue has < 2 actors; when all are busier, bias toward
/// farther-ranked stations (`2nd nearest`, `3rd nearest`, ...). Mirrors the old
/// synchronous `find_best_charger` ranking.
fn rank_charger_candidates(
    mut candidates: Vec<ChargerCandidate>,
) -> Option<(EntityCoordinates, Vec<(i32, i32)>)> {
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by_key(|(_, _, _, path_cost)| *path_cost);
    if let Some((coords, path, _, _)) = candidates
        .iter()
        .find(|(_, _, waiting_len, _)| *waiting_len < 2)
        .cloned()
    {
        return Some((coords, path));
    }
    let closest_waiting = candidates[0].2;
    let rank = closest_waiting.saturating_sub(1);
    let idx = rank.min(candidates.len().saturating_sub(1));
    let (coords, path, _, _) = candidates.swap_remove(idx);
    Some((coords, path))
}

fn nearest_hypertiles_4(here: (i32, i32)) -> [ChunkCoord; 4] {
    let (center, local) = world_to_chunk_local(here.0, here.1);
    let mid = HYPERMAP_CHUNK_SIZE / 2;
    let dx = if local.x >= mid { 1 } else { -1 };
    let dy = if local.y >= mid { 1 } else { -1 };
    [
        center,
        ChunkCoord::new(center.x + dx, center.y),
        ChunkCoord::new(center.x, center.y + dy),
        ChunkCoord::new(center.x + dx, center.y + dy),
    ]
}

/// `true` if the charger at `coords` has no occupant or is occupied by `me`.
fn charger_free_for(
    map: &InteractiveEntityMap,
    coords: EntityCoordinates,
    me: bevy::prelude::Entity,
) -> bool {
    map.entities_at(coords)
        .iter()
        .filter_map(|e| e.entity.as_charger())
        .next()
        .map(|c| c.occupant().is_none_or(|o| o == me))
        .unwrap_or(false)
}

fn in_waiting_zone(ctx: &BrainContext, coords: EntityCoordinates) -> bool {
    let dx = (ctx.main_tile.x - coords.x).abs();
    let dy = (ctx.main_tile.y - coords.y).abs();
    dx + dy <= WAITING_QUEUE_ENTER_DISTANCE
}

fn dock_allowed_for(
    ctx: &BrainContext,
    queued_waiting: Option<EntityCoordinates>,
    coords: EntityCoordinates,
) -> bool {
    if !charger_free_for(ctx.interactive, coords, ctx.entity) {
        return false;
    }
    if queued_waiting == Some(coords) {
        return ctx.interactive.is_waiting_front(coords, ctx.entity);
    }
    true
}

fn short_wait_recheck_s(rng: &mut StdRng) -> f32 {
    rng::range(rng, WAITING_RECHECK_MIN_S..WAITING_RECHECK_MAX_S)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::brain::low_level::{FollowTuning, Idle};
    use crate::actor::brain::memory::BotMemory;
    use crate::map::interactive_entity::{ChargerEntity, InteractiveEntity};
    use crate::map::pathfind_service::{PathfindQueue, PathfindResults};
    use crate::map::world_map::ChargerFacing;
    use bevy::math::{IVec2, Vec2};
    use bevy::prelude::Entity;

    struct StuckLowAction;

    struct VelocityFinished(Vec2);

    impl LowLevelAction for VelocityFinished {
        fn kind(&self) -> LowLevelKind {
            LowLevelKind::Idle
        }
        fn execute(
            &mut self,
            _state: &mut crate::actor::ActorState,
            _ctx: &BrainContext,
            _rng: &mut StdRng,
            _tuning: &FollowTuning,
        ) {
        }
        fn is_finished(&self) -> bool {
            true
        }
        fn velocity(&self) -> Vec2 {
            self.0
        }
        fn label(&self) -> String {
            "VelocityFinished".to_string()
        }
    }

    impl LowLevelAction for StuckLowAction {
        fn kind(&self) -> LowLevelKind {
            LowLevelKind::Idle
        }
        fn execute(
            &mut self,
            _state: &mut crate::actor::ActorState,
            _ctx: &BrainContext,
            _rng: &mut StdRng,
            _tuning: &FollowTuning,
        ) {
        }
        fn is_finished(&self) -> bool {
            false
        }
        fn is_stuck(&self) -> bool {
            true
        }
        fn label(&self) -> String {
            "StuckLowAction".to_string()
        }
    }

    struct PathfindFixture {
        queue: PathfindQueue,
        results: PathfindResults,
    }

    impl PathfindFixture {
        fn new() -> Self {
            Self {
                queue: PathfindQueue::default(),
                results: PathfindResults::default(),
            }
        }

        fn access(&self) -> PathfindAccess<'_> {
            PathfindAccess {
                queue: &self.queue,
                results: &self.results,
            }
        }

        fn drain_world_routes(&self) -> Vec<((i32, i32), (i32, i32))> {
            self.queue
                .drain_pending()
                .into_iter()
                .filter_map(|(_, kind)| match kind {
                    PathKind::WorldRoute { start, goal, .. } => Some((start, goal)),
                    _ => None,
                })
                .collect()
        }

        fn resolve_all_routes(&self, path: Vec<(i32, i32)>, raw_len: usize) {
            for (id, kind) in self.queue.drain_pending() {
                let outcome = match kind {
                    PathKind::WorldRoute { goal, .. } => PathOutcome::Route {
                        path: path.clone(),
                        raw_len,
                    }
                    .with_goal(goal),
                    other => panic!("unexpected queued kind in brain test: {other:?}"),
                };
                self.results.insert_for_test(id, outcome);
            }
        }

        fn resolve_routes_with_costs(
            &self,
            path: Vec<(i32, i32)>,
            costs: &[((i32, i32), usize)],
        ) {
            for (id, kind) in self.queue.drain_pending() {
                let PathKind::WorldRoute { goal, .. } = kind else {
                    panic!("unexpected queued kind in brain test");
                };
                let raw_len = costs
                    .iter()
                    .find(|(tile, _)| *tile == goal)
                    .map(|(_, cost)| *cost)
                    .unwrap_or(1);
                self.results.insert_for_test(
                    id,
                    PathOutcome::Route {
                        path: path.clone(),
                        raw_len,
                    }
                    .with_goal(goal),
                );
            }
        }
    }

    trait RouteOutcomeExt {
        fn with_goal(self, goal: (i32, i32)) -> Self;
    }

    impl RouteOutcomeExt for PathOutcome {
        fn with_goal(self, goal: (i32, i32)) -> Self {
            match self {
                PathOutcome::Route { mut path, raw_len } => {
                    if path.is_empty() {
                        path.push(goal);
                    } else {
                        *path.last_mut().expect("route path") = goal;
                    }
                    PathOutcome::Route { path, raw_len }
                }
                other => other,
            }
        }
    }

    fn ctx_with_dt<'a>(
        passability: &'a Hypermap<f32>,
        interactive: &'a InteractiveEntityMap,
        charge: f32,
        tile: (i32, i32),
        pf: PathfindAccess<'a>,
        patrol_loop: Option<&'a [(i32, i32)]>,
        dt: f32,
    ) -> BrainContext<'a> {
        BrainContext {
            entity: Entity::PLACEHOLDER,
            dt,
            center: Vec2::new(tile.0 as f32 + 0.5, tile.1 as f32 + 0.5),
            radius_subtiles: 2,
            main_tile: IVec2::new(tile.0, tile.1),
            main_tile_changed: true,
            floor: 0,
            charge,
            missing_charge_pct: (1.0 - charge) * 100.0,
            depleted: charge <= 0.0,
            broken: false,
            passability,
            dirt: None,
            interactive,
            avoidance: None,
            on_screen: true,
            trace: None,
            patrol_loop,
            pathfind: Some(pf),
            fixer: None,
            dynamic_repath: false,
            neighbors: None,
        }
    }

    fn ctx<'a>(
        passability: &'a Hypermap<f32>,
        interactive: &'a InteractiveEntityMap,
        charge: f32,
        tile: (i32, i32),
        pf: PathfindAccess<'a>,
        patrol_loop: Option<&'a [(i32, i32)]>,
    ) -> BrainContext<'a> {
        ctx_with_dt(passability, interactive, charge, tile, pf, patrol_loop, 1.0 / 60.0)
    }

    struct FollowPathNeverDone;

    impl LowLevelAction for FollowPathNeverDone {
        fn kind(&self) -> LowLevelKind {
            LowLevelKind::FollowPath
        }
        fn execute(
            &mut self,
            _state: &mut crate::actor::ActorState,
            _ctx: &BrainContext,
            _rng: &mut StdRng,
            _tuning: &FollowTuning,
        ) {
        }
        fn is_finished(&self) -> bool {
            false
        }
        fn label(&self) -> String {
            "FollowPath".to_string()
        }
    }

    fn is_pending(low: &dyn LowLevelAction) -> bool {
        low.kind() == LowLevelKind::PendingPath
    }

    #[test]
    fn replan_rising_edge_only_on_transition() {
        let stuck = StuckLowAction;
        assert!(low_level_needs_replan(&stuck, false, false));
        assert!(!low_level_needs_replan(&stuck, true, false));
        assert!(!low_level_needs_replan(&stuck, true, true));
    }

    #[test]
    fn random_walker_enqueues_world_route_on_replan() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let pf = PathfindFixture::new();
        let mut action = GoToRandomPoints::default();
        let mut low: Box<dyn LowLevelAction> = Box::new(StuckLowAction);
        let mut rng = rng::seeded(42);

        let out = action.update(
            &ctx(&passability, &interactive, 1.0, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );

        assert!(matches!(out.status, HighLevelStatus::Running));
        assert!(is_pending(low.as_ref()), "stuck walker must park while awaiting a route");
        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].0, (0, 0));
    }

    #[test]
    fn random_walker_pending_path_inherits_velocity() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let pf = PathfindFixture::new();
        let mut action = GoToRandomPoints::default();
        let mut low: Box<dyn LowLevelAction> = Box::new(FollowPath::new(vec![(5, 0)]));
        let mut rng = rng::seeded(42);
        let tuning = FollowTuning::default();
        let mut state = crate::actor::brain::test_support::test_state();
        for _ in 0..5 {
            low.execute(
                &mut state,
                &ctx(&passability, &interactive, 1.0, (0, 0), pf.access(), None),
                &mut rng,
                &tuning,
            );
            state.center += state.move_buffer.tile_delta;
        }
        let inherited = low.velocity();
        assert!(inherited.length_squared() > 0.0, "FollowPath should have built momentum");
        low = Box::new(VelocityFinished(inherited));

        action.update(
            &ctx(&passability, &interactive, 1.0, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        assert!(is_pending(low.as_ref()));
        assert_eq!(low.velocity(), inherited, "PendingPath must inherit prior velocity");
    }

    #[test]
    fn no_charger_waits_instead_of_requesting() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        passability.set(0, 0, 1.0);
        let interactive = InteractiveEntityMap::new();
        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        let out = action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        assert!(matches!(out.status, HighLevelStatus::Running));
        assert!(!is_pending(low.as_ref()), "no candidates → retry Wait, not PendingPath");
        assert!(pf.queue.is_empty(), "no charger → must not enqueue pathfind requests");
        assert!(!low.is_finished(), "the retry Wait keeps the action alive");
    }

    #[test]
    fn charge_seek_enqueues_route_per_candidate() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..=8 {
            passability.set(x, 0, 1.0);
        }
        let mut interactive = InteractiveEntityMap::new();
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::ground(2, 0),
            ChargerFacing::North,
        )));
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::ground(6, 0),
            ChargerFacing::North,
        )));

        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );

        assert!(is_pending(low.as_ref()));
        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 2);
        assert!(routes.iter().all(|(start, _)| *start == (0, 0)));
        let goals: Vec<(i32, i32)> = routes.into_iter().map(|(_, goal)| goal).collect();
        assert!(goals.contains(&(2, 0)));
        assert!(goals.contains(&(6, 0)));
    }

    #[test]
    fn recharge_search_uses_nearest_four_hypertiles() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in -2..=0 {
            passability.set(x, 0, 1.0);
        }
        for x in 0..=130 {
            passability.set(x, 1, 1.0);
        }

        let mut interactive = InteractiveEntityMap::new();
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::ground(-2, 0),
            ChargerFacing::North,
        )));
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::ground(130, 1),
            ChargerFacing::North,
        )));

        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );

        let goals: Vec<(i32, i32)> = pf
            .drain_world_routes()
            .into_iter()
            .map(|(_, goal)| goal)
            .collect();
        assert_eq!(goals, vec![(-2, 0)], "far-chunk charger must be excluded from seek requests");
    }

    #[test]
    fn recharge_prefers_station_with_less_than_two_waiters() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..=8 {
            passability.set(x, 0, 1.0);
        }
        let mut interactive = InteractiveEntityMap::new();
        let close = EntityCoordinates::ground(2, 0);
        let farther = EntityCoordinates::ground(6, 0);
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(close, ChargerFacing::North)));
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(farther, ChargerFacing::North)));
        interactive.add_waiting(close, Entity::from_bits(100));
        interactive.add_waiting(close, Entity::from_bits(101));
        interactive.add_waiting(farther, Entity::from_bits(200));

        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        pf.resolve_all_routes(vec![(0, 0), (6, 0)], 2);
        let out = action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );

        assert!(matches!(out.status, HighLevelStatus::Running));
        assert_eq!(out.effects.queue_want, Some(farther));
        let (path, _) = low.route().expect("ranking should install a route after injected results");
        assert_eq!(path.last().map(|n| n.tile()), Some((6, 0)));
    }

    #[test]
    fn recharge_uses_ranked_fallback_when_all_waiting_queues_are_long() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..=9 {
            passability.set(x, 0, 1.0);
        }
        let mut interactive = InteractiveEntityMap::new();
        let first = EntityCoordinates::ground(2, 0);
        let second = EntityCoordinates::ground(4, 0);
        let third = EntityCoordinates::ground(8, 0);
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(first, ChargerFacing::North)));
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(second, ChargerFacing::North)));
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(third, ChargerFacing::North)));

        for (coords, base) in [(first, 10u64), (second, 20u64), (third, 30u64)] {
            interactive.add_waiting(coords, Entity::from_bits(base));
            interactive.add_waiting(coords, Entity::from_bits(base + 1));
        }

        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        pf.resolve_routes_with_costs(
            vec![(0, 0), (8, 0)],
            &[((2, 0), 2), ((4, 0), 4), ((8, 0), 8)],
        );
        let out = action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );

        assert!(matches!(out.status, HighLevelStatus::Running));
        let (path, _) = low.route().expect("ranked fallback should install a route");
        assert_eq!(path.last().map(|n| n.tile()), Some((4, 0)));
    }

    #[test]
    fn recharge_stuck_handler_reenqueues_seek() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..=6 {
            passability.set(x, 0, 1.0);
        }
        let mut interactive = InteractiveEntityMap::new();
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::ground(6, 0),
            ChargerFacing::North,
        )));

        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(StuckLowAction);
        let mut rng = rng::seeded(1);

        action.update(
            &ctx(&passability, &interactive, 0.2, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );

        assert!(is_pending(low.as_ref()));
        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0], ((0, 0), (6, 0)));
    }

    /// Drives the action through seek → resolve → commit to the nearer charger,
    /// returning the two charger coordinates so a reroute test can assert which
    /// station the bot bailed to.
    fn commit_to_close_charger(
        passability: &Hypermap<f32>,
        interactive: &InteractiveEntityMap,
        pf: &PathfindFixture,
        action: &mut GoToChargeStation,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
        close: EntityCoordinates,
    ) {
        action.update(&ctx(passability, interactive, 0.1, (0, 0), pf.access(), None), low, rng);
        pf.resolve_routes_with_costs(vec![(0, 0), (2, 0)], &[((2, 0), 2), ((6, 0), 6)]);
        let out = action.update(&ctx(passability, interactive, 0.1, (0, 0), pf.access(), None), low, rng);
        assert_eq!(out.effects.queue_want, Some(close), "commits to the nearer charger");
    }

    fn two_charger_world() -> (Hypermap<f32>, InteractiveEntityMap, EntityCoordinates, EntityCoordinates) {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..=8 {
            passability.set(x, 0, 1.0);
        }
        let mut interactive = InteractiveEntityMap::new();
        let close = EntityCoordinates::ground(2, 0);
        let far = EntityCoordinates::ground(6, 0);
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(close, ChargerFacing::North)));
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(far, ChargerFacing::North)));
        (passability, interactive, close, far)
    }

    #[test]
    fn recharge_reroutes_when_chosen_queue_overcrowded() {
        let (passability, mut interactive, close, _far) = two_charger_world();
        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        commit_to_close_charger(&passability, &interactive, &pf, &mut action, &mut low, &mut rng, close);

        // Three *other* bots pile into the close charger's waiting queue.
        interactive.add_waiting(close, Entity::from_bits(100));
        interactive.add_waiting(close, Entity::from_bits(101));
        interactive.add_waiting(close, Entity::from_bits(102));

        let out = action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        assert_eq!(out.effects.queue_unwant, Some(close), "overcrowded charger is released");
        assert!(is_pending(low.as_ref()), "reroute parks the bot while re-seeking");
        let goals: Vec<(i32, i32)> = pf.drain_world_routes().into_iter().map(|(_, g)| g).collect();
        assert_eq!(goals, vec![(6, 0)], "reroute must skip the overcrowded charger");
    }

    #[test]
    fn recharge_keeps_charger_with_two_or_fewer_others() {
        let (passability, mut interactive, close, _far) = two_charger_world();
        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        commit_to_close_charger(&passability, &interactive, &pf, &mut action, &mut low, &mut rng, close);

        // Two others is at the limit, not over it — the bot stays committed.
        interactive.add_waiting(close, Entity::from_bits(100));
        interactive.add_waiting(close, Entity::from_bits(101));

        let out = action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        assert_eq!(out.effects.queue_unwant, None, "two waiters must not trigger a reroute");
        assert!(pf.queue.is_empty(), "no reroute → no fresh seek requests");
    }

    #[test]
    fn recharge_stuck_reroutes_to_different_charger() {
        let (passability, interactive, close, _far) = two_charger_world();
        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        commit_to_close_charger(&passability, &interactive, &pf, &mut action, &mut low, &mut rng, close);

        // The bot wedges en route to the close charger.
        low = Box::new(StuckLowAction);
        let out = action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        assert_eq!(out.effects.queue_unwant, Some(close), "stuck releases the committed charger");
        let goals: Vec<(i32, i32)> = pf.drain_world_routes().into_iter().map(|(_, g)| g).collect();
        assert_eq!(goals, vec![(6, 0)], "stuck reroute must pick a different charger");
    }

    #[test]
    fn traveling_does_not_restop_each_tile_inside_waiting_zone() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..=10 {
            passability.set(x, 0, 1.0);
        }
        let charger = EntityCoordinates::ground(10, 0);
        let mut interactive = InteractiveEntityMap::new();
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            charger,
            ChargerFacing::North,
        )));

        let pf = PathfindFixture::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        pf.resolve_all_routes(vec![(0, 0), (10, 0)], 11);
        action.update(
            &ctx(&passability, &interactive, 0.1, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        assert!(low.route().is_some(), "seek should install a route after injected results");

        let out = action.update(
            &ctx(&passability, &interactive, 0.1, (7, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        assert_eq!(out.effects.queue_wait, Some(charger), "joins waiting queue once");
        interactive.add_waiting(charger, Entity::PLACEHOLDER);

        low = Box::new(Idle);
        action.update(
            &ctx(&passability, &interactive, 0.1, (7, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        let dock_pending = pf.queue.drain_pending();
        assert_eq!(dock_pending.len(), 1, "cleared to approach must enqueue one dock route");
        let PathKind::WorldRoute {
            start,
            goal,
            ..
        } = &dock_pending[0].1
        else {
            panic!("expected a dock WorldRoute request");
        };
        assert_eq!((*start, *goal), ((7, 0), (10, 0)));

        for (id, kind) in dock_pending {
            let PathKind::WorldRoute { goal, .. } = kind else {
                panic!("unexpected queued kind");
            };
            pf.results.insert_for_test(
                id,
                PathOutcome::Route {
                    path: vec![(7, 0), goal],
                    raw_len: 4,
                },
            );
        }
        action.update(
            &ctx(&passability, &interactive, 0.1, (7, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        assert!(low.route().is_some(), "approach route should be installed");

        for tile in [(8, 0), (9, 0)] {
            let out = action.update(
                &ctx(&passability, &interactive, 0.1, tile, pf.access(), None),
                &mut low,
                &mut rng,
            );
            assert_eq!(out.effects.queue_wait, None, "must not re-join waiting queue at {tile:?}");
            assert!(pf.queue.is_empty(), "must not enqueue more routes while crossing the zone");
        }
    }

    #[test]
    fn enqueue_patrol_candidates_issues_reachability_requests() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let queue = PathfindQueue::default();
        let checks = enqueue_patrol_candidates(&mut rng::seeded(7), (0, 0), &passability, &queue, Entity::PLACEHOLDER);
        assert!(!checks.is_empty(), "open map should sample patrol candidates");
        let routes = queue.drain_pending();
        assert_eq!(routes.len(), checks.len());
        for ((id, kind), (check_id, tile)) in routes.iter().zip(checks.iter()) {
            assert_eq!(*id, *check_id);
            assert!(matches!(
                kind,
                PathKind::WorldRoute {
                    start: (0, 0),
                    goal,
                    ..
                } if *goal == *tile
            ));
        }
    }

    #[test]
    fn assemble_patrol_loop_keeps_reachable_distinct_tiles() {
        let resolved = [
            ((1, 0), true),
            ((2, 0), true),
            ((2, 0), true),
            ((3, 0), false),
            ((4, 0), true),
        ];
        let loop_tiles = assemble_patrol_loop(&resolved);
        assert_eq!(loop_tiles, vec![(1, 0), (2, 0), (4, 0)]);
    }

    #[test]
    fn patrol_enqueues_route_to_next_waypoint() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let route = [(0, 0), (4, 0), (4, 4)];
        let pf = PathfindFixture::new();
        let mut action = GoToPatrol::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        let out = action.update(
            &ctx(&passability, &interactive, 1.0, (0, 0), pf.access(), Some(&route)),
            &mut low,
            &mut rng,
        );
        assert!(matches!(out.status, HighLevelStatus::Running));
        assert!(is_pending(low.as_ref()));
        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0], ((0, 0), (4, 0)), "must skip the tile the bot stands on");
    }

    #[test]
    fn wander_times_out_and_picks_new_destination() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let pf = PathfindFixture::new();
        let mut action = GoToRandomPoints::default();
        let mut low: Box<dyn LowLevelAction> = Box::new(StuckLowAction);
        let mut rng = rng::seeded(42);

        action.update(
            &ctx(&passability, &interactive, 1.0, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        let pending = pf.queue.drain_pending();
        assert_eq!(pending.len(), 1);
        let (id, kind) = &pending[0];
        let PathKind::WorldRoute { start, goal, .. } = kind else {
            panic!("expected wander WorldRoute");
        };
        let start = *start;
        let goal = *goal;
        assert_eq!(start, (0, 0));
        pf.results.insert_for_test(
            *id,
            PathOutcome::Route {
                path: vec![start, goal],
                raw_len: 2,
            },
        );
        action.update(
            &ctx(&passability, &interactive, 1.0, (0, 0), pf.access(), None),
            &mut low,
            &mut rng,
        );
        assert_eq!(low.label(), "FollowPath");

        let timeout = leg_timeout_secs(start, goal) + 0.1;
        let out = action.update(
            &ctx_with_dt(
                &passability,
                &interactive,
                1.0,
                (0, 0),
                pf.access(),
                None,
                timeout,
            ),
            &mut low,
            &mut rng,
        );
        assert_eq!(
            out.effects.log,
            Some(BrainLogEvent::WanderDestinationTimedOut { goal })
        );
        assert!(is_pending(low.as_ref()), "timed-out wander must enqueue a fresh route");
        assert_eq!(pf.drain_world_routes().len(), 1);
    }

    #[test]
    fn patrol_times_out_and_skips_waypoint() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let route = [(0, 0), (4, 0), (4, 4)];
        let pf = PathfindFixture::new();
        let mut action = GoToPatrol::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        action.update(
            &ctx(&passability, &interactive, 1.0, (0, 0), pf.access(), Some(&route)),
            &mut low,
            &mut rng,
        );
        let pending = pf.queue.drain_pending();
        assert_eq!(pending.len(), 1);
        let (id, kind) = &pending[0];
        let PathKind::WorldRoute { start, goal, .. } = kind else {
            panic!("expected patrol WorldRoute");
        };
        assert_eq!((*start, *goal), ((0, 0), (4, 0)));
        pf.results.insert_for_test(
            *id,
            PathOutcome::Route {
                path: vec![*start, *goal],
                raw_len: 2,
            },
        );
        action.update(
            &ctx(&passability, &interactive, 1.0, (0, 0), pf.access(), Some(&route)),
            &mut low,
            &mut rng,
        );
        low = Box::new(FollowPathNeverDone);

        let timeout = leg_timeout_secs((0, 0), (4, 0)) + 0.1;
        let out = action.update(
            &ctx_with_dt(
                &passability,
                &interactive,
                1.0,
                (0, 0),
                pf.access(),
                Some(&route),
                timeout,
            ),
            &mut low,
            &mut rng,
        );
        assert_eq!(
            out.effects.log,
            Some(BrainLogEvent::PatrolWaypointSkipped { waypoint: (4, 0) })
        );
        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0], ((0, 0), (4, 4)), "must skip to the next loop waypoint");
    }

    #[test]
    fn patrol_resumes_at_nearest_waypoint_on_creation() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let route = [(0, 0), (10, 0), (20, 0)];
        let pf = PathfindFixture::new();
        let mut action = GoToPatrol::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(0);

        action.update(
            &ctx(&passability, &interactive, 1.0, (9, 0), pf.access(), Some(&route)),
            &mut low,
            &mut rng,
        );
        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0], ((9, 0), (10, 0)), "resumes at the nearest loop waypoint");
    }

    // --- GoFixBots ----------------------------------------------------------

    use crate::actor::dispatch::{DispatchQueue, RepairPart};

    /// A `BrainContext` with the fixer bundle filled in.
    fn fixer_ctx<'a>(
        passability: &'a Hypermap<f32>,
        interactive: &'a InteractiveEntityMap,
        tile: (i32, i32),
        pf: PathfindAccess<'a>,
        dispatch: &'a DispatchQueue,
        home: Option<EntityCoordinates>,
        carried: Option<RepairPart>,
    ) -> BrainContext<'a> {
        let mut c = ctx(passability, interactive, 1.0, tile, pf, None);
        c.fixer = Some(FixerContext {
            dispatch,
            home_depot: home,
            carried,
            ignored_unreachable: &[],
        });
        c
    }

    #[test]
    fn fixer_loiter_outside_radius_does_not_route_to_depot() {
        // With no open tasks a fixer outside the loiter zone should wander toward
        // a tile within the zone — not path to the depot tile itself first.
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(12);
        let home = EntityCoordinates::ground(0, 0);

        let c = fixer_ctx(
            &passability,
            &interactive,
            (20, 0),
            pf.access(),
            &dispatch,
            Some(home),
            None,
        );
        action.update(&c, &mut low, &mut rng);

        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 1);
        let (_, goal) = routes[0];
        assert_ne!(goal, (0, 0), "loiter wander must not target the depot tile");
        assert!(
            manhattan(goal, (0, 0)) <= FIXER_LOITER_RADIUS as u32,
            "goal {goal:?} should lie within the loiter radius of the depot"
        );
    }

    #[test]
    fn fixer_without_home_waits_and_enqueues_nothing() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(1);

        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, None, None);
        action.update(&c, &mut low, &mut rng);

        assert!(!is_pending(low.as_ref()), "no home depot → hold, don't route");
        assert!(pf.queue.is_empty(), "no routes until a home depot exists");
    }

    #[test]
    fn fixer_claims_open_request_and_routes_to_depot() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let broken = Entity::from_bits(0xB0B);
        dispatch.post(broken, RepairPart::MovementEngine, IVec2::new(3, 4));

        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(2);
        let home = EntityCoordinates::ground(0, 0);

        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        action.update(&c, &mut low, &mut rng);

        // The request is now claimed by this fixer, and a route to the depot is queued.
        let claim = dispatch.claim_of(Entity::PLACEHOLDER).expect("claimed");
        assert_eq!(claim.broken_bot, broken);
        assert!(is_pending(low.as_ref()), "fixer parks while routing to the depot");
        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0], ((0, 0), (0, 0)), "routes to the home depot tile");
    }

    #[test]
    fn fixer_full_flow_picks_up_part_then_repairs_on_contact() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let broken = Entity::from_bits(0xDEAD);
        dispatch.post(broken, RepairPart::ControlPlane, IVec2::new(3, 3));

        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(3);
        let home = EntityCoordinates::ground(0, 0);

        // Tick 1 (loiter @ home): claim + route to depot.
        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        action.update(&c, &mut low, &mut rng);
        pf.resolve_all_routes(vec![(0, 0)], 1); // already on the depot tile

        // Tick 2 (fetch): poll resolves AtGoal → pick up the part, route to the bot.
        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        let out = action.update(&c, &mut low, &mut rng);
        assert_eq!(out.effects.pickup_part, Some(RepairPart::ControlPlane), "part picked up at depot");
        let routes = pf.drain_world_routes();
        assert!(routes.iter().any(|(_, goal)| *goal == (3, 3)), "routes toward the stranded bot");

        // Tick 3 (deliver): standing next to the bot → repair on proximity.
        let c = fixer_ctx(
            &passability,
            &interactive,
            (3, 3),
            pf.access(),
            &dispatch,
            Some(home),
            Some(RepairPart::ControlPlane),
        );
        let out = action.update(&c, &mut low, &mut rng);
        assert_eq!(
            out.effects.repair_target,
            Some((broken, RepairPart::ControlPlane)),
            "repairs the stranded bot's part"
        );
        assert!(out.effects.clear_inventory, "carried part is consumed");
        assert_eq!(
            out.effects.integer_memory_write,
            Some((IntegerMemoryId::HelpFailuresCount, 0)),
            "a successful delivery clears the help-failure counter"
        );
        assert!(dispatch.is_empty(), "completed request leaves the board");
    }

    #[test]
    fn fixer_full_flow_delivers_battery_and_recharges_on_contact() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let discharged = Entity::from_bits(0xBA77);
        dispatch.post(discharged, RepairPart::Battery, IVec2::new(3, 3));

        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(3);
        let home = EntityCoordinates::ground(0, 0);

        // Tick 1 (loiter @ home): claim + route to depot.
        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        action.update(&c, &mut low, &mut rng);
        pf.resolve_all_routes(vec![(0, 0)], 1); // already on the depot tile

        // Tick 2 (fetch): poll resolves AtGoal → pick up the battery, route to the bot.
        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        let out = action.update(&c, &mut low, &mut rng);
        assert_eq!(out.effects.pickup_part, Some(RepairPart::Battery), "battery picked up at depot");

        // Tick 3 (deliver): standing next to the discharged bot → recharge on proximity.
        let c = fixer_ctx(
            &passability,
            &interactive,
            (3, 3),
            pf.access(),
            &dispatch,
            Some(home),
            Some(RepairPart::Battery),
        );
        let out = action.update(&c, &mut low, &mut rng);
        assert!(out.effects.repair_target.is_none(), "a battery is not a part repair");
        let (target, level) = out.effects.recharge_target.expect("recharges the discharged bot");
        assert_eq!(target, discharged);
        assert!(
            (BATTERY_RECHARGE_MIN..=BATTERY_RECHARGE_MAX).contains(&level),
            "recharge level {level} within [{BATTERY_RECHARGE_MIN}, {BATTERY_RECHARGE_MAX}]"
        );
        assert!(out.effects.clear_inventory, "carried battery is consumed");
        assert!(dispatch.is_empty(), "completed request leaves the board");
    }

    #[test]
    fn fixer_skips_unreachable_task_at_claim_and_remembers_location() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..=5 {
            passability.set(x, 0, 1.0);
        }
        // Wall blocks the far side from (0,0).
        passability.set(6, 0, 0.0);

        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let broken = Entity::from_bits(0xB0B);
        dispatch.post(broken, RepairPart::MovementEngine, IVec2::new(6, 0));

        let mut memory = BotMemory::default();
        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(21);
        let home = EntityCoordinates::ground(0, 0);

        let mut c = fixer_ctx(
            &passability,
            &interactive,
            (0, 0),
            pf.access(),
            &dispatch,
            Some(home),
            None,
        );
        c.fixer = Some(FixerContext {
            dispatch: &dispatch,
            home_depot: Some(home),
            carried: None,
            ignored_unreachable: &[],
        });

        let out = action.update(&c, &mut low, &mut rng);
        assert!(dispatch.claim_of(Entity::PLACEHOLDER).is_none(), "must not claim unreachable task");
        assert_eq!(out.effects.remember_unreachable_len, 1);
        assert_eq!(out.effects.remember_unreachable[0], IVec2::new(6, 0));
        assert_eq!(
            out.effects.log,
            Some(BrainLogEvent::FixerTargetUnreachable { target: (6, 0) })
        );

        memory.remember_unreachable_fixer_task(out.effects.remember_unreachable[0]);
        let ignored = memory
            .unreachable_fixer_tasks()
            .map(|tasks| tasks.points.as_slice())
            .unwrap_or(&[]);
        let mut c2 = fixer_ctx(
            &passability,
            &interactive,
            (0, 0),
            pf.access(),
            &dispatch,
            Some(home),
            None,
        );
        c2.fixer = Some(FixerContext {
            dispatch: &dispatch,
            home_depot: Some(home),
            carried: None,
            ignored_unreachable: ignored,
        });
        let out2 = action.update(&c2, &mut low, &mut rng);
        assert!(out2.effects.remember_unreachable_len == 0, "blacklisted task is not retried");
        assert!(dispatch.claim_of(Entity::PLACEHOLDER).is_none());
    }

    #[test]
    fn fixer_logs_error_when_stranded_bot_is_unreachable() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let broken = Entity::from_bits(0xB0B);
        let target = (5, 5);
        dispatch.post(broken, RepairPart::MovementEngine, IVec2::new(target.0, target.1));

        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(13);
        let home = EntityCoordinates::ground(0, 0);

        // Claim + route to depot.
        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        action.update(&c, &mut low, &mut rng);
        pf.resolve_all_routes(vec![(0, 0)], 1);

        // Pick up at depot + route toward the stranded bot.
        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        action.update(&c, &mut low, &mut rng);
        let deliver_pending = pf.queue.drain_pending();
        assert_eq!(deliver_pending.len(), 1);
        let (deliver_id, _) = deliver_pending[0];
        pf.results.insert_for_test(deliver_id, PathOutcome::NoPath);

        // Deliver route fails → abandon the claim and log the unreachable target.
        let c = fixer_ctx(
            &passability,
            &interactive,
            (0, 0),
            pf.access(),
            &dispatch,
            Some(home),
            Some(RepairPart::MovementEngine),
        );
        let out = action.update(&c, &mut low, &mut rng);
        assert_eq!(
            out.effects.log,
            Some(BrainLogEvent::FixerTargetUnreachable { target })
        );
        assert!(dispatch.claim_of(Entity::PLACEHOLDER).is_none(), "claim released");
        assert!(!dispatch.has_open_within(IVec2::ZERO, 100), "task on give-up cooldown");
    }

    #[test]
    fn fixer_preempt_keeps_claim_and_inventory() {
        // A recharge pre-emption must NOT abandon the task: the claim and any
        // carried part survive so the fixer resumes after topping up. The claim is
        // recovered on the next tick via `claim_of`.
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let broken = Entity::from_bits(7);
        dispatch.post(broken, RepairPart::MovementEngine, IVec2::new(2, 2));

        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(4);
        let home = EntityCoordinates::ground(0, 0);

        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        action.update(&c, &mut low, &mut rng);
        assert!(dispatch.claim_of(Entity::PLACEHOLDER).is_some(), "claimed before pre-empt");

        let effects = action.preempt(&c);
        assert!(!effects.clear_inventory, "pre-empt keeps the carried part (permanent)");
        assert!(
            dispatch.claim_of(Entity::PLACEHOLDER).is_some(),
            "pre-empt keeps the claim so the fixer resumes the task after recharging"
        );
    }

    #[test]
    fn fixer_clears_help_failures_on_fresh_claim() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        dispatch.post(Entity::from_bits(0xC0), RepairPart::MovementEngine, IVec2::new(3, 4));

        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(7);
        let home = EntityCoordinates::ground(0, 0);

        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        let out = action.update(&c, &mut low, &mut rng);
        assert_eq!(
            out.effects.integer_memory_write,
            Some((IntegerMemoryId::HelpFailuresCount, 0)),
            "claiming a fresh task clears the per-task help-failure counter"
        );
    }

    #[test]
    fn fixer_returns_carried_part_to_depot_when_unclaimed() {
        // No claim but still carrying a part (gave up after too many help
        // failures, or abandoned an unreachable target): route to the depot and
        // drop it there rather than haul it around forever.
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = rng::seeded(11);
        let home = EntityCoordinates::ground(0, 0);

        // Tick 1 @ (5,5): enter DropPart, route toward the (home) depot.
        let c = fixer_ctx(
            &passability,
            &interactive,
            (5, 5),
            pf.access(),
            &dispatch,
            Some(home),
            Some(RepairPart::MovementEngine),
        );
        action.update(&c, &mut low, &mut rng);
        assert_eq!(action.label(), "GoFixBots (drop part)");
        assert_eq!(
            pf.drain_world_routes(),
            vec![((5, 5), (0, 0))],
            "routes to the depot to drop the part",
        );

        // Tick 2 @ (0,0): standing on the depot → drop the part.
        let c = fixer_ctx(
            &passability,
            &interactive,
            (0, 0),
            pf.access(),
            &dispatch,
            Some(home),
            Some(RepairPart::MovementEngine),
        );
        let out = action.update(&c, &mut low, &mut rng);
        assert!(out.effects.clear_inventory, "the carried part is returned to the depot");
    }

    #[test]
    fn fixer_recovers_from_orphaned_pending_path() {
        // Regression: a fixer left holding a PendingPath with no in-flight request
        // (e.g. a loiter route failed/timed out, or a phase transition handed the
        // next phase a stale PendingPath) used to coast forever — PendingPath never
        // reports stuck/finished, so the replan gate could never fire. The update
        // safety net must swap it out so the bot recovers.
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let dispatch = DispatchQueue::default();
        let pf = PathfindFixture::new();
        let mut action = GoFixBots::new(); // starts in Loiter, nothing awaiting
        let mut low: Box<dyn LowLevelAction> = Box::new(PendingPath::new());
        let mut rng = rng::seeded(9);
        let home = EntityCoordinates::ground(0, 0);

        let c = fixer_ctx(&passability, &interactive, (0, 0), pf.access(), &dispatch, Some(home), None);
        action.update(&c, &mut low, &mut rng);

        assert!(
            !is_pending(low.as_ref()),
            "an orphaned PendingPath must be recovered, not coasted forever"
        );
    }

    // --- GoClean (cleaner) -------------------------------------------------

    /// A `BrainContext` for a cleaner: like [`ctx`] but with a dirt field wired in.
    fn cleaning_ctx<'a>(
        passability: &'a Hypermap<f32>,
        dirt: &'a Hypermap<f32>,
        interactive: &'a InteractiveEntityMap,
        tile: (i32, i32),
        pf: PathfindAccess<'a>,
    ) -> BrainContext<'a> {
        let mut c = ctx(passability, interactive, 1.0, tile, pf, None);
        c.dirt = Some(dirt);
        c
    }

    #[test]
    fn cleaner_routes_to_dirtiest_cell_when_dirty() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let dirt: Hypermap<f32> = Hypermap::new(0.0);
        // One clearly-dirtiest cell within scan radius, above threshold.
        dirt.set(3, 1, 0.4);
        dirt.set(2, 0, 0.9); // the dirtiest
        let interactive = InteractiveEntityMap::new();
        let pf = PathfindFixture::new();
        let mut action = GoClean::default();
        let mut low: Box<dyn LowLevelAction> = Box::new(StuckLowAction);
        let mut rng = rng::seeded(7);

        let out = action.update(
            &cleaning_ctx(&passability, &dirt, &interactive, (0, 0), pf.access()),
            &mut low,
            &mut rng,
        );
        // While awaiting the route it isn't yet driving the cleaning leg.
        assert!(matches!(out.status, HighLevelStatus::Running));
        assert_eq!(out.effects.set_cleaning, Some(false));
        assert!(is_pending(low.as_ref()), "cleaner parks while awaiting its route");
        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 1, "exactly one route to the dirtiest cell");
        assert_eq!(routes[0], ((0, 0), (2, 0)), "routes to the dirtiest cell");
    }

    #[test]
    fn cleaner_drives_cleaning_leg_and_reports_cleaning() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let dirt: Hypermap<f32> = Hypermap::new(0.0);
        dirt.set(2, 0, 0.9);
        let interactive = InteractiveEntityMap::new();
        let pf = PathfindFixture::new();
        let mut action = GoClean::default();
        let mut low: Box<dyn LowLevelAction> = Box::new(StuckLowAction);
        let mut rng = rng::seeded(7);

        // Tick 1: enqueue + park.
        action.update(
            &cleaning_ctx(&passability, &dirt, &interactive, (0, 0), pf.access()),
            &mut low,
            &mut rng,
        );
        // Resolve the route and tick again to install the cleaning FollowPath.
        pf.resolve_all_routes(vec![(0, 0), (2, 0)], 2);
        let out = action.update(
            &cleaning_ctx(&passability, &dirt, &interactive, (0, 0), pf.access()),
            &mut low,
            &mut rng,
        );
        assert_eq!(low.kind(), LowLevelKind::FollowPath, "installs a follow path");
        assert_eq!(
            out.effects.set_cleaning,
            Some(true),
            "a driving cleaning leg reports cleaning mode on",
        );
    }

    #[test]
    fn cleaner_relocates_when_surroundings_clean() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let dirt: Hypermap<f32> = Hypermap::new(0.0); // nothing above threshold
        let interactive = InteractiveEntityMap::new();
        let pf = PathfindFixture::new();
        let mut action = GoClean::default();
        let mut low: Box<dyn LowLevelAction> = Box::new(StuckLowAction);
        let mut rng = rng::seeded(7);

        let out = action.update(
            &cleaning_ctx(&passability, &dirt, &interactive, (0, 0), pf.access()),
            &mut low,
            &mut rng,
        );
        assert_eq!(out.effects.set_cleaning, Some(false), "relocation is not cleaning");
        let routes = pf.drain_world_routes();
        assert_eq!(routes.len(), 1, "relocates to a fresh area to scan");
        let (start, goal) = routes[0];
        assert_eq!(start, (0, 0));
        let d2 = (goal.0 as f32).powi(2) + (goal.1 as f32).powi(2);
        assert!(
            d2 >= RELOCATE_MIN_TILES * RELOCATE_MIN_TILES
                && d2 <= RELOCATE_MAX_TILES * RELOCATE_MAX_TILES,
            "relocation goal {goal:?} must land in the 10–30 tile ring (d²={d2})",
        );
    }
}

//! High-level actions — the single exclusive task a bot is pursuing.
//!
//! The brain selects one high-level action from the dominant
//! [`Priority`](super::Priority) each tick; that action [`update`](HighLevelAction::update)s
//! the bot's low-level action (`Wait` / `FollowPath`) and may request side
//! effects ([`BrainEffects`]). When an action reports
//! [`HighLevelStatus::Done`] the brain drops it and re-plans next tick.

use crate::rng::{self, StdRng};

use crate::map::hypermap::{world_to_chunk_local, ChunkCoord, Hypermap, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_pathfind::world_tile_walkable;
use crate::map::interactive_entity::{EntityCoordinates, InteractiveEntityMap};
use crate::map::pathfind_service::{PathKind, PathOutcome, RequestId};

use super::low_level::{FollowPath, LowLevelAction, PendingPath, Wait};
use super::priority::PriorityKind;
use super::{BrainContext, BrainEffects, PathfindAccess};

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

/// `true` on the first frame the low-level action reports stuck or finished
/// since the previous tick it did not (rising edge). Prevents re-running
/// expensive A\* / charger scans every frame while `Wait::retry` stays stalled.
fn low_level_needs_replan(low: &dyn LowLevelAction, prev_stuck: bool, prev_finished: bool) -> bool {
    let stuck = low.is_stuck();
    let finished = low.is_finished();
    (stuck && !prev_stuck) || (finished && !prev_finished)
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
}

/// Default mapping from a priority kind to the action that serves it. A brain
/// may supply a different factory, but this covers BlackBot.
pub fn make_high_level(kind: PriorityKind) -> Box<dyn HighLevelAction> {
    match kind {
        PriorityKind::RandomWalking => Box::new(GoToRandomPoints::default()),
        PriorityKind::Patrolling => Box::new(GoToPatrol::new()),
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
    ) {
        let here = (ctx.main_tile.x, ctx.main_tile.y);
        match sample_wander_goal(rng, here, ctx.passability) {
            Some(goal) => {
                let id = pf.queue.enqueue(PathKind::WorldRoute {
                    start: here,
                    goal,
                    max_expanded: WANDER_SEARCH_LIMIT,
                    simplify_buffer: PATH_CORNER_BUFFER,
                });
                self.awaiting = Some(id);
                self.await_elapsed = 0.0;
                *low = Box::new(PendingPath::with_velocity(low.velocity()));
            }
            None => {
                self.awaiting = None;
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
                        *low = Box::new(FollowPath::new(path));
                    }
                    _ => self.request_target(pf, ctx, low, rng),
                }
            } else if self.await_elapsed >= PATH_WAIT_RETRY_S {
                self.request_target(pf, ctx, low, rng);
            }
            self.prev_low_stuck = low.is_stuck();
            self.prev_low_finished = low.is_finished();
            return HighLevelOutcome::running();
        }

        if low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished) {
            self.request_target(pf, ctx, low, rng);
        }
        self.prev_low_stuck = low.is_stuck();
        self.prev_low_finished = low.is_finished();
        HighLevelOutcome::running()
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
                });
                self.awaiting = Some(id);
                self.await_elapsed = 0.0;
                *low = Box::new(PendingPath::with_velocity(low.velocity()));
                return;
            }
            *cursor = (*cursor + 1) % len;
        }
        self.awaiting = None;
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
                            self.request_leg(pf, loop_tiles, here, &mut cursor, low);
                        }
                    }
                }
            } else if self.await_elapsed >= PATH_WAIT_RETRY_S {
                self.request_leg(pf, loop_tiles, here, &mut cursor, low);
            }
            self.prev_low_stuck = low.is_stuck();
            self.prev_low_finished = low.is_finished();
            self.cursor = Some(cursor);
            return HighLevelOutcome::running();
        }

        if low_level_needs_replan(low.as_ref(), self.prev_low_stuck, self.prev_low_finished) {
            // Once we have reached (or abandoned) the waypoint we were heading to,
            // move on to the next; the first engaged leg keeps the nearest one.
            if self.engaged {
                cursor = (cursor + 1) % len;
            }
            self.legs_tried = 0;
            self.request_leg(pf, loop_tiles, here, &mut cursor, low);
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
            });
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
    fn begin_seek_into(
        &mut self,
        pf: PathfindAccess,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        effects: &mut BrainEffects,
    ) {
        self.phase = ChargePhase::Seeking;
        self.seek = None;
        let here = (ctx.main_tile.x, ctx.main_tile.y);
        let candidates = gather_charger_candidates(ctx);
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
            });
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
            // Handler for "need charge but got stuck": restart charger selection
            // on the rising edge only — not every frame while stalled.
            let mut effects = BrainEffects::default();
            self.clear_queues(&mut effects);
            self.charger = None;
            self.dock_route = None;
            self.begin_seek_into(pf, ctx, low, &mut effects);
            self.prev_low_stuck = low.is_stuck();
            return HighLevelOutcome::running_with(effects);
        }
        self.prev_low_stuck = low_stuck;

        match self.phase {
            ChargePhase::Seeking => {
                if self.seek.is_some() {
                    return self.poll_seek(pf, ctx, low);
                }
                if !ctx.main_tile_changed && !low.is_finished() {
                    return HighLevelOutcome::running();
                }
                let mut effects = BrainEffects::default();
                self.begin_seek_into(pf, ctx, low, &mut effects);
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
                        });
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

/// Coordinates of every charger in the nearest 4 hypertiles on the bot's floor.
/// The reachability / path-cost ranking happens later from the async route
/// results (see [`rank_charger_candidates`]).
fn gather_charger_candidates(ctx: &BrainContext) -> Vec<EntityCoordinates> {
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
    use crate::map::interactive_entity::{ChargerEntity, InteractiveEntity};
    use crate::map::pathfind_service::{PathfindQueue, PathfindResults};
    use crate::map::world_map::ChargerFacing;
    use bevy::math::{IVec2, Vec2};
    use bevy::prelude::Entity;

    struct StuckLowAction;

    struct VelocityFinished(Vec2);

    impl LowLevelAction for VelocityFinished {
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

    fn ctx<'a>(
        passability: &'a Hypermap<f32>,
        interactive: &'a InteractiveEntityMap,
        charge: f32,
        tile: (i32, i32),
        pf: PathfindAccess<'a>,
        patrol_loop: Option<&'a [(i32, i32)]>,
    ) -> BrainContext<'a> {
        BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 1.0 / 60.0,
            center: Vec2::new(tile.0 as f32 + 0.5, tile.1 as f32 + 0.5),
            main_tile: IVec2::new(tile.0, tile.1),
            main_tile_changed: true,
            floor: 0,
            charge,
            missing_charge_pct: (1.0 - charge) * 100.0,
            depleted: charge <= 0.0,
            broken: false,
            passability,
            interactive,
            avoidance: None,
            patrol_loop,
            pathfind: Some(pf),
        }
    }

    fn is_pending(low: &dyn LowLevelAction) -> bool {
        low.label() == "PendingPath"
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
        let (path, _) = low.path().expect("ranking should install a route after injected results");
        assert_eq!(path.last().copied(), Some((6, 0)));
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
        let (path, _) = low.path().expect("ranked fallback should install a route");
        assert_eq!(path.last().copied(), Some((4, 0)));
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
        assert!(low.path().is_some(), "seek should install a route after injected results");

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
        assert!(low.path().is_some(), "approach route should be installed");

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
        let checks = enqueue_patrol_candidates(&mut rng::seeded(7), (0, 0), &passability, &queue);
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
}

//! High-level actions — the single exclusive task a bot is pursuing.
//!
//! The brain selects one high-level action from the dominant
//! [`Priority`](super::Priority) each tick; that action [`update`](HighLevelAction::update)s
//! the bot's low-level action (`Wait` / `FollowPath`) and may request side
//! effects ([`BrainEffects`]). When an action reports
//! [`HighLevelStatus::Done`] the brain drops it and re-plans next tick.

use rand::rngs::StdRng;
use rand::Rng;

use crate::map::hypermap::{world_to_chunk_local, ChunkCoord, Hypermap, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_pathfind::{
    astar_shortest_world_path, simplify_path_line_of_sight, HypermapPathResult, HypermapSearchLimits,
};
use crate::map::interactive_entity::{EntityCoordinates, InteractiveEntityMap};

use super::low_level::{FollowPath, LowLevelAction, Wait};
use super::priority::PriorityKind;
use super::{BrainContext, BrainEffects};

/// Wander radius (tiles) for [`GoToRandomPoints`].
const WANDER_RADIUS: f32 = 15.0;
/// Random-target sampling attempts before giving up for this tick.
const MAX_TARGET_ATTEMPTS: u32 = 8;
/// Tiles kept on each side of a bend during path simplification (see
/// [`simplify_path_line_of_sight`]).
const PATH_CORNER_BUFFER: usize = 1;
/// Retry delay when no wander target / charger could be found.
const RETRY_S: f32 = 0.5;
/// A* expansion cap for charger routes.
const SEARCH_LIMIT: usize = 5000;

/// Charge gained per second while docked (infinite station — charger stored
/// energy is intentionally ignored).
pub const RECHARGE_PER_S: f32 = 0.05;
/// Charge level treated as "full" (undock threshold).
const CHARGE_FULL: f32 = 0.999;
/// Retry delay while seeking a charger that isn't currently reachable/free.
const CHARGE_RETRY_S: f32 = 1.0;
/// Enter waiting queue once Manhattan distance to the station is < 5.
const WAITING_QUEUE_ENTER_DISTANCE: i32 = 4;
/// Random backoff while holding a waiting-queue slot near a station.
const WAITING_RECHECK_MIN_S: f32 = 0.2;
const WAITING_RECHECK_MAX_S: f32 = 0.8;

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
}

/// Default mapping from a priority kind to the action that serves it. A brain
/// may supply a different factory, but this covers BlackBot.
pub fn make_high_level(kind: PriorityKind) -> Box<dyn HighLevelAction> {
    match kind {
        PriorityKind::RandomWalking => Box::new(GoToRandomPoints),
        PriorityKind::RechargeYourself => Box::new(GoToChargeStation::new()),
    }
}

// ---------------------------------------------------------------------------
// GoToRandomPoints
// ---------------------------------------------------------------------------

/// Perpetual wander: whenever the current path finishes, pick a new random
/// reachable target and follow it. Never reports `Done`.
pub struct GoToRandomPoints;

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
        let needs_new_target = low.is_stuck() || low.is_finished();
        if needs_new_target {
            let here = (ctx.main_tile.x, ctx.main_tile.y);
            match pick_random_target(rng, here, ctx.passability) {
                Some(path) => *low = Box::new(FollowPath::new(path)),
                None => *low = Box::new(Wait::new(RETRY_S)),
            }
        }
        HighLevelOutcome::running()
    }
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

/// Path to the nearest accessible, unoccupied charger, dock, charge to full,
/// then report `Done`.
pub struct GoToChargeStation {
    phase: ChargePhase,
    charger: Option<EntityCoordinates>,
    queued_wanting: Option<EntityCoordinates>,
    queued_waiting: Option<EntityCoordinates>,
}

impl GoToChargeStation {
    pub fn new() -> Self {
        Self {
            phase: ChargePhase::Seeking,
            charger: None,
            queued_wanting: None,
            queued_waiting: None,
        }
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
    fn update(
        &mut self,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        rng: &mut StdRng,
    ) -> HighLevelOutcome {
        if low.is_stuck() && self.phase != ChargePhase::Charging {
            // Handler for "need charge but got stuck": immediately restart charger
            // selection instead of progressing the previous phase state machine.
            let mut effects = BrainEffects::default();
            self.clear_queues(&mut effects);
            self.charger = None;
            self.phase = ChargePhase::Seeking;
            match find_best_charger(ctx) {
                Some((coords, path)) => {
                    self.retarget(coords, &mut effects);
                    *low = Box::new(FollowPath::new(path));
                }
                None => {
                    *low = Box::new(Wait::new(CHARGE_RETRY_S));
                }
            }
            return HighLevelOutcome::running_with(effects);
        }

        match self.phase {
            ChargePhase::Seeking => {
                if !ctx.main_tile_changed && !low.is_finished() {
                    return HighLevelOutcome::running();
                }

                let mut effects = BrainEffects::default();
                match find_best_charger(ctx) {
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

                if low.is_finished() {
                    if dock_allowed_for(ctx, self.queued_waiting, charger) {
                        let here = (ctx.main_tile.x, ctx.main_tile.y);
                        let goal = (charger.x, charger.y);
                        match astar_shortest_world_path(
                            ctx.passability,
                            here,
                            goal,
                            HypermapSearchLimits { max_expanded: SEARCH_LIMIT },
                        ) {
                            HypermapPathResult::Found { path, .. } if path.len() <= 1 => {
                                self.phase = ChargePhase::Charging;
                                *low = Box::new(Wait::new(f32::INFINITY));
                                let mut effects = BrainEffects::default();
                                effects.queue_unwait = self.queued_waiting.take();
                                effects.dock = Some(charger);
                                return HighLevelOutcome::running_with(effects);
                            }
                            HypermapPathResult::Found { path, .. } => {
                                let simplified = simplify_path_line_of_sight(
                                    ctx.passability,
                                    &path,
                                    PATH_CORNER_BUFFER,
                                );
                                self.phase = ChargePhase::Traveling;
                                *low = Box::new(FollowPath::new(simplified));
                            }
                            _ => {
                                self.phase = ChargePhase::Seeking;
                                self.charger = None;
                            }
                        }
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

/// Picks a random reachable tile within [`WANDER_RADIUS`] and returns a
/// simplified A* path to it (start + bends + goal), or `None`.
pub fn pick_random_target(
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
        let target = (current_tile.0 + dx.round() as i32, current_tile.1 + dy.round() as i32);
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
                return Some(simplify_path_line_of_sight(passability, &path, PATH_CORNER_BUFFER));
            }
        }
    }
    None
}

/// Finds a charger in the nearest 4 hypertiles using queue-aware selection:
/// prefer waiting queues with <2 actors; when all are busier, bias toward
/// farther-ranked stations (`2nd nearest`, `3rd nearest`, ...).
fn find_best_charger(ctx: &BrainContext) -> Option<(EntityCoordinates, Vec<(i32, i32)>)> {
    let here = (ctx.main_tile.x, ctx.main_tile.y);
    let nearby_chunks = nearest_hypertiles_4(here);

    let mut candidates: Vec<(EntityCoordinates, Vec<(i32, i32)>, usize, usize)> = Vec::new();
    for entry in ctx.interactive.iter().filter(|e| e.coordinates.floor == ctx.floor) {
        let (chunk, _) = world_to_chunk_local(entry.coordinates.x, entry.coordinates.y);
        if !nearby_chunks.contains(&chunk) {
            continue;
        }
        if entry.entity.as_charger().is_none() {
            continue;
        }
        let goal = (entry.coordinates.x, entry.coordinates.y);
        let (path_cost, path) = match astar_shortest_world_path(
            ctx.passability,
            here,
            goal,
            HypermapSearchLimits { max_expanded: SEARCH_LIMIT },
        ) {
            HypermapPathResult::Found { path, .. } if path.len() > 1 => {
                (
                    path.len(),
                    simplify_path_line_of_sight(ctx.passability, &path, PATH_CORNER_BUFFER),
                )
            }
            // Already on/at the charger tile — a single-waypoint path arrives at once.
            HypermapPathResult::Found { path, .. } => (path.len(), path),
            _ => continue,
        };
        let waiting_len = ctx.interactive.waiting_len(entry.coordinates);
        candidates.push((entry.coordinates, path, waiting_len, path_cost));
    }

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
    rng.gen_range(WAITING_RECHECK_MIN_S..WAITING_RECHECK_MAX_S)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::brain::low_level::{FollowTuning, Idle};
    use crate::actor::brain::test_support::test_state;
    use crate::map::interactive_entity::{ChargerEntity, InteractiveEntity};
    use crate::map::world_map::ChargerFacing;
    use bevy::math::{IVec2, Vec2};
    use bevy::prelude::Entity;
    use rand::SeedableRng;

    struct StuckLowAction;

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

    fn ctx<'a>(
        passability: &'a Hypermap<f32>,
        interactive: &'a InteractiveEntityMap,
        charge: f32,
        tile: (i32, i32),
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
        }
    }

    /// Bot starts on the charger tile so the route is a single waypoint and the
    /// phase machine can be driven deterministically without long travel.
    #[test]
    fn charge_station_seeks_docks_charges_and_finishes() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        passability.set(4, 0, 1.0);
        let mut interactive = InteractiveEntityMap::new();
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::ground(4, 0),
            ChargerFacing::North,
        )));

        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = StdRng::seed_from_u64(0);
        let tuning = FollowTuning::default();
        let mut state = test_state();
        state.center = Vec2::new(4.5, 0.5);

        // Seeking → installs a FollowPath to the charger tile.
        let out = action.update(&ctx(&passability, &interactive, 0.1, (4, 0)), &mut low, &mut rng);
        assert!(matches!(out.status, HighLevelStatus::Running));
        assert!(low.path().is_some(), "must be routing to the charger");

        // One execute reaches the single waypoint (bot is on the tile).
        low.execute(&mut state, &ctx(&passability, &interactive, 0.1, (4, 0)), &mut rng, &tuning);
        assert!(low.is_finished(), "single-waypoint route completes on arrival");

        // Traveling near station -> joins waiting queue first.
        let out = action.update(&ctx(&passability, &interactive, 0.1, (4, 0)), &mut low, &mut rng);
        assert_eq!(out.effects.queue_wait, Some(EntityCoordinates::ground(4, 0)));
        interactive.add_waiting(EntityCoordinates::ground(4, 0), Entity::PLACEHOLDER);

        // Next waiting recheck (forced finished low action) -> first in queue and free -> dock.
        low = Box::new(Idle);
        let out = action.update(&ctx(&passability, &interactive, 0.1, (4, 0)), &mut low, &mut rng);
        assert_eq!(out.effects.dock, Some(EntityCoordinates::ground(4, 0)));

        // Charging → recharge requested while not full.
        let out = action.update(&ctx(&passability, &interactive, 0.5, (4, 0)), &mut low, &mut rng);
        assert!(out.effects.recharge > 0.0);
        assert!(matches!(out.status, HighLevelStatus::Running));

        // Full → undock + done.
        let out = action.update(&ctx(&passability, &interactive, 1.0, (4, 0)), &mut low, &mut rng);
        assert!(matches!(out.status, HighLevelStatus::Done));
        assert_eq!(out.effects.undock, Some(EntityCoordinates::ground(4, 0)));
    }

    #[test]
    fn traveling_does_not_restop_each_tile_inside_waiting_zone() {
        // Regression: once a bot has joined a charger's waiting queue and been
        // cleared to approach, the Traveling phase must not bounce it back into a
        // WaitingQueue `Wait` on every tile boundary inside the zone — that is the
        // "stop and go at every step near the charger" stutter.
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

        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = StdRng::seed_from_u64(0);

        // Seeking → Traveling: joins the *wanting* queue and routes to the charger.
        let _ = action.update(&ctx(&passability, &interactive, 0.1, (0, 0)), &mut low, &mut rng);
        assert!(low.path().is_some(), "routing to the charger");

        // First entry into the waiting zone joins the *waiting* queue (one stop).
        let out = action.update(&ctx(&passability, &interactive, 0.1, (7, 0)), &mut low, &mut rng);
        assert_eq!(out.effects.queue_wait, Some(charger), "joins waiting queue once");
        interactive.add_waiting(charger, Entity::PLACEHOLDER);

        // Cleared to approach: WaitingQueue → Traveling with a fresh route.
        low = Box::new(Idle); // the short waiting recheck is "finished"
        let _ = action.update(&ctx(&passability, &interactive, 0.1, (7, 0)), &mut low, &mut rng);
        assert!(low.path().is_some(), "approaching the charger again");

        // Crossing more tiles still inside the zone must NOT re-stop the bot.
        for tile in [(8, 0), (9, 0)] {
            let out = action.update(&ctx(&passability, &interactive, 0.1, tile), &mut low, &mut rng);
            assert_eq!(out.effects.queue_wait, None, "must not re-join waiting queue at {tile:?}");
            assert!(low.path().is_some(), "must keep following the approach path at {tile:?}, not stop");
        }
    }

    #[test]
    fn no_charger_waits_instead_of_routing() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        passability.set(0, 0, 1.0);
        let interactive = InteractiveEntityMap::new();
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = StdRng::seed_from_u64(0);

        let out = action.update(&ctx(&passability, &interactive, 0.1, (0, 0)), &mut low, &mut rng);
        assert!(matches!(out.status, HighLevelStatus::Running));
        assert!(low.path().is_none(), "no charger → should be waiting, not following a path");
        assert!(!low.is_finished(), "the retry Wait keeps the action alive");
    }

    #[test]
    fn recharge_search_uses_nearest_four_hypertiles() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in -2..=0 {
            passability.set(x, 0, 1.0);
        }
        // Also make a reachable charger in a far chunk that should be ignored by
        // the bounded 4-hypertile search from (0, 0).
        for x in 0..=130 {
            passability.set(x, 1, 1.0);
        }

        let mut interactive = InteractiveEntityMap::new();
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::ground(-2, 0), // chunk (-1, 0), should be considered
            ChargerFacing::North,
        )));
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::ground(130, 1), // chunk (1, 0), excluded from nearest 4
            ChargerFacing::North,
        )));

        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = StdRng::seed_from_u64(0);
        let out = action.update(&ctx(&passability, &interactive, 0.1, (0, 0)), &mut low, &mut rng);

        assert!(matches!(out.status, HighLevelStatus::Running));
        let (path, _) = low.path().expect("must route to charger in nearest hypertiles");
        assert_eq!(path.last().copied(), Some((-2, 0)));
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

        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = StdRng::seed_from_u64(0);
        let out = action.update(&ctx(&passability, &interactive, 0.1, (0, 0)), &mut low, &mut rng);

        assert!(matches!(out.status, HighLevelStatus::Running));
        let (path, _) = low.path().expect("must pick a charger");
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
        assert_eq!(interactive.waiting_len(first), 2);
        assert_eq!(interactive.waiting_len(second), 2);
        assert_eq!(interactive.waiting_len(third), 2);

        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(Idle);
        let mut rng = StdRng::seed_from_u64(0);
        let out = action.update(&ctx(&passability, &interactive, 0.1, (0, 0)), &mut low, &mut rng);

        assert!(matches!(out.status, HighLevelStatus::Running));
        let (path, _) = low.path().expect("must pick ranked fallback charger");
        assert_eq!(path.last().copied(), Some((4, 0)), "closest has 2 waiters, so choose 2nd closest");
    }

    #[test]
    fn random_walker_stuck_handler_retargets() {
        let passability: Hypermap<f32> = Hypermap::new(1.0);
        let interactive = InteractiveEntityMap::new();
        let mut action = GoToRandomPoints;
        let mut low: Box<dyn LowLevelAction> = Box::new(StuckLowAction);
        let mut rng = StdRng::seed_from_u64(42);

        let out = action.update(&ctx(&passability, &interactive, 1.0, (0, 0)), &mut low, &mut rng);
        assert!(matches!(out.status, HighLevelStatus::Running));
        assert!(low.path().is_some(), "stuck random walker should install a new route");
    }

    #[test]
    fn recharge_stuck_handler_reruns_charger_search() {
        let passability: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..=6 {
            passability.set(x, 0, 1.0);
        }
        let mut interactive = InteractiveEntityMap::new();
        interactive.insert(InteractiveEntity::Charger(ChargerEntity::new(
            EntityCoordinates::ground(6, 0),
            ChargerFacing::North,
        )));
        let mut action = GoToChargeStation::new();
        let mut low: Box<dyn LowLevelAction> = Box::new(StuckLowAction);
        let mut rng = StdRng::seed_from_u64(1);

        let out = action.update(&ctx(&passability, &interactive, 0.2, (0, 0)), &mut low, &mut rng);
        assert!(matches!(out.status, HighLevelStatus::Running));
        let (path, _) = low.path().expect("stuck recharge should immediately rerun charger search");
        assert_eq!(path.last().copied(), Some((6, 0)));
    }
}

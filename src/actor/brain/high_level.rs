//! High-level actions — the single exclusive task a bot is pursuing.
//!
//! The brain selects one high-level action from the dominant
//! [`Priority`](super::Priority) each tick; that action [`update`](HighLevelAction::update)s
//! the bot's low-level action (`Wait` / `FollowPath`) and may request side
//! effects ([`BrainEffects`]). When an action reports
//! [`HighLevelStatus::Done`] the brain drops it and re-plans next tick.

use rand::rngs::StdRng;
use rand::Rng;

use crate::map::hypermap::Hypermap;
use crate::map::hypermap_pathfind::{
    astar_shortest_world_path, manhattan, simplify_path_line_of_sight, HypermapPathResult,
    HypermapSearchLimits,
};
use crate::map::interactive_entity::{EntityCoordinates, EntityType, InteractiveEntityMap};

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

/// Max 4-neighbour steps out to look for a reachable charger.
const CHARGER_SEARCH_STEPS: u32 = 64;
/// Charge gained per second while docked (infinite station — charger stored
/// energy is intentionally ignored).
pub const RECHARGE_PER_S: f32 = 0.05;
/// Charge level treated as "full" (undock threshold).
const CHARGE_FULL: f32 = 0.999;
/// Retry delay while seeking a charger that isn't currently reachable/free.
const CHARGE_RETRY_S: f32 = 1.0;

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
        if low.is_finished() {
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
    Charging,
}

/// Path to the nearest accessible, unoccupied charger, dock, charge to full,
/// then report `Done`.
pub struct GoToChargeStation {
    phase: ChargePhase,
    charger: Option<EntityCoordinates>,
}

impl GoToChargeStation {
    pub fn new() -> Self {
        Self { phase: ChargePhase::Seeking, charger: None }
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
            ChargePhase::Charging => "GoToChargeStation (charging)".to_string(),
        }
    }
    fn update(
        &mut self,
        ctx: &BrainContext,
        low: &mut Box<dyn LowLevelAction>,
        _rng: &mut StdRng,
    ) -> HighLevelOutcome {
        match self.phase {
            ChargePhase::Seeking => {
                match find_nearest_charger(ctx) {
                    Some((coords, path)) => {
                        self.charger = Some(coords);
                        *low = Box::new(FollowPath::new(path));
                        self.phase = ChargePhase::Traveling;
                    }
                    None => {
                        *low = Box::new(Wait::new(CHARGE_RETRY_S));
                    }
                }
                HighLevelOutcome::running()
            }
            ChargePhase::Traveling => {
                // Dock as soon as the bot is standing on the charger tile — don't
                // wait for `FollowPath` to settle within `waypoint_eps`. Steering
                // inertia can leave a lone bot orbiting the exact tile center it
                // can never land on, so gating the dock on sub-tile arrival makes
                // it circle forever instead of charging. Tile occupancy (`round`
                // of the float center) is the forgiving, correct dock condition.
                let on_charger_tile = self.charger.is_some_and(|c| {
                    c.floor == ctx.floor && ctx.main_tile.x == c.x && ctx.main_tile.y == c.y
                });
                if low.is_finished() || on_charger_tile {
                    if let Some(c) = self.charger {
                        if charger_free_for(ctx.interactive, c, ctx.entity) {
                            self.phase = ChargePhase::Charging;
                            // Dwell indefinitely; we exit via the charge check below.
                            *low = Box::new(Wait::new(f32::INFINITY));
                            let mut e = BrainEffects::default();
                            e.dock = Some(c);
                            return HighLevelOutcome::running_with(e);
                        }
                    }
                    // Lost the charger (taken) or the path was abandoned: re-seek.
                    self.phase = ChargePhase::Seeking;
                    self.charger = None;
                }
                HighLevelOutcome::running()
            }
            ChargePhase::Charging => {
                if ctx.charge >= CHARGE_FULL {
                    let mut e = BrainEffects::default();
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

/// Finds the nearest charger reachable from the bot that is free (no occupant,
/// or occupied by the bot itself) and returns its coordinates plus a simplified
/// path to its (passable) tile.
fn find_nearest_charger(ctx: &BrainContext) -> Option<(EntityCoordinates, Vec<(i32, i32)>)> {
    let here = (ctx.main_tile.x, ctx.main_tile.y);
    let candidates = ctx.interactive.find_accessible_within(
        ctx.passability,
        here,
        ctx.floor,
        CHARGER_SEARCH_STEPS,
        Some(EntityType::Charger),
    );

    let mut best: Option<(EntityCoordinates, u32)> = None;
    for entry in candidates {
        let free = entry
            .entity
            .as_charger()
            .map(|c| c.occupant().is_none_or(|o| o == ctx.entity))
            .unwrap_or(false);
        if !free {
            continue;
        }
        let goal = (entry.coordinates.x, entry.coordinates.y);
        let dist = manhattan(here, goal);
        if best.is_none_or(|(_, d)| dist < d) {
            best = Some((entry.coordinates, dist));
        }
    }

    let (coords, _) = best?;
    let goal = (coords.x, coords.y);
    match astar_shortest_world_path(
        ctx.passability,
        here,
        goal,
        HypermapSearchLimits { max_expanded: SEARCH_LIMIT },
    ) {
        HypermapPathResult::Found { path, .. } if path.len() > 1 => {
            Some((coords, simplify_path_line_of_sight(ctx.passability, &path, PATH_CORNER_BUFFER)))
        }
        // Already on/at the charger tile — a single-waypoint path arrives at once.
        HypermapPathResult::Found { path, .. } => Some((coords, path)),
        _ => None,
    }
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
            floor: 0,
            charge,
            missing_charge_pct: (1.0 - charge) * 100.0,
            depleted: charge <= 0.0,
            broken: false,
            passability,
            interactive,
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

        // Traveling → arrived & free → dock + start charging.
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
    fn charge_station_docks_on_charger_tile_even_if_path_unsettled() {
        // Bot is standing on the charger tile but its `FollowPath` has not
        // settled within `waypoint_eps`. Tile occupancy must dock it anyway — a
        // lone bot can orbit the exact tile center indefinitely, so gating the
        // dock on sub-tile arrival would make it circle forever instead of charge.
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

        // Seeking → Traveling, installs a route to the charger tile.
        let out = action.update(&ctx(&passability, &interactive, 0.1, (4, 0)), &mut low, &mut rng);
        assert!(matches!(out.status, HighLevelStatus::Running));
        assert!(low.path().is_some(), "must be routing to the charger");
        assert!(!low.is_finished(), "precondition: path not settled (no execute ran)");

        // Standing on the charger tile with the path still unsettled → tile dock.
        let out = action.update(&ctx(&passability, &interactive, 0.1, (4, 0)), &mut low, &mut rng);
        assert_eq!(out.effects.dock, Some(EntityCoordinates::ground(4, 0)));
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
}

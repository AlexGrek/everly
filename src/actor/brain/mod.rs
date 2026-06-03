//! The OOP "brain" for smart actors.
//!
//! A [`Brain`] sits *above* the low-level movement pipeline
//! ([`Actor::try_move`](crate::actor::Actor::try_move)). Each frame it:
//!
//! 1. runs every [`Behavior`] it holds — they mutate the [`Priorities`] list
//!    (the bot's sorted "wishes"), each carrying a `value`;
//! 2. selects the single highest-value priority and ensures the matching
//!    **high-level action** is current (pre-empting a different one);
//! 3. lets that [`HighLevelAction`] advance — it dictates the current
//!    **low-level action** ([`Wait`] / [`FollowPath`]) and may request
//!    [`BrainEffects`];
//! 4. executes the low-level action, writing this frame's movement intent.
//!
//! Behaviors receive a [`BrainContext`] bundling every bot property they may
//! read. Side effects (docking a charger, adding charge) are *returned*, not
//! applied here, so the brain itself never touches ECS resources — the owning
//! system applies them. See `docs/actor-brain.md`.

pub mod behavior;
pub mod high_level;
pub mod low_level;
pub mod priority;

use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::SeedableRng;

use crate::actor::{ActorMoveBuffer, ActorState};
use crate::map::hypermap::Hypermap;
use crate::map::interactive_entity::{EntityCoordinates, InteractiveEntityMap};

pub use behavior::{Behavior, ChargeSelfKeeper, RandomWalker};
pub use high_level::{
    make_high_level, GoToChargeStation, GoToRandomPoints, HighLevelAction, HighLevelStatus,
    RECHARGE_PER_S,
};
pub use low_level::{FollowPath, FollowTuning, Idle, LowLevelAction, Wait};
pub use priority::{Priorities, Priority, PriorityKind};

/// Read-only snapshot of every bot property a behavior / high-level action may
/// consult during a single brain tick.
pub struct BrainContext<'a> {
    pub entity: Entity,
    pub dt: f32,
    pub center: Vec2,
    pub main_tile: IVec2,
    pub main_tile_changed: bool,
    pub floor: i32,
    pub charge: f32,
    pub missing_charge_pct: f32,
    pub depleted: bool,
    pub broken: bool,
    pub passability: &'a Hypermap<f32>,
    pub interactive: &'a InteractiveEntityMap,
}

/// Side effects a high-level action requests, applied by the owning ECS system
/// after the tick. Fixed-size — the tick never allocates to report effects.
#[derive(Debug, Default, Clone, Copy)]
pub struct BrainEffects {
    /// Add this bot to the station's wanting queue.
    pub queue_want: Option<EntityCoordinates>,
    /// Remove this bot from the station's wanting queue.
    pub queue_unwant: Option<EntityCoordinates>,
    /// Add this bot to the station's waiting queue (and drop from wanting).
    pub queue_wait: Option<EntityCoordinates>,
    /// Remove this bot from the station's waiting queue.
    pub queue_unwait: Option<EntityCoordinates>,
    /// Dock this bot at the charger at these coordinates.
    pub dock: Option<EntityCoordinates>,
    /// Undock this bot from the charger at these coordinates.
    pub undock: Option<EntityCoordinates>,
    /// Add this much charge (`0.0..=1.0` units) to the bot this frame.
    pub recharge: f32,
}

/// Maps a winning [`PriorityKind`] to the high-level action that serves it.
pub type HighLevelFactory = fn(PriorityKind) -> Box<dyn HighLevelAction>;

/// The brain component attached to a smart actor.
#[derive(Component)]
pub struct Brain {
    behaviors: Vec<Box<dyn Behavior>>,
    priorities: Priorities,
    current: Option<Box<dyn HighLevelAction>>,
    low_level: Box<dyn LowLevelAction>,
    factory: HighLevelFactory,
    /// Movement tuning handed to [`FollowPath`] each frame.
    pub tuning: FollowTuning,
    rng: StdRng,
    rng_seed: u64,
}

impl Brain {
    pub fn new(behaviors: Vec<Box<dyn Behavior>>, factory: HighLevelFactory, rng_seed: u64) -> Self {
        Self {
            behaviors,
            priorities: Priorities::new(),
            current: None,
            low_level: Box::new(Idle),
            factory,
            tuning: FollowTuning::default(),
            rng: StdRng::seed_from_u64(rng_seed),
            rng_seed,
        }
    }

    pub fn rng_seed(&self) -> u64 {
        self.rng_seed
    }

    /// The brain's seeded RNG, exposed for deterministic side rolls owned by the
    /// same entity (e.g. BlackBot's wear/break checks) so they share one stream.
    pub fn rng_mut(&mut self) -> &mut StdRng {
        &mut self.rng
    }

    /// Forget the current plan so the brain re-evaluates from scratch next tick.
    pub fn reset(&mut self) {
        self.current = None;
        self.low_level = Box::new(Idle);
        self.priorities.clear();
    }

    /// Stop all motion this frame (used by the depleted / broken gate). Keeps the
    /// current plan but bleeds momentum so resuming is clean.
    pub fn halt(&mut self, state: &mut ActorState) {
        self.low_level.halt();
        state.move_buffer = ActorMoveBuffer::default();
        state.next_waypoint_hint = None;
    }

    /// Run one full high-level tick and return any requested side effects.
    pub fn tick(&mut self, ctx: &BrainContext, state: &mut ActorState) -> BrainEffects {
        self.priorities.clear();
        for behavior in self.behaviors.iter_mut() {
            behavior.update_priorities(ctx, &mut self.priorities);
        }

        // Select / pre-empt the high-level action from the dominant wish.
        match self.priorities.top() {
            Some(top) => {
                let needs_switch = self.current.as_ref().map(|a| a.kind()) != Some(top.kind);
                if needs_switch {
                    self.current = Some((self.factory)(top.kind));
                    self.low_level = Box::new(Idle); // fresh action plans from scratch
                }
            }
            None => self.current = None,
        }

        let mut effects = BrainEffects::default();
        let mut done = false;
        if let Some(action) = self.current.as_mut() {
            let outcome = action.update(ctx, &mut self.low_level, &mut self.rng);
            effects = outcome.effects;
            done = matches!(outcome.status, HighLevelStatus::Done);
        }
        if done {
            self.current = None;
        }

        self.low_level.execute(state, ctx, &mut self.rng, &self.tuning);
        effects
    }

    // --- Inspector / overlay accessors -------------------------------------

    pub fn current_priority(&self) -> Option<Priority> {
        self.priorities.top()
    }

    pub fn current_kind(&self) -> Option<PriorityKind> {
        self.current.as_ref().map(|a| a.kind())
    }

    pub fn high_level_label(&self) -> String {
        self.current.as_ref().map(|a| a.label()).unwrap_or_else(|| "—".to_string())
    }

    pub fn low_level_label(&self) -> String {
        self.low_level.label()
    }

    pub fn target_tile(&self) -> Option<(i32, i32)> {
        self.low_level.target_tile()
    }

    pub fn path(&self) -> Option<(&[(i32, i32)], usize)> {
        self.low_level.path()
    }

    pub fn velocity(&self) -> Vec2 {
        self.low_level.velocity()
    }

    pub fn stuck_timer(&self) -> f32 {
        self.low_level.stuck_timer()
    }

    pub fn is_stuck(&self) -> bool {
        self.low_level.is_stuck()
    }

    pub fn has_target(&self) -> bool {
        self.low_level.path().map(|(p, i)| i < p.len()).unwrap_or(false)
    }

    pub fn remaining_waypoints(&self) -> usize {
        self.low_level.path().map(|(p, i)| p.len().saturating_sub(i)).unwrap_or(0)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for brain unit tests.
    use super::*;
    use std::sync::OnceLock;

    fn empty_passability() -> &'static Hypermap<f32> {
        static M: OnceLock<Hypermap<f32>> = OnceLock::new();
        M.get_or_init(|| Hypermap::new(0.0))
    }

    fn empty_interactive() -> &'static InteractiveEntityMap {
        static M: OnceLock<InteractiveEntityMap> = OnceLock::new();
        M.get_or_init(InteractiveEntityMap::new)
    }

    /// A `BrainContext` borrowing shared empty maps, with the given charge.
    pub fn ctx_with_charge(charge: f32) -> BrainContext<'static> {
        BrainContext {
            entity: Entity::PLACEHOLDER,
            dt: 1.0 / 60.0,
            center: Vec2::ZERO,
            main_tile: IVec2::ZERO,
            main_tile_changed: true,
            floor: 0,
            charge,
            missing_charge_pct: (1.0 - charge) * 100.0,
            depleted: charge <= 0.0,
            broken: false,
            passability: empty_passability(),
            interactive: empty_interactive(),
        }
    }

    pub fn test_state() -> ActorState {
        ActorState {
            center: Vec2::new(0.5, 0.5),
            radius_subtiles: 2,
            rotation: 0.0,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: None,
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
        }
    }

    pub fn black_bot_brain(seed: u64) -> Brain {
        Brain::new(
            vec![Box::new(RandomWalker), Box::new(ChargeSelfKeeper::new())],
            make_high_level,
            seed,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn full_charge_selects_random_walking() {
        let mut brain = black_bot_brain(1);
        let mut state = test_state();
        brain.tick(&ctx_with_charge(1.0), &mut state);
        assert_eq!(brain.current_kind(), Some(PriorityKind::RandomWalking));
    }

    #[test]
    fn low_charge_preempts_with_recharge() {
        let mut brain = black_bot_brain(2);
        let mut state = test_state();
        brain.tick(&ctx_with_charge(1.0), &mut state);
        assert_eq!(brain.current_kind(), Some(PriorityKind::RandomWalking));
        // Drop below 25% — recharge (≥75) outranks wander (15).
        brain.tick(&ctx_with_charge(0.20), &mut state);
        assert_eq!(brain.current_kind(), Some(PriorityKind::RechargeYourself));
    }

    #[test]
    fn recharge_releases_at_full_and_returns_to_wander() {
        let mut brain = black_bot_brain(3);
        let mut state = test_state();
        brain.tick(&ctx_with_charge(0.20), &mut state);
        assert_eq!(brain.current_kind(), Some(PriorityKind::RechargeYourself));
        // Still latched while topping up (floored above wander).
        brain.tick(&ctx_with_charge(0.80), &mut state);
        assert_eq!(brain.current_kind(), Some(PriorityKind::RechargeYourself));
        // Full: latch releases, wander wins again.
        brain.tick(&ctx_with_charge(1.0), &mut state);
        assert_eq!(brain.current_kind(), Some(PriorityKind::RandomWalking));
    }

    #[test]
    fn reset_clears_current_plan() {
        let mut brain = black_bot_brain(4);
        let mut state = test_state();
        brain.tick(&ctx_with_charge(1.0), &mut state);
        assert!(brain.current_kind().is_some());
        brain.reset();
        assert!(brain.current_kind().is_none());
    }
}

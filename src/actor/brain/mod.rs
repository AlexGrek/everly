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
pub mod memory;
pub mod path;
pub mod priority;

use bevy::prelude::*;
use crate::rng::{self, StdRng};

use crate::actor::dispatch::{DispatchQueue, RepairPart};
use crate::actor::{ActorMoveBuffer, ActorState};
use crate::hud::game_log::{GameLog, LogEntry, LogLevel};
use crate::map::hypermap::Hypermap;
use crate::map::interactive_entity::{EntityCoordinates, InteractiveEntityMap};
use crate::map::passability::{DynamicPassabilityMap, SubtilePassability};
use crate::map::pathfind_service::{PathfindQueue, PathfindResults};

pub use behavior::{Behavior, ChargeSelfKeeper, FixerDuty, Patroller, RandomWalker};
pub use high_level::{
    assemble_patrol_loop, enqueue_patrol_candidates, make_high_level, GoFixBots,
    GoToChargeStation, GoToPatrol, GoToRandomPoints, HighLevelAction, HighLevelStatus,
    RECHARGE_PER_S,
};
pub use low_level::{FollowPath, FollowTuning, Idle, LowLevelAction, LowLevelKind, PendingPath, Wait};
pub use memory::{
    BotMemory, CoordinatesMemoryId, FloatMemoryId, FreeformMemoryId, IntegerMemoryId, MemoryRecord,
};
pub use path::PathNode;
pub use priority::{Priorities, Priority, PriorityKind};

/// Read-only views the low-level subtile bot-on-bot detour needs: the actor's
/// own size-aware footprint test against static geometry and the dynamic
/// occupancy of other creatures.
///
/// Bundled into one optional so most call sites (and unit tests) that don't run
/// collision avoidance can simply pass `None`.
#[derive(Clone, Copy)]
pub struct AvoidanceViews<'a> {
    /// Dynamic occupancy (other actors' footprints); read buffer.
    pub dynamic: &'a DynamicPassabilityMap,
    /// Static geometry as per-subtile flags (walls, void, corners).
    pub static_subtiles: &'a Hypermap<SubtilePassability>,
    /// Flag bits this actor treats as impassable (size-aware footprint test).
    pub blocked_flags: u64,
}

/// Handles a high-level action uses to drive the async pathfinding service:
/// enqueue a query and read its result back by id. Both are interior-mutable so
/// they're usable through the shared `&BrainContext`.
#[derive(Clone, Copy)]
pub struct PathfindAccess<'a> {
    pub queue: &'a PathfindQueue,
    pub results: &'a PathfindResults,
}

/// Everything a fixer bot's [`GoFixBots`](high_level::GoFixBots) needs that other
/// bots don't: the shared dispatch board, the bot's home depot, and what it is
/// carrying. Bundled into one optional so non-fixer bots (and most tests) pass
/// `None`. `dispatch` is interior-mutable, so claims/releases work through `&`.
#[derive(Clone, Copy)]
pub struct FixerContext<'a> {
    /// Shared repair-request board.
    pub dispatch: &'a DispatchQueue,
    /// The fixer's home parts depot (lazily located at spawn); `None` until found.
    pub home_depot: Option<EntityCoordinates>,
    /// The part the fixer is currently carrying, if any.
    pub carried: Option<RepairPart>,
}

/// Read-only snapshot of every bot property a behavior / high-level action may
/// consult during a single brain tick.
pub struct BrainContext<'a> {
    pub entity: Entity,
    pub dt: f32,
    pub center: Vec2,
    /// Bot footprint radius in subtiles. Plumbed to `WorldRoute` enqueues so a
    /// size-aware dynamic repath routes around clusters the bot's whole body
    /// (not just its center) would clip.
    pub radius_subtiles: i32,
    pub main_tile: IVec2,
    pub main_tile_changed: bool,
    pub floor: i32,
    pub charge: f32,
    pub missing_charge_pct: f32,
    pub depleted: bool,
    pub broken: bool,
    pub passability: &'a Hypermap<f32>,
    pub interactive: &'a InteractiveEntityMap,
    /// Occupancy views for the bot-on-bot subtile detour; `None` disables it.
    pub avoidance: Option<AvoidanceViews<'a>>,
    /// `true` when the bot is on a rendered chunk. Proactive look-ahead
    /// avoidance ([`FollowPath`](low_level::FollowPath)) runs only on-screen —
    /// off-screen bots advance without collision, so probing is wasted work.
    pub on_screen: bool,
    /// In-game event log, present **only for the currently selected bot**, so
    /// movement/brain code can trace its decisions (stuck, escape, step-aside,
    /// detour) to the on-screen log. `None` for every other bot — keeps the log
    /// readable and the trace cost off the hot path. See `Brain::trace`.
    pub trace: Option<&'a GameLog>,
    /// The bot's fixed patrol route, surfaced from its
    /// [`Patrol`](crate::actor::black_bot::Patrol) component for
    /// [`GoToPatrol`](high_level::GoToPatrol). `None` (or empty) for non-patrol
    /// bots and most tests, which disables patrolling.
    pub patrol_loop: Option<&'a [(i32, i32)]>,
    /// Async pathfinding handles. `None` disables route requests (most unit
    /// tests that exercise only movement / priority selection).
    pub pathfind: Option<PathfindAccess<'a>>,
    /// Fixer-only context (dispatch board, home depot, carried part). `Some` only
    /// for [`Fixer`](crate::actor::black_bot::BotSpecialization::Fixer) bots.
    pub fixer: Option<FixerContext<'a>>,
    /// One-shot flag from [`Brain::take_dynamic_repath`]: when `true` the first
    /// `WorldRoute` enqueued this tick must use `include_dynamic: true` so it
    /// routes around current creature positions (post collision-pressure relocation).
    pub dynamic_repath: bool,
}

impl BrainContext<'_> {
    /// Pushes a diagnostic line to the in-game log **iff this is the selected
    /// bot** (`trace` is `Some`). No-op otherwise — so call sites can trace
    /// freely without gating. Forced, so it shows even with the panel collapsed.
    #[inline]
    pub fn trace(&self, text: impl Into<String>) {
        if let Some(log) = self.trace {
            log.push_world(
                self.main_tile.x,
                self.main_tile.y,
                LogEntry::Message { level: LogLevel::Info, text: text.into() },
                true,
            );
        }
    }
}

/// One-shot gameplay event a high-level action wants logged by the owning system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrainLogEvent {
    /// Wander leg exceeded its Manhattan-distance travel budget.
    WanderDestinationTimedOut { goal: (i32, i32) },
    /// Patrol leg exceeded its budget; the waypoint is skipped.
    PatrolWaypointSkipped { waypoint: (i32, i32) },
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
    /// Set this bot's carried inventory part (fixer picked a part up at the depot).
    pub pickup_part: Option<RepairPart>,
    /// Clear this bot's carried inventory (part delivered or dropped on pre-empt).
    pub clear_inventory: bool,
    /// Repair `part` on the target bot (reset its wear and clear the broken flag):
    /// a fixer delivering a part to a stranded bot.
    pub repair_target: Option<(Entity, RepairPart)>,
    /// Recharge the target bot to this charge level (`0.0..=1.0`): a fixer
    /// delivering a [`Battery`](RepairPart::Battery) to a discharged bot.
    pub recharge_target: Option<(Entity, f32)>,
    /// Write `value` into the bot's own [`IntegerMemory`](memory::BotMemory) slot
    /// `id` this tick. Used by high-level actions to update memory through the
    /// effects channel (they don't get `&mut Brain`). Applied by `black_bot_brain`.
    pub integer_memory_write: Option<(IntegerMemoryId, i64)>,
    /// Optional in-game log line for this tick.
    pub log: Option<BrainLogEvent>,
    /// Re-assert that this bot is still actively pursuing a station's queue this
    /// tick (liveness keepalive read by [`InteractiveEntityMap::refresh_queue`]).
    /// Set every tick by an action that holds a queue slot; a despawned or
    /// abandoned bot stops emitting it, so the queue watchdog can evict its stale
    /// membership. The coordinates are informational — refresh is keyed by entity.
    pub queue_keepalive: Option<EntityCoordinates>,
}

fn merge_brain_effects(into: &mut BrainEffects, add: BrainEffects) {
    if let Some(v) = add.queue_want {
        into.queue_want = Some(v);
    }
    if let Some(v) = add.queue_unwant {
        into.queue_unwant = Some(v);
    }
    if let Some(v) = add.queue_wait {
        into.queue_wait = Some(v);
    }
    if let Some(v) = add.queue_unwait {
        into.queue_unwait = Some(v);
    }
    if let Some(v) = add.dock {
        into.dock = Some(v);
    }
    if let Some(v) = add.undock {
        into.undock = Some(v);
    }
    if add.recharge > 0.0 {
        into.recharge += add.recharge;
    }
    if let Some(v) = add.pickup_part {
        into.pickup_part = Some(v);
    }
    if add.clear_inventory {
        into.clear_inventory = true;
    }
    if let Some(v) = add.repair_target {
        into.repair_target = Some(v);
    }
    if let Some(v) = add.recharge_target {
        into.recharge_target = Some(v);
    }
    if let Some(v) = add.integer_memory_write {
        into.integer_memory_write = Some(v);
    }
    if let Some(v) = add.log {
        into.log = Some(v);
    }
    if let Some(v) = add.queue_keepalive {
        into.queue_keepalive = Some(v);
    }
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
    /// Countdown (frames remaining) for the dynamic-repath window. While > 0,
    /// every `WorldRoute` enqueued by a high-level action uses
    /// `include_dynamic: true` so it routes around current creature positions.
    /// Set to [`DYNAMIC_REPATH_FRAMES`] on any genuine wedge frame (including
    /// the collision-pressure relocation); ticks down by 1 each brain tick.
    /// Survives `reset()` — the window must outlive the stuck-detection delay.
    dynamic_repath: u32,
    /// Persistent per-bot memory (see [`memory`]). **Survives `reset()`** — only
    /// the plan is wiped on reset, never the memory.
    memory: BotMemory,
}

/// How many brain ticks (frames) the dynamic-repath window lasts after a
/// collision. Must cover `FollowTuning::stuck_repath_secs` (~60 frames at
/// 60 Hz) plus escape + retry overhead; 120 ≈ 2 s gives comfortable margin.
const DYNAMIC_REPATH_FRAMES: u32 = 120;

impl Brain {
    pub fn new(behaviors: Vec<Box<dyn Behavior>>, factory: HighLevelFactory, rng_seed: u64) -> Self {
        Self {
            behaviors,
            priorities: Priorities::new(),
            current: None,
            low_level: Box::new(Idle),
            factory,
            tuning: FollowTuning::default(),
            rng: rng::seeded(rng_seed),
            rng_seed,
            dynamic_repath: 0,
            memory: BotMemory::default(),
        }
    }

    // --- Memory (persistent; survives `reset()`) ---------------------------

    /// Read-only view of this bot's memory (inspector / read-side helpers).
    pub fn memory(&self) -> &BotMemory {
        &self.memory
    }

    pub fn integer_memory(&self, id: IntegerMemoryId) -> i64 {
        self.memory.integer(id)
    }

    pub fn set_integer_memory(&mut self, id: IntegerMemoryId, value: i64) {
        self.memory.set_integer(id, value);
    }

    /// Adds `delta` to an integer slot and returns the new value.
    pub fn bump_integer_memory(&mut self, id: IntegerMemoryId, delta: i64) -> i64 {
        self.memory.bump_integer(id, delta)
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
    /// Does NOT clear `dynamic_repath` — the flag is set after this call by the
    /// collision-pressure handler and must survive into the next planning tick.
    /// Does NOT clear `memory` — memory is persistent runtime state and several
    /// counters (e.g. [`HelpFailuresCount`](IntegerMemoryId::HelpFailuresCount))
    /// exist precisely to span the resets they count.
    pub fn reset(&mut self) {
        self.current = None;
        self.low_level = Box::new(Idle);
        self.priorities.clear();
    }

    /// (Re-)arm the dynamic-repath window. Call on every genuine wedge frame
    /// (collision while not moving and not recovering) — including the
    /// collision-pressure relocation. Each call resets the countdown to
    /// [`DYNAMIC_REPATH_FRAMES`] so sustained wedging keeps the window alive.
    pub fn set_dynamic_repath(&mut self) {
        self.dynamic_repath = DYNAMIC_REPATH_FRAMES;
    }

    /// Tick the dynamic-repath countdown and return whether the window is
    /// currently active. Called every frame by `black_bot_brain` when building
    /// `BrainContext`; the countdown decrements by 1 so the window expires
    /// automatically after [`DYNAMIC_REPATH_FRAMES`] ticks without a new wedge.
    pub fn take_dynamic_repath(&mut self) -> bool {
        if self.dynamic_repath > 0 {
            self.dynamic_repath -= 1;
            true
        } else {
            false
        }
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
        let mut effects = BrainEffects::default();
        match self.priorities.top() {
            Some(top) => {
                let needs_switch = self.current.as_ref().map(|a| a.kind()) != Some(top.kind);
                if needs_switch {
                    if let Some(action) = self.current.as_mut() {
                        merge_brain_effects(&mut effects, action.preempt(ctx));
                    }
                    self.current = Some((self.factory)(top.kind));
                    self.low_level = Box::new(Idle); // fresh action plans from scratch
                }
            }
            None => {
                if let Some(action) = self.current.as_mut() {
                    merge_brain_effects(&mut effects, action.preempt(ctx));
                }
                self.current = None;
            }
        }

        let mut done = false;
        if let Some(action) = self.current.as_mut() {
            let outcome = action.update(ctx, &mut self.low_level, &mut self.rng);
            merge_brain_effects(&mut effects, outcome.effects);
            done = matches!(outcome.status, HighLevelStatus::Done);
            // Keepalive: while an action still actively holds a station queue slot,
            // re-assert it every tick so the liveness watchdog never evicts a bot
            // that is genuinely pursuing the charger.
            if !done {
                if let Some(coords) = action.active_queue() {
                    effects.queue_keepalive = Some(coords);
                }
            }
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
        self.current.as_ref().map(|a| a.label()).unwrap_or_else(|| "-".to_string())
    }

    pub fn low_level_label(&self) -> String {
        self.low_level.label()
    }

    /// `true` while the bot is coasting in wait of an async pathfind result.
    pub fn is_awaiting_path(&self) -> bool {
        self.low_level.is_awaiting_path()
    }

    /// `true` while the bot is running a collision-recovery maneuver (detour /
    /// step-aside / escape). Collision pressure is suspended during these.
    pub fn is_recovering(&self) -> bool {
        self.low_level.is_recovering()
    }

    pub fn target_tile(&self) -> Option<(i32, i32)> {
        self.low_level.target_tile()
    }

    /// Remaining route as unified nodes plus the active cursor (overlay /
    /// selection gizmos). Consumers read `node.center()` / `node.tile()` and
    /// never branch on cell vs subcell.
    pub fn route(&self) -> Option<(&[PathNode], usize)> {
        self.low_level.route()
    }

    pub fn velocity(&self) -> Vec2 {
        self.low_level.velocity()
    }

    /// The bot's intended movement direction this tick as a unit vector
    /// (`Vec2::ZERO` when it has none). Published to [`ActorState::heading`] by
    /// the owning think system; see [`LowLevelAction::heading`].
    pub fn heading(&self) -> Vec2 {
        self.low_level.heading()
    }

    pub fn stuck_timer(&self) -> f32 {
        self.low_level.stuck_timer()
    }

    pub fn is_stuck(&self) -> bool {
        self.low_level.is_stuck()
    }

    pub fn has_target(&self) -> bool {
        self.low_level.route().map(|(p, i)| i < p.len()).unwrap_or(false)
    }

    pub fn remaining_waypoints(&self) -> usize {
        self.low_level.route().map(|(p, i)| p.len().saturating_sub(i)).unwrap_or(0)
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
            radius_subtiles: 2,
            main_tile: IVec2::ZERO,
            main_tile_changed: true,
            floor: 0,
            charge,
            missing_charge_pct: (1.0 - charge) * 100.0,
            depleted: charge <= 0.0,
            broken: false,
            passability: empty_passability(),
            interactive: empty_interactive(),
            on_screen: true,
            trace: None,
            avoidance: None,
            patrol_loop: None,
            pathfind: None,
            fixer: None,
            dynamic_repath: false,
        }
    }

    pub fn test_state() -> ActorState {
        ActorState {
            center: Vec2::new(0.5, 0.5),
            radius_subtiles: 2,
            rotation: 0.0,
            heading: Vec2::ZERO,
            move_buffer: ActorMoveBuffer::default(),
            last_movement_error: None,
            last_accepted_center_subtile: None,
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
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

    #[test]
    fn memory_survives_reset() {
        let mut brain = black_bot_brain(5);
        brain.set_integer_memory(IntegerMemoryId::HelpFailuresCount, 3);
        brain.reset();
        assert_eq!(
            brain.integer_memory(IntegerMemoryId::HelpFailuresCount),
            3,
            "memory must persist across a plan reset",
        );
    }
}

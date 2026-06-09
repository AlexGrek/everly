//! Background pathfinding service.
//!
//! All bot route searches (and the per-frame bot-on-bot subtile detour) are
//! offloaded from the main thread. A caller [`enqueue`](PathfindQueue::enqueue)s
//! a [`PathKind`] and gets a [`RequestId`]; a dispatcher spawns the actual A\*
//! onto the [`AsyncComputeTaskPool`], keeping at most [`MAX_IN_FLIGHT`] searches
//! running; a collector polls finished tasks and stores their [`PathOutcome`] in
//! [`PathfindResults`]. The caller reads the result back by id.
//!
//! The worker only ever **reads** the shared map data (via the `Arc`-shared
//! [`Hypermap`] / [`DoubleBufferedHypermap`], whose per-chunk `RwLock`s make
//! concurrent reads safe even while the main thread edits geometry) and writes
//! **only** into [`PathfindResults`]. This keeps the hot physics / collision /
//! movement systems free of the heavy search work.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use bevy::math::IVec2;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use futures_lite::future;

use crate::map::hypermap::{DoubleBufferedHypermap, Hypermap};
use crate::map::hypermap_pathfind::{
    astar_shortest_world_path, astar_subtile_detour, simplify_path_line_of_sight,
    HypermapPathResult, HypermapSearchLimits,
};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::passability::{DynamicPassabilityMap, SubtilePassability};
use crate::menu::main_menu::GameState;
use crate::hud::game_log::{GameLog, LogEntry};
use crate::scene::camera::StrategyCamera;

/// Maximum number of pathfinding searches running on the task pool at once.
pub const MAX_IN_FLIGHT: usize = 10;
/// Backlog (queued, not-yet-dispatched requests) above which a throttled warning
/// is logged.
pub const BACKLOG_WARN: usize = 10;
/// Seconds an unconsumed result lingers in [`PathfindResults`] before pruning, so
/// a despawned / re-planned bot's stale result can't accumulate forever.
const RESULT_TTL_S: f32 = 10.0;

/// Opaque, process-unique handle to a queued pathfinding request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// What to search for. Each variant is one A\* query.
#[derive(Clone, Debug)]
pub enum PathKind {
    /// Tile-level world route from `start` to `goal`, simplified with the given
    /// corner buffer.
    WorldRoute {
        start: (i32, i32),
        goal: (i32, i32),
        max_expanded: usize,
        simplify_buffer: usize,
    },
    /// Subtile-level local detour around other creatures (footprint-aware).
    SubtileDetour {
        start: IVec2,
        goal: IVec2,
        pad: i32,
        max_span: i32,
        max_expanded: usize,
        radius: i32,
        blocked_flags: u64,
    },
}

/// Result of a finished [`PathKind`] search.
#[derive(Clone, Debug, PartialEq)]
pub enum PathOutcome {
    /// A simplified world route. `raw_len` is the length of the unsimplified
    /// tile path (callers use `raw_len <= 1` to detect "already at the goal").
    Route { path: Vec<(i32, i32)>, raw_len: usize },
    /// A raw subtile detour path (includes the start subtile; length >= 2).
    Detour(Vec<IVec2>),
    /// No route exists.
    NoPath,
    /// The search hit its expansion budget before proving a result.
    LimitExceeded,
}

/// Pending request queue. Interior-mutable so it can be enqueued through a shared
/// `&` reference from the (single-threaded) brain tick.
#[derive(Resource, Default)]
pub struct PathfindQueue {
    next_id: AtomicU64,
    pending: Mutex<VecDeque<(RequestId, PathKind, Entity)>>,
}

impl PathfindQueue {
    /// Mints a fresh id and appends the request. Returns the id to poll later.
    pub fn enqueue(&self, kind: PathKind, entity: Entity) -> RequestId {
        let id = RequestId(self.next_id.fetch_add(1, AtomicOrdering::Relaxed));
        self.pending
            .lock()
            .expect("pathfind queue poisoned")
            .push_back((id, kind, entity));
        id
    }

    /// Number of requests still waiting to be dispatched.
    pub fn len(&self) -> usize {
        self.pending.lock().expect("pathfind queue poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn pop(&self) -> Option<(RequestId, PathKind)> {
        self.pending
            .lock()
            .expect("pathfind queue poisoned")
            .pop_front()
            .map(|(id, kind, _entity)| (id, kind))
    }

    /// Returns entities that have more than one request pending in the queue.
    /// Used by the backlog warning to detect runaway re-enqueue loops.
    fn find_duplicate_entities(&self) -> Vec<Entity> {
        let pending = self.pending.lock().expect("pathfind queue poisoned");
        let mut counts: HashMap<Entity, u32> = HashMap::new();
        for (_id, _kind, entity) in pending.iter() {
            *counts.entry(*entity).or_insert(0) += 1;
        }
        counts
            .into_iter()
            .filter(|(_e, n)| *n > 1)
            .map(|(e, _)| e)
            .collect()
    }

    /// Test helper: drains every pending request without running dispatch.
    #[cfg(test)]
    pub fn drain_pending(&self) -> Vec<(RequestId, PathKind)> {
        let mut out = Vec::new();
        while let Some(item) = self.pop() {
            out.push(item);
        }
        out
    }
}

/// Finished results keyed by [`RequestId`], with an age used for pruning.
#[derive(Resource, Default)]
pub struct PathfindResults {
    map: Mutex<HashMap<RequestId, (PathOutcome, f32)>>,
}

impl PathfindResults {
    fn insert(&self, id: RequestId, outcome: PathOutcome) {
        self.map
            .lock()
            .expect("pathfind results poisoned")
            .insert(id, (outcome, 0.0));
    }

    /// Removes and returns the outcome for `id` if it has arrived.
    pub fn take(&self, id: RequestId) -> Option<PathOutcome> {
        self.map
            .lock()
            .expect("pathfind results poisoned")
            .remove(&id)
            .map(|(outcome, _)| outcome)
    }

    /// `true` when a result for `id` is available (without consuming it).
    pub fn contains(&self, id: RequestId) -> bool {
        self.map
            .lock()
            .expect("pathfind results poisoned")
            .contains_key(&id)
    }

    fn age_and_prune(&self, dt: f32, ttl: f32) {
        let mut map = self.map.lock().expect("pathfind results poisoned");
        map.retain(|_, (_, age)| {
            *age += dt;
            *age < ttl
        });
    }

    fn len(&self) -> usize {
        self.map.lock().expect("pathfind results poisoned").len()
    }

    /// Test helper: pre-install a finished outcome so brain state-machine tests
    /// can drive the await→consume flow without the task pool.
    #[cfg(test)]
    pub fn insert_for_test(&self, id: RequestId, outcome: PathOutcome) {
        self.insert(id, outcome);
    }
}

/// Tasks currently running on the pool.
#[derive(Resource, Default)]
pub struct PathfindInFlight {
    tasks: Vec<(RequestId, Task<PathOutcome>)>,
}

impl PathfindInFlight {
    pub fn len(&self) -> usize {
        self.tasks.len()
    }
}

/// Ordering anchor so the brain can run between collect and dispatch.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathfindSet {
    /// Polls finished tasks into [`PathfindResults`] (before the brain reads them).
    Collect,
    /// Spawns new searches from the queue (after the brain enqueues them).
    Dispatch,
}

pub struct PathfindServicePlugin;

impl Plugin for PathfindServicePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PathfindQueue>()
            .init_resource::<PathfindResults>()
            .init_resource::<PathfindInFlight>()
            .add_systems(
                Update,
                (
                    pathfind_collect.in_set(PathfindSet::Collect),
                    pathfind_dispatch.in_set(PathfindSet::Dispatch),
                )
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

/// Drains finished tasks into [`PathfindResults`] and ages out stale results.
fn pathfind_collect(
    time: Res<Time>,
    mut in_flight: ResMut<PathfindInFlight>,
    results: Res<PathfindResults>,
) {
    let mut still: Vec<(RequestId, Task<PathOutcome>)> = Vec::with_capacity(in_flight.tasks.len());
    for (id, mut task) in in_flight.tasks.drain(..) {
        match future::block_on(future::poll_once(&mut task)) {
            Some(outcome) => results.insert(id, outcome),
            None => still.push((id, task)),
        }
    }
    in_flight.tasks = still;
    results.age_and_prune(time.delta_secs(), RESULT_TTL_S);
}

/// Spawns queued searches onto the task pool, keeping at most [`MAX_IN_FLIGHT`]
/// running. Reads only the `Arc`-shared map snapshots inside each task.
fn pathfind_dispatch(
    queue: Res<PathfindQueue>,
    runtime: Res<HypermapRuntime>,
    dynamic: Res<DynamicPassabilityMap>,
    results: Res<PathfindResults>,
    mut in_flight: ResMut<PathfindInFlight>,
    game_log: Res<GameLog>,
    camera: Query<&StrategyCamera>,
) {
    let pool = AsyncComputeTaskPool::get();
    while in_flight.tasks.len() < MAX_IN_FLIGHT {
        let Some((id, kind)) = queue.pop() else {
            break;
        };
        let task = match kind {
            PathKind::WorldRoute { .. } => {
                let passability = runtime.static_passability_map.clone();
                pool.spawn(async move { compute_world_route(&passability, &kind) })
            }
            PathKind::SubtileDetour { .. } => {
                let dynamic_inner = dynamic.share_inner();
                let static_cache = runtime.static_subtile_cache.clone();
                pool.spawn(
                    async move { compute_subtile_detour(&dynamic_inner, &static_cache, &kind) },
                )
            }
        };
        in_flight.tasks.push((id, task));
    }

    let backlog = queue.len();
    if backlog > BACKLOG_WARN {
        let (wx, wy) = camera
            .iter()
            .next()
            .map(|c| (c.focus.x.floor() as i32, c.focus.z.floor() as i32))
            .unwrap_or((0, 0));
        game_log.push_world(
            wx,
            wy,
            LogEntry::PathfindBacklog {
                queued: backlog,
                in_flight: in_flight.tasks.len(),
                cached: results.len(),
                threshold: BACKLOG_WARN,
            },
            false,
        );
        for entity in queue.find_duplicate_entities() {
            error!(
                "pathfind backlog: entity {:?} has more than one pending request",
                entity
            );
        }
    }
}

/// Runs a [`PathKind::WorldRoute`] query against `passability`. Used by the
/// dispatch task and by tests that fulfil requests synchronously.
fn compute_world_route(passability: &Hypermap<f32>, kind: &PathKind) -> PathOutcome {
    let PathKind::WorldRoute {
        start,
        goal,
        max_expanded,
        simplify_buffer,
    } = *kind
    else {
        return PathOutcome::NoPath;
    };
    match astar_shortest_world_path(passability, start, goal, HypermapSearchLimits { max_expanded })
    {
        HypermapPathResult::Found { path, .. } => {
            let raw_len = path.len();
            let simplified = if raw_len > 1 {
                simplify_path_line_of_sight(passability, &path, simplify_buffer)
            } else {
                path
            };
            PathOutcome::Route {
                path: simplified,
                raw_len,
            }
        }
        HypermapPathResult::NoPath { .. } => PathOutcome::NoPath,
        HypermapPathResult::LimitExceeded { .. } => PathOutcome::LimitExceeded,
    }
}

/// Runs a [`PathKind::SubtileDetour`] query against the shared dynamic occupancy
/// and static subtile cache (READ-only).
fn compute_subtile_detour(
    dynamic_inner: &Arc<DoubleBufferedHypermap<SubtilePassability>>,
    static_cache: &Hypermap<SubtilePassability>,
    kind: &PathKind,
) -> PathOutcome {
    let PathKind::SubtileDetour {
        start,
        goal,
        pad,
        max_span,
        max_expanded,
        radius,
        blocked_flags,
    } = *kind
    else {
        return PathOutcome::NoPath;
    };
    let dyn_map = DynamicPassabilityMap::from_inner(dynamic_inner.clone());
    // The actor's own current footprint is bypassed so it never treats its own
    // body as an obstacle near the start.
    let previous = Some((start, radius));
    let result = astar_subtile_detour(start, goal, pad, max_span, max_expanded, |sub| {
        dyn_map
            .probe_footprint(sub, radius, previous, blocked_flags, static_cache)
            .is_ok()
    });
    match result {
        Some(path) if path.len() >= 2 => PathOutcome::Detour(path),
        _ => PathOutcome::NoPath,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::passability::{DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID};
    use crate::map::test_world::TestWorld;

    #[test]
    fn enqueue_mints_monotonic_ids() {
        let q = PathfindQueue::default();
        let a = q.enqueue(PathKind::WorldRoute {
            start: (0, 0),
            goal: (1, 1),
            max_expanded: 10,
            simplify_buffer: 1,
        });
        let b = q.enqueue(PathKind::WorldRoute {
            start: (0, 0),
            goal: (2, 2),
            max_expanded: 10,
            simplify_buffer: 1,
        });
        assert_eq!(a.0 + 1, b.0, "ids must be monotonic");
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn results_take_consumes_once() {
        let r = PathfindResults::default();
        let id = RequestId(7);
        r.insert(id, PathOutcome::NoPath);
        assert!(r.contains(id));
        assert_eq!(r.take(id), Some(PathOutcome::NoPath));
        assert_eq!(r.take(id), None, "a result is consumed on take");
    }

    #[test]
    fn results_age_out_after_ttl() {
        let r = PathfindResults::default();
        r.insert(RequestId(1), PathOutcome::NoPath);
        r.age_and_prune(0.5, 1.0);
        assert_eq!(r.len(), 1, "fresh result survives one tick");
        r.age_and_prune(0.6, 1.0);
        assert_eq!(r.len(), 0, "result is pruned once it exceeds the TTL");
    }

    #[test]
    fn compute_world_route_finds_path_on_open_map() {
        let passability = Hypermap::new(1.0);
        let kind = PathKind::WorldRoute {
            start: (0, 0),
            goal: (5, 0),
            max_expanded: 2000,
            simplify_buffer: 1,
        };
        let outcome = compute_world_route(&passability, &kind);
        let PathOutcome::Route { path, raw_len } = outcome else {
            panic!("expected a route on an open map, got {outcome:?}");
        };
        assert!(raw_len > 1);
        assert_eq!(path.last().copied(), Some((5, 0)));
    }

    #[test]
    fn compute_world_route_reports_no_path_to_blocked_goal() {
        let passability = Hypermap::new(0.0);
        passability.set(0, 0, 1.0);
        let kind = PathKind::WorldRoute {
            start: (0, 0),
            goal: (3, 0),
            max_expanded: 2000,
            simplify_buffer: 1,
        };
        assert_eq!(compute_world_route(&passability, &kind), PathOutcome::NoPath);
    }

    #[test]
    fn compute_world_route_on_test_world_fixture() {
        let world = TestWorld::load();
        let start = world.walkable_neighbor(64, 64).expect("fixture has walkable tiles");
        let goal = world.walkable_neighbor(70, 64).expect("fixture has walkable tiles");
        let kind = PathKind::WorldRoute {
            start,
            goal,
            max_expanded: 10_000,
            simplify_buffer: 1,
        };
        let outcome = compute_world_route(&world.passability, &kind);
        assert!(
            matches!(outcome, PathOutcome::Route { .. }),
            "fixture walkable tiles should be connected, got {outcome:?}"
        );
    }

    #[test]
    fn compute_subtile_detour_on_clear_map() {
        let dynamic = DynamicPassabilityMap::new();
        let static_cache = Hypermap::new(SubtilePassability::EMPTY);
        let start = IVec2::new(0, 0);
        let goal = IVec2::new(4, 0);
        let kind = PathKind::SubtileDetour {
            start,
            goal,
            pad: 4,
            max_span: 40,
            max_expanded: 4096,
            radius: 2,
            blocked_flags: FLAG_BLOCKED | FLAG_VOID,
        };
        let outcome = compute_subtile_detour(&dynamic.share_inner(), &static_cache, &kind);
        let PathOutcome::Detour(path) = outcome else {
            panic!("expected a detour on a clear map, got {outcome:?}");
        };
        assert!(path.len() >= 2);
        assert_eq!(path[0], start);
    }

    #[test]
    fn drain_pending_matches_enqueue_order() {
        let q = PathfindQueue::default();
        let a = q.enqueue(PathKind::WorldRoute {
            start: (0, 0),
            goal: (1, 0),
            max_expanded: 10,
            simplify_buffer: 1,
        });
        let b = q.enqueue(PathKind::WorldRoute {
            start: (0, 0),
            goal: (2, 0),
            max_expanded: 10,
            simplify_buffer: 1,
        });
        let drained = q.drain_pending();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].0, a);
        assert_eq!(drained[1].0, b);
        assert!(q.is_empty());
    }
}

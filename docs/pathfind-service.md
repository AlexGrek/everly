# Async pathfinding service

Heavy route searches for smart actors are **offloaded** from the main-thread
brain tick into a background queue processed by Bevy's
[`AsyncComputeTaskPool`](https://docs.rs/bevy/0.18.1/bevy/tasks/struct.AsyncComputeTaskPool.html).
The main thread enqueues a [`PathKind`], parks the bot in a
[`PendingPath`](../src/actor/brain/low_level.rs) hold, and polls
[`PathfindResults`] by [`RequestId`] when a finished outcome lands.

Source: [`src/map/pathfind_service.rs`](../src/map/pathfind_service.rs).
Pure A\* helpers (no queue) remain in
[`src/map/hypermap_pathfind.rs`](../src/map/hypermap_pathfind.rs).

## Why

Tile-level world routes (wander, patrol legs, charger scans) and subtile-level
bot-on-bot detours are bounded A\* searches over large hypermaps. Running them
inline inside the sequential `black_bot_brain` loop serialized every bot's
movement whenever one bot replanned. The service keeps **physics, collision,
and movement** on the main thread and limits concurrent searches so backlog is
visible.

## Resources

| Resource | Role |
|----------|------|
| [`PathfindQueue`] | FIFO of pending `(RequestId, PathKind)` pairs. Interior-mutable (`Mutex` deque + `AtomicU64` id minting) so callers enqueue through a shared `&`. |
| [`PathfindInFlight`] | Up to [`MAX_IN_FLIGHT`] (10) `Task<PathOutcome>` handles currently on the pool. |
| [`PathfindResults`] | `HashMap<RequestId, (PathOutcome, age)>` of finished outcomes waiting to be consumed. Stale entries prune after [`RESULT_TTL_S`] (10 s). |

[`PathfindAccess`] (on [`BrainContext`]) bundles `&PathfindQueue` + `&PathfindResults`
for high-level actions. It is `Some` only in the live `black_bot_brain` system;
unit tests that do not need routing pass `None`, or use test helpers on the queue.

## Request / outcome types

### `PathKind` — what to search

- **`WorldRoute`** — tile-level A\* from `start` to `goal` on
  `Hypermap<f32>` static passability, then line-of-sight simplification
  (`simplify_buffer` corner padding). Used by wander, patrol legs, charger
  candidate scans, dock-approach routes, and patrol-loop generation reachability
  checks.
- **`SubtileDetour`** — bounded 4-neighbour A\* on the subtile grid around a
  head-on bot bump, footprint-tested via `DynamicPassabilityMap::probe_footprint`
  + static subtile cache. Used only by [`FollowPath`](../src/actor/brain/low_level.rs).

### `PathOutcome` — what came back

| Variant | Meaning |
|---------|---------|
| `Route { path, raw_len }` | Simplified world tile path. `raw_len` is the pre-simplification length; callers use `raw_len <= 1` for "already at goal". |
| `Detour(Vec<IVec2>)` | Raw subtile staircase (includes start; length ≥ 2). |
| `NoPath` | Proved unreachable within the search budget / geometry. |
| `LimitExceeded` | Expansion cap hit before a proof. |

Consumers **`take`** results (remove on read) so each id is single-use.

The service stays decoupled from the brain's path representation: it returns
`Route { path: Vec<(i32,i32)> }` and `Detour(Vec<IVec2>)` as the search
algorithms naturally produce them. Conversion to the unified
[`PathNode`](../src/actor/brain/path.rs) (`Route` → `Cell` nodes, `Detour` →
spliced `Sub` nodes) happens **only at the install boundary** inside
[`FollowPath`](../src/actor/brain/low_level.rs) / the high-level actions — the
async layer never depends on `PathNode`.

## Frame schedule

[`PathfindServicePlugin`] registers two systems in `Update` (gated on
`GameState::InGame`):

```
pathfind_collect   (PathfindSet::Collect)   — poll finished tasks → PathfindResults; age/prune
        ↓
black_bot_brain    — behaviors tick; high-level actions enqueue + take results
        ↓
pathfind_dispatch  (PathfindSet::Dispatch)  — dequeue up to 10 searches → AsyncComputeTaskPool
```

`black_bot_brain` is explicitly ordered
`.after(PathfindSet::Collect).before(PathfindSet::Dispatch)` so a bot can
enqueue this frame and read a result that finished last frame before new work is
spawned.

Dispatch snapshots map data for workers:

- **`WorldRoute`** — `HypermapRuntime::static_passability_map` (`Arc` clone).
- **`SubtileDetour`** — `DynamicPassabilityMap::share_inner()` (read buffer) +
  `static_subtile_cache` (`Arc` clone).

Workers **only read** those shared structures (per-chunk `RwLock`s inside the
hypermap). They **never** mutate passability or occupancy. The only write path
is `pathfind_collect` inserting into `PathfindResults`.

When the pending queue length exceeds [`BACKLOG_WARN`] (10), **every frame**
`pathfind_dispatch` pushes a [`LogEntry::PathfindBacklog`](../src/hud/game_log.rs)
to the in-game log on the camera's hypertile (yellow `warn` line). There is no
console logging and no throttle — the check runs each dispatch tick.

## Bot integration

### Waiting-for-path (`PendingPath`)

When a high-level action enqueues a route it swaps the low-level action to
[`PendingPath`], carrying the previous action's **velocity** so the bot coasts to
a stop under inertia instead of snapping still. `PendingPath` never finishes on
its own; the owning high-level action owns retry timing.

### High-level state machines

| Action | Async pattern |
|--------|----------------|
| [`GoToRandomPoints`] | Sample walkable goal → enqueue `WorldRoute` → `PendingPath` → `take` → `FollowPath` or resample. |
| [`GoToPatrol`] | Enqueue `WorldRoute` to next loop waypoint → `PendingPath` → `FollowPath`. |
| [`GoToChargeStation`] | **Seeking:** enqueue one `WorldRoute` per nearby charger candidate → rank resolved routes by cost + queue depth → `FollowPath`. **WaitingQueue:** enqueue dock-approach `WorldRoute` when cleared to approach. |
| Patrol loop generation (`Patrol` component) | `enqueue_patrol_candidates` issues reachability `WorldRoute`s from the anchor; `assemble_patrol_loop` builds the fixed loop once results resolve (or after a generation timeout). |

If no result arrives within **`PATH_WAIT_RETRY_S` (3 s)**, the action reissues the
request (new `RequestId`, fresh `PendingPath`).

[`FollowPath`](../src/actor/brain/low_level.rs) subtile detours follow the same
pattern: enqueue `SubtileDetour` on a head-on bump that chose detour, hold while
`detour_request` is set, install the detour or fall back to step-aside on
`NoPath`/timeout.

Replanning (`stuck` / `finished` handlers) still uses the **rising edge** latch
documented in [`actor-brain.md`](actor-brain.md) — one replan episode per stall,
not per frame.

## Determinism

- **Bot RNG** remains deterministic: `black_bot_brain` is sequential and each
  bot keeps a seeded `StdRng`.
- **Pathfinding completion order** is *not* frame-exact across runs. Tasks on
  `AsyncComputeTaskPool` may finish in different orders depending on core
  scheduling. Correctness is preserved (`RequestId` routing, single `take`), but
  *when* a route lands can shift by a few frames. Do not write golden tests that
  assume synchronous, same-frame path installation.

For reproducible route *geometry*, test
[`compute_world_route`](../src/map/pathfind_service.rs) /
[`compute_subtile_detour`](../src/map/pathfind_service.rs) or the pure helpers in
[`hypermap_pathfind.rs`](../src/map/hypermap_pathfind.rs) directly — not the
live task pool.

## Testing split

| Suite | Location | Asserts |
|-------|----------|---------|
| **Brain** | `src/actor/brain/*` tests | Correct `PathKind` enqueued (`start`/`goal`, candidate count), `PendingPath` while waiting, phase transitions when results are **injected** via `PathfindResults::insert_for_test`. Does **not** run real A\*. |
| **Service** | `src/map/pathfind_service.rs` tests | Queue id monotonicity, result take/TTL, `compute_world_route` / `compute_subtile_detour` on hand-built maps and [`TestWorld`](test-world.md). |
| **Algorithms** | `src/map/hypermap_pathfind.rs` tests | Simplification, doorway handling, subtile detour geometry — unchanged. |

Test helpers (cfg(test) only): `PathfindQueue::drain_pending`,
`PathfindResults::insert_for_test`.

## Adding a new routed behavior

1. Enqueue the appropriate `PathKind` through `ctx.pathfind` (never call
   `astar_*` inline from a brain tick).
2. Park the bot in `PendingPath::with_velocity(low.velocity())`.
3. Poll `results.take(id)` each tick; on success install `FollowPath` (or handle
   `NoPath` / retry).
4. Brain unit test: assert the enqueue shape. Service or `hypermap_pathfind`
   test: assert the geometry if needed.

See also [`docs/actor-brain.md`](actor-brain.md) for the full brain stack.

[`PathfindQueue`]: ../src/map/pathfind_service.rs
[`PathfindResults`]: ../src/map/pathfind_service.rs
[`PathfindInFlight`]: ../src/map/pathfind_service.rs
[`PathfindServicePlugin`]: ../src/map/pathfind_service.rs
[`RequestId`]: ../src/map/pathfind_service.rs
[`PathfindAccess`]: ../src/actor/brain/mod.rs
[`BrainContext`]: ../src/actor/brain/mod.rs
[`GoToRandomPoints`]: ../src/actor/brain/high_level.rs
[`GoToPatrol`]: ../src/actor/brain/high_level.rs
[`GoToChargeStation`]: ../src/actor/brain/high_level.rs
[`MAX_IN_FLIGHT`]: ../src/map/pathfind_service.rs
[`BACKLOG_WARN`]: ../src/map/pathfind_service.rs
[`RESULT_TTL_S`]: ../src/map/pathfind_service.rs

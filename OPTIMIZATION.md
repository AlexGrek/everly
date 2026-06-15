# OPTIMIZATION.md

Performance rules and the log of applied optimizations for **Everly**.

Read this before any performance-sensitive work (per-frame hot paths, locking,
parallelism, large data structures). When you land an optimization, append it to
[Applied optimizations](#applied-optimizations) with file references so the next
agent inherits the context instead of re-deriving it.

The goal is a simulation where **per-frame work is lock-free at steady state,
hypertile-local, allocation-free, and parallelizable**. The rules below all serve
that goal.

## General rules

1. **No process-global lock on a per-frame hot path.** A single global
   `Mutex`/`RwLock` acquired per actor (or per cell) serializes *all* parallel
   work, even when the protected data never changes after first build. Prefer
   lock-free reads: `OnceLock` slot tables indexed by a small key, atomics, or
   immutable `&'static` data baked once and leaked. Keep a lock only for a rare,
   off-hot-path fallback (e.g. a pathological input size). Even an uncontended
   `RwLock::read()` does a shared-atomic RMW that cache-line-bounces across cores
   under many-reader load — for read-mostly tables prefer a lock-free read view.
   Tools proven here: **`arc-swap::ArcSwap<Arc<Map>>`** for a read-mostly table
   with rare *wholesale* replacement (lock-free `load`, atomic single-pointer
   `store` keeps a wholesale swap atomic for concurrent readers — see the
   hypermap chunk table); **`papaya::HashMap`** for a table with concurrent
   per-entry inserts/removes from many threads (use an `AtomicU32`/`AtomicU64`
   field for any value mutated through `retain`'s shared `&V`); and Bevy's
   **`bevy::utils::Parallel<T>`** (thread-local queues) instead of a `Mutex<Vec>`
   to collect from inside a `par_iter_mut`.

2. **Be hypertile-local: resolve a chunk once, not once per cell.** Hypermap
   access costs a global chunk-table lock + `HashMap` lookup + `Arc` clone to
   reach a chunk. A compact region (an actor footprint, a small query) almost
   always lives in **one** chunk, so resolve the chunk handle once and reuse it
   for every cell in that region. Group or cache by chunk; never pay per-cell
   table resolution for work that touches a single hypertile.

3. **Read by reference; never clone a large tile to read one field.** Tile
   values can be hundreds of bytes (`SubtilePassability` = `[u64; 25]`). Use
   borrowing accessors (`with_*_read`, `get_local`, `get_local_mut`) under the
   chunk lock instead of value-returning `get()` that clones the whole tile.

4. **Allocation-free steady state.** No per-actor / per-frame `Vec` or `HashSet`.
   Use compact value-copy state (`(IVec2, i32)` instead of `Vec<IVec2>`), baked
   immutable lookup tables, and stamp directly into target buffers as you iterate
   rather than materializing an intermediate collection.

5. **Design for parallelism via order-independence.** A per-entity system can run
   on `par_iter_mut` *deterministically* only if its result does not depend on
   processing order. Achieve that with double buffering: reads hit an **immutable
   read snapshot** (flushed once per frame), writes accumulate as **commutative**
   operations (e.g. per-chunk `|=` ORs). Each entity must mutate only its own
   disjoint state. Use `ParallelCommands` for deferred structural changes
   (spawns, marker insert/remove) — one per distinct entity.

6. **Take locks at the finest useful granularity, exactly when needed.** Acquire
   a per-chunk lock only for the cell you touch; do not hold one chunk's guard
   across a whole multi-cell operation if that would block other workers needing
   the same chunk. Fine-grained per-cell locking under a cached handle beats one
   coarse guard held across the loop.

7. **Probe once, commit once.** Don't repeat validation. If a placement was
   already proven clear this frame, commit it with a check-free path rather than
   re-running the collision query. Never stamp intermediate/speculative state
   into a shared buffer that later flushes and corrupts the next frame — probe on
   a side, commit a single final result.

8. **Optimize without changing semantics, and prove it.** A performance change
   must be behavior-identical (same outputs, same error variants, same edge
   cases). Keep/extend unit tests, run the touched suites, and confirm
   `cargo check` is warning-clean. Measure or reason about the win explicitly;
   don't add complexity on a hunch.

## Applied optimizations

### Actor collision core — lock-free, chunk-local, parallel (2026-05)

The actor per-frame pipeline (`flush_actor_occupancy → think → process_actors →
field interactions`) and its collision core in
[`src/map/passability.rs`](src/map/passability.rs) /
[`src/map/hypermap.rs`](src/map/hypermap.rs) had four blockers, fixed together.
Full rationale: [`docs/actor.md`](docs/actor.md) §§ "Lock-free, chunk-local
collision" and "Parallel actor processing".

1. **Lock-free circle-shadow cache** (rule 1).
   [`baked_circle_shadow`](src/map/passability.rs) previously took a process-wide
   `Mutex<HashMap>` ~7× per actor per frame — the hard blocker to parallelizing
   `process_actors`. Replaced with a `static [OnceLock<&'static CircleShadow>;
   32]` slot table indexed by radius (single atomic load on the warm path); a
   locked map remains only for radii ≥ 32 (never hit in practice).

2. **Chunk-local footprint access** (rules 2, 3, 6).
   The footprint loop in
   [`probe_footprint`](src/map/passability.rs) / `write_circle` re-resolved the
   chunk (global table lock + `Arc` clone) and **cloned the 200-byte
   `SubtilePassability`** for *every* subtile, on both maps, ×3 probes. Added
   `SubtileReadCursor` / `SubtileWriteCursor` that cache the chunk handle (also
   caching *missing* chunks) so a footprint resolves its single chunk once
   (~26× fewer table locks + `Arc` clones) and reads cells by reference. Added
   [`HypermapChunk::get_local_mut`](src/map/hypermap.rs) for in-place write
   without copying the tile in and out.

3. **No redundant re-probe in BlackBot** (rule 7).
   [`BlackBot::try_move`](src/actor/black_bot.rs) probes X-only and Y-only for
   wall-sliding, then committed via `try_update_footprint`, which *re-probed*.
   For axis-aligned `final_shift` (idle and single-axis steps — most frames) that
   placement was already probed, so it now commits via the new check-free
   [`DynamicPassabilityMap::commit_footprint`](src/map/passability.rs). Only a
   genuinely-unprobed diagonal `final_shift` still runs the full check.

4. **Parallel `process_actors`** (rule 5).
   [`process_actors`](src/actor/mod.rs) is now `par_iter_mut` + `ParallelCommands`
   (deferred `OffScreenActor` marker changes). Deterministic: reads hit the
   immutable read buffer, writes are commutative per-chunk ORs, each actor
   mutates only its own state. Enabled by fixes 1–2 removing all global hot-path
   locks. Per-bot RNGs live in the `*_think` systems (still sequential), so
   randomness is unaffected.

Net effect: the collision hot path holds **no process-global lock** — only
fine-grained per-chunk locks taken exactly when a cell is touched — and the actor
loop scales across cores. Verified: `cargo test` full suite green, `cargo check`
warning-clean, semantics byte-identical.

### BlackBot steering — single sqrt in `approach_velocity` (2026-05)

Audited the new mass/inertia movement code in
[`src/actor/black_bot.rs`](src/actor/black_bot.rs) (`black_bot_think`,
`drive_velocity`, `approach_velocity`, reroute shuffle). The steady-state
moving path was already allocation-free and lock-free — `passability` is
resolved once before the loop, the reroute `candidates` is a stack `[Vec2; 3]`,
and A\*/`world_tile_walkable` only fire on cold repath/reroute branches; the
`*_think` system stays intentionally sequential for per-bot RNG determinism.

One micro-fix (rule 8): [`approach_velocity`](src/actor/black_bot.rs) computed
two square roots per moving bot per frame — `dv.length()` for the threshold
then `dv.normalize()` recomputing it. Now computes `len = dv.length()` once and
steers via `velocity + dv * (max_step / len)` (algebraically identical to
`dv.normalize() * max_step`). Behavior pinned by the `approach_velocity_*` unit
tests (ramp / snap / decel). Small constant win on per-bot-per-frame math; no
semantic change.

### GPU temperature diffusion — chunk-local pack, order-independent kernel (2026-05)

New GPU compute temperature spread
([`src/map/temperature_diffusion.rs`](src/map/temperature_diffusion.rs),
`docs/temperature-diffusion.md`) runs per-frame while in-game. Designed against the rules:

- **Rule 2 (hypertile-local):** [`pack_window`](src/map/temperature_diffusion.rs) resolves each
  chunk handle **once** (`with_chunk_read` on the temperature read map + passability) and fills the
  whole 128×128 region under that one lock — never a per-tile `get()` (which would take the global
  chunk-table lock + `HashMap` lookup per tile, ~65k times/frame).
- **Rule 4 (allocation-free steady state):** the pack `temps`/`mask` `Vec`s live in a reused
  `DiffusionScratch` resource sized once to `WINDOW_CAPACITY`; the per-frame upload overwrites the
  storage-buffer asset's existing bytes in place (`bytemuck::cast_slice` → `data[..].copy_from_slice`),
  no per-frame `Vec` allocation on the CPU side.
- **Rule 5 (order-independence / parallelism):** the kernel is the textbook parallel pattern — reads
  hit an immutable `src` snapshot, each invocation writes only its own `dst[idx]`; ping-pong A↔B
  across substeps. Trivially parallel on the GPU and bit-reproducible per dispatch.
- **Rule 8 (prove it):** the WGSL is mirrored by `diffusion_step_cpu` in the test module
  (conservation, wall insulation, ambient relaxation); `apply_window_to_read` has a clamp/dirty/
  skip-unloaded round-trip test. Full suite green, `cargo check` warning-clean.

Cost: the window is tiny (visible set ≈ a 2×2-chunk bbox; capacity 3×3 = 384²), so the per-frame
upload + GPU→CPU readback is ~0.6 MB each way — the readback round-trip was an explicit product
choice (CPU stays source of truth). Gated by `SETTLE_FRAMES` so an async result is only applied
while its window is stable (no race, no regression). **Future:** Bevy re-prepares the storage-buffer
asset (possible GPU-buffer reallocation) each frame because we mutate its data; a persistent GPU
buffer written via `RenderQueue::write_buffer` would avoid that churn. Mask is re-uploaded every
frame though it changes only on geometry edits — a `mask_dirty` gate would skip most of those.

### Actor brain layer — allocation-free steady-state planning (2026-05)

Added the OOP brain ([`src/actor/brain/`](src/actor/brain/)) above BlackBot's low-level movement
(`docs/actor-brain.md`); the per-frame `black_bot_brain` tick was designed against the rules:

- **Rule 4 (allocation-free steady state):** `Priorities::clear()` reuses its `Vec` (never shrinks);
  a tick's side effects are a fixed-size `BrainEffects` struct (no `Vec`); the `FollowPath` path `Vec`
  and the A\* / charger BFS allocate only on a cold re-path/seek (same cadence as the previous
  `black_bot_think`), not on steady moving frames. The tuned mass/inertia steering
  (`approach_velocity`, `drive_velocity`) and stack-only bot-on-bot reroute moved **verbatim** into
  [`FollowPath`](src/actor/brain/low_level.rs), preserving the prior single-sqrt steering math.
- **Sequential by necessity (rule 5 caveat):** `black_bot_brain` mutates the `InteractiveEntityMap`
  resource (charger dock/undock) and owns the per-bot RNG, so it stays a sequential `for` loop —
  exactly as the old think system did. The parallel `process_actors` collision stage is untouched, so
  there is no parallelism regression.

Verified: full lib suite green (414 passed), `cargo check --all-targets` warning-clean.

### BlackBot status visual cache bounded to live entities (2026-06)

Follow-up optimization for the new stuck/broken color sync in
[`src/actor/black_bot.rs`](src/actor/black_bot.rs)
(`sync_black_bot_status_visual`):

- **Issue (rule 4):** The per-system `Local<HashMap<Entity, (bool, bool)>>`
  cached the last seen `(control_plane_broken, stuck)` state but never removed
  despawned entities. In long sessions with actor spawn/despawn churn, this
  map grew monotonically, adding unnecessary hash work every frame and
  violating allocation-free steady-state expectations.
- **Fix:** After processing live bots, prune cache entries with
  `last_status.retain(|entity, _| bots.get(*entity).is_ok())`, so retained
  state remains proportional to currently alive BlackBots.
- **Semantics:** Visual behavior is unchanged for live entities; only stale
  dead-entity cache rows are discarded.

Verified green: `cargo test -p everly` (183 passed, 0 failed, 2 ignored) and
`cargo check -p everly` warning-clean.

### BlackBot stuck check — O(1) queue-membership lookup (2026-06)

The overhauled BlackBot stuck detection in
[`FollowPath::execute`](src/actor/brain/low_level.rs) gates the stuck timer on
"is this bot waiting in any charger queue?" via
`InteractiveEntityMap::is_in_any_queue`. That query is called **once per moving
BlackBot per frame** (the path follower's hot path).

- **Issue (rules 2/4):** the first implementation scanned **every** station's
  `wanting`/`waiting` `VecDeque`s with a linear `contains`, so the per-frame cost
  was `bots × stations × queue_len`. The common case — a wandering bot, never
  enqueued — paid the full map scan only to return `false` every frame, scaling
  badly with station count.
- **Fix:** added a ref-counted reverse index `queued_actors: HashMap<Entity, u32>`
  to [`InteractiveEntityMap`](src/map/interactive_entity.rs) counting each actor's
  `(station, queue)` memberships. `is_in_any_queue` is now a single
  `contains_key` (O(1)). The count is maintained only at the cold queue-mutation
  sites (`add_wanting`/`add_waiting`/`remove_*`/`remove_actor_from_queues`/
  `clear_queues_at`/`evict_actor_everywhere`/`clear`), each adjusting the index by
  exactly the number of memberships it actually added/removed.
- **Semantics (rule 8):** membership answers are identical to the old scan;
  `queued_actors` is not serialized and rebuilds empty on load, matching the
  already-non-persisted queues. Pinned by
  `is_in_any_queue_tracks_membership_across_operations` (promotion, multi-station,
  eviction, per-station clear).

Verified green: `cargo test -p everly` (201 passed, 0 failed, 2 ignored) and
`cargo check -p everly` warning-clean.

### FPS counter — allocation-free steady state (2026-06)

New FPS counter HUD overlay in [`src/hud/game_hud.rs`](src/hud/game_hud.rs)
(`update_fps_counter`) originally called `format!("{fps} fps")` and assigned
the resulting `String` **every frame** — a per-frame heap allocation violating
rule 4 even when the displayed integer was unchanged.

- **Fix:** added a `Local<u32>` tracking the last displayed fps. The `format!`
  and text write are now gated on `fps != *last_fps`, so the system is a no-op
  on steady-framerate frames (one integer compare, zero allocations). The
  `String` is still allocated only when the fps integer changes (~1 Hz at 60 fps).
- **Rule 4.** No semantic change: the counter still shows instantaneous fps.

Verified green: `cargo test -p everly` (211 passed, 0 failed, 2 ignored) and
`cargo check -p everly` warning-clean.

### Patrol loop generation — back off on empty result (2026-06)

The lazy patrol-loop generation in
[`black_bot_brain`](src/actor/black_bot.rs) (the per-frame, per-bot brain loop)
re-ran [`generate_patrol_loop`](src/actor/brain/high_level.rs) whenever
`Patrol::loop_tiles` was empty.

- **Issue (rules 4/7 — repeated expensive probe on a per-frame path):**
  `generate_patrol_loop` runs A\* up to `PATROL_GEN_ATTEMPTS` (64) times
  (`max_expanded` 2000 each). A normal bot fills its loop on the first
  operational frame and never retries — but a bot whose anchor has **no**
  reachable waypoint within `PATROL_RADIUS` (boxed-in, or a chunk still
  streaming in) returned empty, leaving `is_empty()` true, so the full 64-search
  sweep re-ran **every frame, forever** — a per-frame A\* storm scaling with the
  number of trapped patrol bots.
- **Fix:** added `Patrol::retry_cooldown` (`f32`, `Default` `0.0`). An empty
  result sets it to [`PATROL_RETRY_COOLDOWN_SECS`] (0.5 s); while positive it just
  decrements by `dt` and skips generation. So a stuck bot retries ~twice/second
  instead of ~60×/second (≈60× fewer A\* sweeps on that degenerate path).
- **Semantics (rule 8):** the normal case is **identical** — `retry_cooldown`
  starts at `0.0`, so the first frame attempts immediately and a reachable anchor
  fills the loop with no retry ever. Only the unreachable-anchor path changes,
  and only in *timing* (loop appears up to 0.5 s later once the area becomes
  reachable). `Patrol` is not serialized, so the new field needs no migration.

Verified green: `cargo test -p everly` (212 passed, 0 failed, 2 ignored) and
`cargo check -p everly --all-targets` warning-clean.

### In-game event log — hypertile-local queues, deferred rendering (2026-06)

New event-log overlay ([`src/hud/game_log.rs`](src/hud/game_log.rs),
`docs/game-log.md`) records gameplay events (bot reroute, charging) and shows
them top-left. Designed against the rules so logging never taxes the actor hot
path:

- **Rule 1/6 (no global hot-path lock; finest granularity, briefly held):**
  `GameLog` stores per-hypertile queues in
  `RwLock<HashMap<ChunkCoord, Mutex<ChunkLog>>>`. The warm push (queue already
  exists) takes only the **read** lock to reach the chunk's `Mutex`, held just
  for the single push; the **write** lock is taken only on the cold path that
  first creates a hypertile's queue. `enabled` is an `AtomicBool`, so the whole
  resource is reached through a shared `Res<GameLog>` and can be written from any
  system (the charging push lives in the sequential `black_bot_brain`; the design
  also admits parallel callers without exclusive access).
- **Rule 2 (hypertile-local):** events are grouped by `ChunkCoord`; only the
  queue for the hypertile the camera is on is ever rendered. Every other chunk's
  events stay as structs and age out unrendered.
- **Rule 4 (allocation-free push / deferred string work):** `push` stores a
  plain `LogEntry` struct (copied values) — **no `format!` at push time**.
  Strings are produced by `LogEntry::render` only when actually displayed, then
  cached on the entry so they are never re-rendered. While the panel is off,
  only FORCE-flagged lines are rendered; the UI rebuilds only when the shown chunk's
  queue changed or the camera crossed into a new hypertile. Queues are capped
  (`MAX_LOGS_PER_CHUNK`) so a busy/off-screen chunk can't grow unbounded.

Verified green: `cargo test -p everly` (217 passed, 0 failed, 2 ignored;
including 5 new `game_log` tests) and `cargo check` warning-clean.

### Stuck / escape replan — rising edge, no per-frame A\* storm (2026-06)

Follow-up to the stuck-detection overhaul and escape-before-reschedule maneuver in
[`src/actor/brain/low_level.rs`](src/actor/brain/low_level.rs) /
[`src/actor/brain/high_level.rs`](src/actor/brain/high_level.rs).

- **Issue (rules 4/7 — repeated expensive probe on a per-frame path):**
  `Wait::retry` reports both `is_stuck()` and `is_finished()` for every frame
  once its stall timer fires. `GoToPatrol`, `GoToRandomPoints`, and the
  `GoToChargeStation` stuck handler all keyed replanning on
  `low.is_stuck() || low.is_finished()` with **no rising edge**, so a single
  trapped patrol bot re-ran up to `PATROL_LOOP_LEN` tile A\* searches (or
  `MAX_TARGET_ATTEMPTS` wander searches, or a full nearby-charger scan) **every
  frame** while stalled. `black_bot_brain` is intentionally sequential (per-bot
  RNG + `InteractiveEntityMap` mutation), so one wedged bot serialized the entire
  brain loop and made every other bot's movement appear slower even at a steady
  FPS.
- **Fix:** added `low_level_needs_replan` — replan only on the **rising edge**
  of stuck or finished (`(stuck && !prev_stuck) || (finished && !prev_finished)`).
  `GoToPatrol`, `GoToRandomPoints`, and `GoToChargeStation` each latch the
  previous low-level stuck/finished flags across ticks. Sustained
  `Wait::retry` stall now triggers one replan attempt per episode; the wait /
  retry timers are no longer reset every frame by a failed replan reinstall.
- **Semantics (rule 8):** normal path completion and the first frame of a new
  stuck episode still replan immediately; only the sustained-stuck / sustained-
  finished spam is removed. Escape (`find_escape_cell` + `run_escape`) is
  unchanged — it runs once on stall trigger (cold path, ≤81 footprint probes in
  a 9×9 tile window).

Verified green: `cargo test -p everly` (223 passed, 0 failed, 2 ignored) and
`cargo check -p everly` warning-clean.

**Follow-up micro-opts** on the same escape path:

- **`find_escape_cell` ring scan (rule 7):** replaced the full 9×9 brute-force
  loop with Chebyshev-ring expansion (`for_each_chebyshev_ring`) and early exit
  once the best free cell on an inner ring is closer than
  `min_dist2_to_chebyshev_ring` on the next ring. Typical case (current tile
  free via self-bypass) probes **one** footprint instead of up to 81.
- **`run_escape` single `sqrt` (rule 8):** `to_wp.length()` is computed once and
  reused for heading (`to_wp / dist`) and braking — same pattern as
  `approach_velocity`.

Verified green: `cargo test -p everly` (226 passed, 0 failed, 2 ignored) and
`cargo check -p everly` warning-clean.

### Async pathfinding queue — offload A\* from brain tick (2026-06)

[`src/map/pathfind_service.rs`](src/map/pathfind_service.rs) /
[`src/actor/brain/high_level.rs`](src/actor/brain/high_level.rs) /
[`src/actor/brain/low_level.rs`](src/actor/brain/low_level.rs).

- **Issue (rules 4/5 — per-frame allocation + sequential hot-path work):**
  Tile-level routes (wander, patrol legs, multi-charger scans, patrol-loop
  generation) and subtile bot-on-bot detours all ran synchronous `astar_*` inside
  the sequential `black_bot_brain` loop. One replanning bot blocked every other
  bot's movement cadence even when FPS stayed high.
- **Fix:** `PathfindQueue` + `PathfindInFlight` (cap
  [`MAX_IN_FLIGHT`](src/map/pathfind_service.rs) = 10 on `AsyncComputeTaskPool`)
  + `PathfindResults`. Bots enqueue a [`PathKind`](src/map/pathfind_service.rs),
  park in [`PendingPath`](src/actor/brain/low_level.rs) (inertial coast), and
  `take` outcomes by `RequestId`. Workers **read only** `Arc`-shared
  passability / occupancy snapshots; they write **only** finished outcomes back
  through `pathfind_collect`. Schedule:
  `Collect → black_bot_brain → Dispatch`.
- **Semantics (rule 8):** route geometry unchanged (same `compute_world_route` /
  `compute_subtile_detour` helpers as before). Bot RNG stays sequential;
  **frame-exact arrival timing** of async results is not deterministic across
  runs — documented in [`docs/pathfind-service.md`](docs/pathfind-service.md).
- **Tests:** brain suite asserts enqueued requests + injected results; service +
  `hypermap_pathfind` suites assert actual paths.

Verified green: `cargo test -p everly` (239 passed, 0 failed, 2 ignored) and
`cargo check -p everly` warning-clean.

### Lock-free hot-path tables — chunk-table read snapshot, concurrent results map, parallel re-entry queue (2026-06)

Migrated three concurrency-control sites off process-global locks (rule 1), after
auditing every `Mutex`/`RwLock` in `src/`. New deps: `arc-swap = "1"`,
`papaya = "0.2"`.

1. **Hypermap chunk table — lock-free reads + atomic wholesale flush**
   ([`src/map/hypermap.rs`](src/map/hypermap.rs)). `Hypermap<T>.chunks` was a
   single `RwLock<HashMap<ChunkCoord, Arc<RwLock<Chunk>>>>`: **every** chunk
   resolution (parallel actors in `process_actors` + the ≤10 async pathfind
   workers) took the table read lock, and the double-buffered occupancy *write*
   map — drained every frame by `flush()` — re-created each touched chunk under
   the table **write** lock inside the `par_iter_mut` pass. Added an
   `ArcSwap<ChunkTable<T>>` read snapshot beside the authoritative
   `RwLock<HashMap>`: all reads (`get_chunk` / `has_chunk` / `loaded_chunk_count`
   / `loaded_chunks`) `load()` the immutable snapshot lock-free; the write lock is
   taken only on a *structural* change (create / drain / replace), which then
   `republish`es via a single atomic `snapshot.store`. This keeps the read-side
   wholesale flush (`replace_chunks`) **atomic** for the concurrent async workers
   that read the dynamic-occupancy read map across frames — a worker always sees a
   fully-old or fully-new table, never a partial one (the correctness invariant
   the original `RwLock` provided). Rules 1, 2, 6.

2. **`PathfindResults` — concurrent lock-free results map**
   ([`src/map/pathfind_service.rs`](src/map/pathfind_service.rs)). The finished-
   route store was a `Mutex<HashMap<RequestId, (PathOutcome, f32)>>` that all ≤10
   async workers serialized on to insert outcomes (plus the brain's per-frame
   `take` / `age_and_prune`). Now a `papaya::HashMap<RequestId, (PathOutcome,
   AtomicU32)>`: workers insert lock-free. The age is an `AtomicU32` (holding
   `f32` bits) so `age_and_prune` still accumulates `dt` in place through the
   shared `&V` papaya's `retain` hands out — TTL semantics are byte-identical
   (`results_age_out_after_ttl` unchanged). Rule 1.

3. **`process_actors` re-entry queue — Bevy `Parallel<Vec>`**
   ([`src/actor/mod.rs`](src/actor/mod.rs)). The off-screen→on-screen re-entry
   list was a `Mutex<Vec<Entity>>` locked from inside the `par_iter_mut` pass
   (rare transition path, but a lock in the parallel loop). Replaced with
   `bevy::utils::Parallel<Vec<Entity>>` (thread-local queues, no shared lock),
   drained + `sort_unstable`d afterward — the sort already made the result
   order-independent, so behavior is unchanged. Rule 1/5.

Static standalone `Hypermap`s benefit from the same lock-free reads. Left as-is
(intentionally): `PathfindQueue.pending` (`Mutex<VecDeque>`, enqueue+pop both
main-thread/sequential), the tile-field/dirt/temperature `dirty`/`seeded`/
`hydrated` mutexes (sequential per-frame seed/flush systems), and the radius-≥32
shadow-cache fallback (documented cold path, never hit).

Verified green: `cargo test -p everly` (246 passed, 0 failed, 2 ignored) and
`cargo check -p everly --all-targets` warning-clean.

### Arbitrated movement pipeline — sequential owner-grid arbiter replaces contended parallel footprint writes (2026-06)

The actor `move_pass` (the old parallel `try_move`) spiked to ~40 ms under load.
Each on-screen actor OR-stamped its footprint into the dynamic **write** buffer
from inside `par_iter_mut`; with many actors clustered in one hypermap chunk, the
per-chunk `RwLock::write()` taken per subtile serialized every writer on the same
chunk — lock contention, not useful work. Collisions were also resolved a frame
late against the immutable read snapshot, so two actors could step onto the same
cell and overlap until the next flush.

Replaced with a three-phase pipeline
([`src/actor/movement.rs`](src/actor/movement.rs),
[`src/actor/mod.rs`](src/actor/mod.rs), `docs/actor.md`):

- **Propose (parallel, rule 5):** [`propose_actor_moves`](src/actor/movement.rs)
  runs `think`/`prepare`/[`Actor::propose_move`] on `par_iter_mut`. `propose_move`
  validates the step against **static** geometry only via
  [`first_static_block`](src/map/passability.rs) (read-only, lock-free chunk
  reads through the `ArcSwap` snapshot) and writes the proposed footprint into the
  actor's own [`ActorShadow`] — **no** shared dynamic-map writes, so the parallel
  phase takes no contended locks at all.
- **Arbitrate (sequential, deterministic):**
  [`arbitrate_actor_moves`](src/actor/movement.rs) stamps every proposal into a
  reused per-frame **owner grid** (`Hypermap<SubtileOwners>`, one slot index per
  subtile) in entity-sorted order. A contested cell backs the mover off to its
  previous footprint; a deeper conflict recursively backs off the *touched* actor
  (depth-capped, then squeezed). One thread, uncontended per-chunk locks, bounded
  by the visible-actor count — the contended parallel writes are gone.
- **Allocation-free steady state (rule 4):** the [`OccupancyArbiter`] resource
  reuses its owner grid (`OwnerGrid::clear` drops chunks via the new
  [`Hypermap::clear`](src/map/hypermap.rs), same churn profile as the existing
  per-frame `flush`), `records`/`entities`/`squeeze`/`placements` vectors, and the
  per-record cell buffers (`clear` + `extend_from_slice`). No per-actor/per-frame
  allocation.
- **Hypertile-local (rule 2):** `OwnerGrid::first_foreign`/`stamp`/`clear_cells`
  cache the chunk handle, so a compact footprint resolves its chunk once.

Downstream contracts preserved (rule 8): accepted footprints are still stamped
into the `DynamicPassabilityMap` write buffer (`write_footprint`) and flushed, so
the brain's `AvoidanceViews` and the async pathfinder read identical occupancy;
`last_movement_error` still carries `BlockedByOccupancy`/`BlockedByStatic` for the
brain's collision response and pressure tracking. Squeeze/teleport reuses the
existing `resolve_offscreen_collision` re-entry placement. Determinism: the
sequential pass is entity-sorted, so the result is independent of the parallel
propose phase's thread scheduling.

The pure arbiter (`arbitrate` / `back_off`) is unit-tested in `movement.rs`
(contention → lower-entity wins, occupant priority via back-off, depth-cap
squeeze, no ghost ownership after a back-off). Perf timers renamed to
`propose_pass` / `arbitrate_pass`
([`src/hud/perf_timings.rs`](src/hud/perf_timings.rs)).

### Arbitrated movement — compact footprints, lock-free owner grid, foldhash chunk table (2026-06)

Deeper pass on the new pipeline after HUD timers showed `arb_conflict`/`arb_apply`
dominating. Three fixes (the first two supersede the owner-grid implementation
notes in the previous entry):

1. **Footprints are compact `(center, radius)` end-to-end** (rules 4, 3).
   The circle footprint was materialized as `Vec<IVec2>` **four times per actor
   per frame**: `propose_move` filled `shadow.previous` + `shadow.current`
   (both impls), then `arbitrate_actor_moves` copied both into the
   `MoveRecord` cell buffers. All four lists were exactly
   `baked_circle_shadow(radius)` translated by a center — rule 4's literal
   example. Now [`ActorShadow`](src/actor/movement.rs) stores only
   `proposed_center` + `origin` (back-off center), [`MoveRecord`] is plain
   `Copy` data (`current`/`previous` centers + `radius`), and
   [`OwnerGrid`](src/actor/movement.rs) ops expand through the `&'static`
   baked offsets at the point of use. The apply stage stamps via the existing
   check-free [`commit_footprint`](src/map/passability.rs) instead of a cell
   list. `ActorShadow::fill_cells` deleted.

2. **Owner grid is a flat foldhash `HashMap<IVec2, u32>`** (rule 1). The grid
   was first a `Hypermap<SubtileOwners>` (per-cell ArcSwap load + chunk-table
   lookup + `Arc` clone + `RwLock` acquire — all pure overhead on a
   **sequential** pass), then briefly a `std::collections::HashMap` (SipHash:
   ~3× slower per op for 8-byte keys, buying DoS resistance nothing needs
   here). Now `bevy::platform::collections::HashMap` (foldhash), cleared and
   reused each frame. Also folded the entity collect/sort/snapshot into the
   `ArbConflict` timer scope so the HUD accounts for the whole stage.

3. **Hypermap `ChunkTable` switched to the foldhash `HashMap`**
   ([`src/map/hypermap.rs`](src/map/hypermap.rs), rules 1–2). Every chunk
   resolution in the codebase — passability cursors, owner-grid-era lookups,
   pathfind workers, meshing — hashed `ChunkCoord` with SipHash through the
   `ArcSwap` snapshot. Iteration order was already documented as unspecified,
   so the hasher swap is observable only as speed.

Also stamped on the same pass: [`write_footprint`](src/map/passability.rs) now
uses a `SubtileWriteCursor` (chunk resolved once per compact footprint, not per
cell — rule 2); it remains as the cell-list escape hatch while the movement
pipeline itself uses `commit_footprint`.

Verified green: `cargo test -p everly` (250 passed, 0 failed, 2 ignored) and
`cargo check -p everly --all-targets` warning-clean at every step. Docs updated:
`docs/actor.md`, `docs/charge.md`, `.claude/SKILLS/actor-engineer/SKILL.md`.

### Dynamic passability — single-floor chunks + recycled flush (2026-06)

HUD timers attributed `arb_apply` ≈ 1.2 ms/frame to footprint stamping, but the
stamping was innocent: the dynamic occupancy buffer is **dropped and rebuilt
every frame** (`flush()` drains the write map), so each frame's first stamp into
a chunk re-allocated it — `vec![EMPTY; 128 × 128 × 10 floors]` of 200-byte
`SubtilePassability` = **~33 MB of alloc + fill per chunk per frame** to carry a
few dozen meaningful cells. The same churn plausibly drove the system-wide
hitches seen as `propose` wall-clock spikes (allocator pressure / page faults).
Two fixes in [`src/map/hypermap.rs`](src/map/hypermap.rs):

1. **Per-map floor count** (rule 4). `Hypermap`/`HypermapChunk` now carry a
   `floors` field instead of baking `HYPERMAP_FLOOR_COUNT` into the cell index;
   `Hypermap::new_single_floor` / `DoubleBufferedHypermap::new_single_floor`
   allocate one floor. The two `SubtilePassability` maps — the dynamic
   occupancy buffer ([`src/map/passability.rs`](src/map/passability.rs)) and
   `static_subtile_cache`
   ([`src/map/hypermap_world.rs`](src/map/hypermap_world.rs)) — only ever
   address floor 0 and are now flat: 33 MB → 3.3 MB per chunk (the static cache
   saves that **persistently** per loaded chunk). The geometry, `f32`
   pathfinding, style, and field maps genuinely use floors and keep 10.

2. **Chunk recycling with dirty spot-reset** (rules 4, 1). Recycling maps
   (`new_single_floor` double buffers) log written cell indices per chunk
   (`DirtyLog`, saturating to a full refill if most of a chunk is touched).
   `flush()` now returns displaced read chunks to a write-side pool after
   resetting **only the dirty cells** — so the steady state allocates nothing
   and clears ~25 cells per actor instead of memsetting megabytes. A displaced
   chunk still referenced by a concurrent reader (old `ArcSwap` table snapshot
   held by an async pathfind worker) is detected via `Arc::strong_count` and
   dropped instead of reused, preserving the fully-old-or-fully-new snapshot
   invariant exactly. The pool `Mutex` is taken only on chunk creation and
   flush (a handful of ops per frame, uncontended — rule 1's documented
   cold-path exception). `flush_merge` recycles its drained write chunks the
   same way.

Semantics pinned by `recycled_chunk_reads_as_default` (no ghost occupancy
through a reused chunk) and `recycled_pool_chunk_is_reused_not_reallocated`;
the existing passability flush-cycle suite now runs against the recycling
buffer. Verified green: `cargo test -p everly` (252 passed, 0 failed,
2 ignored), `cargo check -p everly --all-targets` warning-clean.

### Propose phase made sequential — off the global ComputeTaskPool (2026-06)

The HUD propose timers showed `prop_par` peaking ~29 ms while `prop_body` (the
summed per-actor work) stayed at ~0.02 ms — i.e. the propose pass was spending
~29 ms doing *no actor work*. Root cause, confirmed from `bevy_tasks`
`task_pool.rs`: `Query::par_iter_mut` runs on the global `ComputeTaskPool` via
`scope(tick_task_pool_executor = true)`. While waiting for its own (tiny)
batches, the calling thread loops on `executor.tick()`, **executing arbitrary
other tasks queued on the global compute pool** (render batching, visibility,
transform propagation, etc.). That foreign runtime is billed to the propose
scope. With the movement pipeline now in `FixedUpdate` at 60 Hz, a slow render
frame runs several catch-up ticks, each absorbing more queued compute — a
self-amplifying, frame-rate-coupled stall where bots appeared to slow in sync
while fps and every instrumented system read ~0.

Fix ([`src/actor/movement.rs`](src/actor/movement.rs)): `propose_actor_moves`
now iterates `actors.iter_mut()` **sequentially on the main thread** instead of
`par_iter_mut`. `ParallelCommands` → `Commands`, `Parallel<Vec<Entity>>` →
the arbiter's reused `reentrants` Vec, per-thread `AtomicU64` timers → plain
locals. The per-bot work is a single static-slide probe (`first_static_block`,
lock-free static-cache reads) — microseconds for the whole crowd — so
single-threaded costs nothing measurable and removes the pool coupling
entirely.

This is a deliberate, documented exception to rule 5 (design for parallelism):
parallelism only pays when the per-entity work is large enough to amortize the
scheduling, and here it is ~0.02 ms total. A *private* `TaskPool` would also
isolate propose, but `par_iter_mut` is hard-wired to `ComputeTaskPool`, so a
custom pool needs manual `unsafe` chunking of a `&mut` query plus extra OS
threads contending for cores — all to parallelize 0.02 ms. Reconsider a private
pool only if per-bot propose work grows heavy (hundreds of bots). The
`prop_par` HUD row is kept as a regression sentinel: it now tracks `prop_body`,
and a divergence means pool coupling was re-introduced.

Verified green: `cargo test -p everly` (252 passed, 0 failed, 2 ignored) and
`cargo check -p everly --all-targets` warning-clean. Docs updated:
`docs/movement.md`, `docs/actor.md`, `.claude/SKILLS/actor-engineer/SKILL.md`.

### Movement collapsed to one sequential pass — no arbitrate split, no jam teleport (2026-06)

Now that propose is sequential, the propose/arbitrate two-system split bought
nothing: the proposal pass already runs on one thread, so collision detection can
be done in the same sweep. Merged `propose_actor_moves` + `arbitrate_actor_moves`
into a single `process_actor_moves`
([`src/actor/movement.rs`](src/actor/movement.rs)) — think + static propose,
owner-grid resolution, apply + commit, off-screen re-entry — and **deleted the
back-off cascade, the depth cap, and the squeeze/teleport jam pool**.

- **Why no jam teleport is needed (rule 8 — semantics simplified, not broken).**
  The resolver (`arbitrate`) now **pre-stamps every actor's currently-occupied
  footprint** before resolving, then each actor in entity order releases its own
  origin and claims its proposed footprint or holds at origin. Two consequences:
  a mover can never claim a cell another actor still holds (the occupant always
  wins, independent of order), and every actor always has its own origin as a
  guaranteed fall-back. So there is never a wedged actor with nowhere to go — the
  entire recursive back-off + squeeze-pool machinery the old design needed to
  rescue jams is gone. Precondition: `previous` footprints are pairwise disjoint
  (last frame's accepted, overlap-free positions). The only residual teleport is
  off-screen re-entry placement (`resolve_offscreen_collision`), which is unrelated
  to jams and required because off-screen actors travel without collision.
- **Allocation-free steady state (rule 4):** `OccupancyArbiter` keeps reusing its
  owner grid + `records`/`entities`/`reentrants` vecs; `MoveRecord` shrank to
  plain `Copy` `{ current, previous, radius, collided, conflict_cell }` (dropped
  `placed`/`placed_previous`/`squeezed`). No per-actor/per-frame allocation.
- **Brain deadlock breaker re-routes instead of teleporting**
  ([`src/actor/black_bot.rs`](src/actor/black_bot.rs)): the sequential model
  guarantees no two bots *overlap* but does not prevent a head-on *deadlock*
  (both hold, each blocks the other). `track_black_bot_collision_pressure` no
  longer teleports a wedged bot — it wipes the plan and forces a full re-path
  against the **dynamic** passability map from the bot's current position
  (`set_dynamic_repath`), so the new route steers around the occupied tiles. A bot
  with no passable detour holds until the blocker clears (deliberate trade-off).
- **Dead perf timers removed:** the nine movement HUD rows (`propose`,
  `prop_par`, `prop_body`, `prop_think`, `prop_slide`, `prop_adv`, `arb_conflict`,
  `arb_apply`, `arb_squeeze`) measured the now-gone phases and were deleted from
  [`src/hud/perf_timings.rs`](src/hud/perf_timings.rs) (`TimedSystem::COUNT`
  23 → 14), along with the `squeeze=` gauge / `squeezed_bots` counter. The
  movement pass no longer does any `Instant` timing.

The pure resolver is unit-tested in `movement.rs` (no-conflict advance, lower-index
wins a contested cell, stationary occupant protected from a mover, follower takes a
leader's freed cell same frame, follower-before-leader one-frame ripple, radius-1
self-step does not self-collide). Docs updated: `docs/movement.md`, `docs/actor.md`,
`docs/actor-brain.md`, `docs/charge.md`, `docs/field-interactions.md`, and the
actor-engineer / field-interactions / bevy-engineer skills.

Verified green: `cargo test -p everly` (272 passed, 0 failed, 2 ignored) and
`cargo check -p everly --all-targets` warning-clean.

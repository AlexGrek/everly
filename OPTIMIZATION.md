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
   off-hot-path fallback (e.g. a pathological input size).

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

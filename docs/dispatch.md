# Dispatch queue & bot inventory (fixer bots)

The **DISPATCH_QUEUE** is a global repair-request board that couples *stranded*
bots to *fixer* bots. Source: [`src/actor/dispatch.rs`](../src/actor/dispatch.rs).
The fixer's decision logic is the [`GoFixBots`](../src/actor/brain/high_level.rs)
high-level action — read [`actor-brain.md`](actor-brain.md) first.

## The loop

1. A BlackBot's **movement engine** or **control plane** breaks (see
   [`charge.md`](charge.md) / wear in `black_bot.rs`). It is now *immobilized* and
   *stranded*.
2. Each frame, `maintain_dispatch_queue` posts/refreshes a [`RepairRequest`] for
   every stranded bot: the most-critical broken [`RepairPart`] plus the bot's
   tile (stable — a stranded bot can't move). It also drops requests for bots no
   longer stranded and releases claims held by despawned fixers.
3. A [`Fixer`](../src/actor/black_bot.rs) bot loitering within **10** Manhattan
   tiles of its home parts depot `claim_nearest`s the closest open request.
4. It travels to the depot, picks up the part (`BotInventory` — a floating marker
   appears above it), drives to the stranded bot, and on coming within **1.5
   tiles** repairs that part (wear → 0, broken flag cleared) — *near but not
   touching*, so it never collides with the target.
5. It returns home and resumes loitering.

## `DispatchQueue` (resource)

Interior-mutable (`Mutex<Vec<RepairRequest>>`) so the sequential brain tick can
claim / release / complete through a shared `&` — the same pattern as
[`PathfindQueue`](pathfind-service.md). Key methods:

| Method | Role |
|--------|------|
| `post(bot, part, loc)` | Upsert a request by bot (preserves its `claimed_by`). |
| `claim_nearest(fixer, from)` | Claim the nearest **unclaimed** request; mark it. |
| `has_open_within(from, r)` | Is there an unclaimed request within `r` tiles? |
| `release(fixer)` | Return a fixer's claim to the pool. |
| `complete(bot)` | Remove a request (repaired / gone). |
| `maintain(broken, alive)` | Drop requests for non-stranded bots; free claims of dead fixers. |

**Claim hygiene.** A claim must never outlive the fixer that holds it. Releases
happen on: `GoFixBots::preempt` (recharge), every forced `brain.reset()` site in
`black_bot.rs` (`release_fixer_work`: squeeze-teleport, offline gate, collision
reset), and `maintain` (despawned claimer). Without this a reset fixer would
orphan its task forever.

## `BotInventory` + marker

Every BlackBot carries a `BotInventory { carried: Option<RepairPart> }` and a
hidden [`InventoryMarker`] cube child. `sync_inventory_markers` (in `dispatch.rs`,
`Update`) floats the cube above the bot, shows it when `carried` is set, and tints
it per part. It is **excluded** from `sync_black_bot_transforms` so it keeps its
above-the-bot offset rather than snapping to the sphere center like the ring.

Only fixers ever fill the inventory today, but the component is on every bot so
"carrying is visible over the bot" is a uniform mechanism.

## Repair application

A fixer's `GoFixBots` returns `repair_target: Some((target, part))` as a
[`BrainEffects`](actor-brain.md). Because it mutates a *different* bot's
`Breakable` than the one being iterated, `black_bot_brain` collects these and
applies them in a **second pass** over its bot query after the main loop (reset
that part's `wear` to 0 and clear `broken`, then log a green "repaired" line).
`pickup_part` / `clear_inventory` mutate the fixer's *own* `BotInventory` inline.

## Spawn rate & visuals

`BotSpecialization::roll` makes `FIXER` the rarest role (**1/8**, vs `PATROL`
1/4 of the rest). Fixers wear a **red ring** (`RING_FIXER`), mirroring the blue
patrol ring.

## Tests

- `dispatch.rs` tests: queue post/claim/release/complete/maintain semantics.
- `high_level.rs` tests: `GoFixBots` enqueues the right routes, claims/releases
  the board, picks up the part, and repairs on proximity (asserts effects + queue
  state, not live A\*). Follows the brain-test split in
  [`pathfind-service.md`](pathfind-service.md).

[`RepairRequest`]: ../src/actor/dispatch.rs
[`RepairPart`]: ../src/actor/dispatch.rs
[`InventoryMarker`]: ../src/actor/dispatch.rs

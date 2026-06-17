# Dispatch queue & bot inventory (fixer bots)

The **DISPATCH_QUEUE** is a global repair-request board that couples *stranded*
bots to *fixer* bots. Source: [`src/actor/dispatch.rs`](../src/actor/dispatch.rs).
The fixer's decision logic is the [`GoFixBots`](../src/actor/brain/high_level.rs)
high-level action — read [`actor-brain.md`](actor-brain.md) first.

A bot is *stranded* in two ways, both immobilizing and both rescued through the
same board:

- its **movement engine** or **control plane** breaks (wear, see
  [`charge.md`](charge.md) / `black_bot.rs`), or
- its **battery discharges** to 0% (see [`charge.md`](charge.md)). A discharged
  bot "asks for help" exactly like a broken one.

## The loop

1. A BlackBot breaks an immobilizing part *or* runs its battery flat. It is now
   *immobilized* and *stranded*.
2. Each frame, `maintain_dispatch_queue` posts/refreshes a [`RepairRequest`] for
   every stranded bot plus the bot's tile (stable — a stranded bot can't move).
   The requested [`RepairPart`] is a **`Battery`** when the bot is discharged
   (charge first — repairs don't help a bot that can't move), otherwise its
   most-critical broken part. It also drops requests for bots no longer stranded
   and releases claims held by despawned fixers.
3. A [`Fixer`](../src/actor/black_bot.rs) bot loitering within **10** Manhattan
   tiles of its home parts depot `claim_nearest`s the closest open request.
4. It travels to the depot, picks up the part/battery (`BotInventory` — a floating
   marker appears above it), drives to the stranded bot, and on coming within
   **1.5 tiles** services it — *near but not touching*, so it never collides with
   the target:
   - a **part** → wear → 0, broken flag cleared (`repair_target` effect);
   - a **`Battery`** → the discharged bot is recharged to a random **50–70%**
     (`recharge_target` effect, rolled from the fixer's seeded RNG). It is a
     partial top-up: the revived bot still seeks a charger for the rest.
5. It returns home and resumes loitering.

A bot that is *both* discharged and broken is rescued in two stages: the battery
request comes first; once it wakes it re-posts for the remaining break.

### Giving up a hopeless task (`HELP_FAILURES_COUNT`)

A fixer keeps its claim + inventory across navigation resets (collision repath,
squeeze-teleport) and recharge detours, so it normally just resumes. But a target
boxed into an unreachable spot would make it retry forever. The brain's
`IntegerMemory[HELP_FAILURES_COUNT]` (see
[`actor-brain.md` § Memory](actor-brain.md#memory)) bounds this:

- `= 0` when the fixer claims a fresh task or completes a delivery;
- `+= 1` on every collision/stall reset while it holds a claim
  (`track_black_bot_collision_pressure`);
- once it exceeds **4**, that reset **gives the task up**: the claim is
  `release`d (posting the request back to the queue for another fixer) and the
  counter is cleared. The part is **kept**, not deleted.

With a part but no claim, `GoFixBots` enters `FixPhase::DropPart`: it routes to the
**nearest reachable depot** (`nearest_reachable_depot`, falling back to the home
depot) and drops the part there (`clear_inventory`) before resuming its routine.
The same path also returns a part the deliver phase abandoned as unreachable.

### Give-up cooldown (no instant re-claim)

Giving a task up must not let the same — or another — loitering fixer **re-claim it
on the very next tick**, or the fixer would camp the depot endlessly fetching a part,
failing to deliver, dropping it, and re-fetching (the "stuck on the depot,
pickup/dropping parts" loop). So a give-up uses `release_with_cooldown(fixer,
FIXER_TASK_COOLDOWN_S)` (6 s) instead of a plain `release`: the request returns to
the pool but is **invisible to `claim_nearest` / `has_open_within`** until its
`cooldown` ticks to zero (`tick_cooldowns`, once per frame in
`maintain_dispatch_queue`). A re-post by the still-stranded bot preserves an active
cooldown. Both give-up paths use it: the deliver-phase `abandon_claim` (target
unreachable) and the collision-pressure give-up (`HELP_FAILURES_COUNT > 4`). The
**offline gate** (`release_fixer_claim`, depleted/broken fixer) keeps the plain,
cooldown-free `release` so another fixer can cover an incapacitated one at once.

The effect: a transiently-blocked target is retried after the cooldown; a
permanently-unreachable one is retried only ~once per cooldown (not every tick), and
between attempts the fixer loiter-wanders away from the depot instead of flickering
pickup/drop on it.

## `DispatchQueue` (resource)

Interior-mutable (`Mutex<Vec<RepairRequest>>`) so the sequential brain tick can
claim / release / complete through a shared `&` — the same pattern as
[`PathfindQueue`](pathfind-service.md). Key methods:

| Method | Role |
|--------|------|
| `post(bot, part, loc)` | Upsert a request by bot (preserves its `claimed_by`). |
| `claim_nearest(fixer, from)` | Claim the nearest **unclaimed** request; mark it. |
| `has_open_within(from, r)` | Is there an unclaimed, off-cooldown request within `r` tiles? |
| `release(fixer)` | Return a fixer's claim to the pool (immediately re-claimable). |
| `release_with_cooldown(fixer, s)` | Release **and** bar re-claim for `s` seconds (give-up). |
| `tick_cooldowns(dt)` | Age out give-up cooldowns (once per frame). |
| `complete(bot)` | Remove a request (repaired / gone). |
| `maintain(broken, alive)` | Drop requests for non-stranded bots; free claims of dead fixers. |

**Claim hygiene.** Releases happen on: the offline gate (`depleted || broken`) in
`black_bot.rs` (`release_fixer_claim`, plain `release`) so another fixer can cover
while this one is incapacitated; `maintain` (despawned claimer); and **giving a task
up** — the deliver-phase `abandon_claim` (unreachable target) and the
collision-pressure give-up (`HELP_FAILURES_COUNT > 4`), both via
`release_with_cooldown` (see [Give-up cooldown](#give-up-cooldown-no-instant-re-claim)).
Navigation resets (squeeze-teleport, collision repath) and recharge pre-emptions do
**not** release the claim — `GoFixBots::update` recovers it from
`dispatch.claim_of(entity)` when `Fixing` becomes the dominant priority again and the
new action starts in Loiter.

**Inventory is permanent.** `BotInventory::carried` is never cleared by brain
resets, pre-emptions, or the offline gate. It is overwritten on the next
`pickup_part` (a new delivery cycle) and cleared only on successful delivery
(`clear_inventory` effect). A fixer that carries a part through a recharge
detour or a navigation reset resumes delivering from where it left off.

## `BotInventory` + marker

Every BlackBot carries a `BotInventory { carried: Option<RepairPart> }` and a
hidden [`InventoryMarker`] cube child. `sync_inventory_markers` (in `dispatch.rs`,
`Update`) floats the cube above the bot, shows it when `carried` is set, and tints
it per part. It is **excluded** from `sync_black_bot_transforms` so it keeps its
above-the-bot offset rather than snapping to the sphere center like the ring.

Only fixers ever fill the inventory today, but the component is on every bot so
"carrying is visible over the bot" is a uniform mechanism.

## Repair / recharge application

A fixer's `GoFixBots` returns either `repair_target: Some((target, part))` or,
for a delivered battery, `recharge_target: Some((target, level))` as a
[`BrainEffects`](actor-brain.md). Because both mutate a *different* bot's
`Breakable` / `Charge` than the one being iterated, `black_bot_brain` collects
them and applies them in **two second passes** over its bot query after the main
loop:

- `repair_target` → reset that part's `wear` to 0 and clear `broken`, log a green
  "repaired" line;
- `recharge_target` → set the target's `Charge` to the delivered level (via
  `Charge::new`, so the `[0,1]` invariant holds), log a green "recharged to N%"
  line. The revived bot leaves the depleted gate on the next tick.

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

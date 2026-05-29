# Test world fixture

`TestWorld` ([src/map/test_world.rs](../src/map/test_world.rs), compiled only
under `#[cfg(test)]`) is the **shared world fixture for game-logic unit tests**.
Load it instead of hand-building tiles whenever a test needs a realistic world —
pathfinding, interactive entities, fields, actor movement, etc.

```rust
use crate::map::test_world::TestWorld;

let world = TestWorld::load();
let path = astar_shortest_world_path(world.passability(), start, goal, limits);
let near = world.entities().find_within_radius(center, 20, None);
```

## What it is

A **procedurally generated 6×6-chunk world** (`768 × 768` tiles, floor 0) saved as
plain level-geometry text under `test_fixtures/level_test_world/geometry/{x}_{y}.txt`
— same on-disk format as `levels/level_*/geometry/`, kept in a separate place so it
is never confused with a playable level. The files are committed; tests only read
them, so loading is fast and deterministic.

`TestWorld::load()` reads the 36 chunks and exposes three views:

| Field | Type | Use |
|-------|------|-----|
| `cells` | `Hypermap<CellType>` | the raw generated tiles (roads, walls/houses, corners, chargers) |
| `passability` | `Hypermap<f32>` | `cell_passability` of every tile; what A* and `find_accessible_within` consume |
| `entities` | `InteractiveEntityMap` | one `ChargerEntity` per `Charger` tile (derived on load — generation does not yet populate the submap) |

Helpers: `any_charger_tile()`, `walkable_neighbor(x, y)`, `passability()`, `entities()`.

## Why 6×6 chunks (not tiles)

Each generated chunk carries the generator's [`CHUNK_VOID_MARGIN`](../src/map/map_generator/types.rs)
void border, so **the 36 chunks are separate connected components**. That is
deliberate: a start tile in one chunk can reach only chargers in that chunk, which
gives `find_accessible_within` a real reachable-vs-unreachable split (chargers in
other chunks are genuinely unreachable) without any hand-editing. The fixture's
own tests exercise all three locators and A* against this property.

## Golden tests against the fixture — verification protocol (MANDATORY)

The three interactive-entity search functions are tested with **stored golden
values** in `test_world.rs` (`golden_locator_values`). These tests are **fragile
by design**: they assert exact coordinate sets, so *any* change to the fixture —
regeneration, reseed, a hand-edit, or simply growing the world — will break them.
That fragility is the point: it forces a human to look before a result silently
drifts. As the 6×6 world gains complexity these *will* fail often, so the rules
below are **mandatory** for every golden/snapshot test written against `TestWorld`.

### Rule 1 — every golden is a pair (store + independently verify)

A golden assertion MUST be written as two tests:

1. A **storing** test — the hand-typed literal equals the output of the function
   under test (`golden_locator_values`).
2. A **verification** test — the *same* literal equals the output of an
   **independent computation that never calls the function under test**
   (`golden_locator_values_are_correct`).

The literal is typed in both places but produced/checked by two different
algorithms. If both agree with the same literal, the result is correct by
cross-confirmation. The independent method to use per function:

| Function under test | Independent verifier |
|---------------------|----------------------|
| `find_within_radius` | brute-force `dx² + dy² ≤ r²` over every entity |
| `find_in_rendered_chunks` | brute-force chunk-membership filter over every entity |
| `find_accessible_within` (BFS) | **A\*** (`astar_shortest_world_path`) to each candidate tile — chargers are walkable, so A\* to the tile is exact |

A storing test added **without** its independent verifier is incomplete and must
not be merged. Never bake a value by copying dump/locator output alone — the dump
came from the very function you are trying to test.

### Rule 2 — when a golden fails, diagnose *before* editing any literal

A failing golden is exactly one of two things. Decide which first:

- **Regression.** You changed locator code (or something it relies on — chunking,
  passability, pathfinding) and the fixture is unchanged → **fix the code, do not
  touch the golden.** Tell-tale: `git status` shows *no* changes under
  `test_fixtures/level_test_world/`, yet the values shifted. If the verification
  test fails while the storing test passes (or vice-versa), the two algorithms
  disagree — that is always a real bug; stop and investigate.
- **Intended fixture change.** You ran `regenerate_test_world_fixture`, reseeded,
  or hand-edited a `geometry/*.txt`, so charger coordinates legitimately moved →
  re-bake (Rule 3). Tell-tale: `git status` shows the fixture changed.

Do **not** regenerate the fixture to "make a red golden go green" — that masks
regressions. Regenerate only when the world genuinely needs to change.

### Rule 3 — re-bake procedure (intended fixture changes only)

1. Print the live locator outputs:
   ```sh
   cargo test -p everly dump_locator_truth -- --ignored --nocapture
   ```
2. **Independently verify each printed set before trusting it** — this is the step
   that keeps the goldens honest:
   - *radius*: pick a center/radius whose answer is small (1–15 entries); confirm
     every returned charger is within distance and no nearer charger is missing.
   - *rendered*: compute the footprint chunks by hand — the center's chunk plus one
     neighbor per axis toward the nearer border (**3 chunks**) — then confirm the
     set is exactly the chargers in those chunks (count them).
   - *accessible*: confirm with A\* and a **structural invariant**, e.g. two
     complementary starts must *partition* a chunk's chargers (reachable sets are
     disjoint and their union is all chargers in that chunk; chargers in other
     chunks are unreachable because of the void margins).
3. Paste the verified sets into **both** the storing literal and the verification
   literal.
4. `cargo test map::test_world` — both must pass. The verification test passing is
   your proof, not the storing test.
5. Commit the fixture change and the golden update **together**, with a message
   stating the fixture was regenerated/edited and why.

### Rule 4 — choose queries that age well

To slow the inevitable churn as the world grows:

- Anchor to a **small** result set (1–15 entries) so the literal is auditable.
- Lean on invariants the generator guarantees (void-margin chunk isolation,
  one-charger-per-room) and assert *those* in the verification test, rather than
  incidental positions — e.g. "these two starts partition chunk (0,0)'s chargers"
  survives reseeding better than a bare coordinate list.
- Keep at least one query exercising each function's distinctive behavior:
  radius = distance, rendered = chunk footprint, accessible = pathfinding +
  isolation.

## Regenerating / hand-editing

Geometry is committed. To rebuild (after a generator change, or to reseed via
[`chunk_seed`](../src/map/test_world.rs)):

```sh
cargo test -p everly regenerate_test_world_fixture -- --ignored
```

Seeds are deterministic, so the output is reproducible. After regenerating you may
hand-edit individual `geometry/{x}_{y}.txt` chunks (plain `# floor 0` token grids,
see [tilemap.md](tilemap.md)) when a test needs a specific scenario — e.g. sealing
a charger off or carving a corridor between two chunks.

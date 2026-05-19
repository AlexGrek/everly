---
name: bevy-engineer
description: >-
  Provides Bevy Engine v0.18 guidance for this project: ECS architecture,
  observers/events, rendering and post-processing, UI and text, cameras, Cargo
  feature sets, and fast-compile setup. Use when writing or refactoring Rust
  game code, systems, plugins, shaders, or Bevy-related Cargo.toml.
paths:
  - "**/*.rs"
  - "**/Cargo.toml"
  - "**/*.wgsl"
  - "docs/corners.md"
  - "docs/tilemap.md"
  - "docs/map-generator.md"
---

# Bevy engineer (v0.18)

## Context

This project targets **Bevy v0.18**. When generating, editing, or refactoring Rust for this repo, follow the v0.18 API, a data-driven ECS architecture, and current Bevy practices.

## Important!

**Always read documentation files before reading any code and write documentation to keep it up to date with all code changes!**

## Observers and events

When registering observers for events, use `On<Event>` (not the older `Trigger<Event>` style).

```rust
// Correct (v0.18)
commands.add_observer(|trigger: On<GameOver>| {
    info!("Game over! Score: {}", trigger.score);
});

// Incorrect (v0.17 and older)
// commands.add_observer(|trigger: Trigger<GameOver>| ...
```

## Entity and component access

For safe mutable access to multiple arbitrary components outside normal queries (scripting, complex interactions, tooling), use the stabilized `get_components_mut::<T>()`:

```rust
let (mut a, mut b) = entity.get_components_mut::<(&mut ComponentA, &mut ComponentB)>()?;
```

## Rendering and post-processing

- **Atmospheres:** Avoid hardcoded earth-only parameters. Bevy 0.18 uses the `ScatteringMedium` asset for procedural scattering (fog, alien atmospheres, and similar).
- **Post-processing:** For typical fullscreen effects, prefer the high-level `FullscreenMaterial` trait and `FullscreenMaterialPlugin` instead of bespoke low-level render features.

```rust
impl FullscreenMaterial for ChromaticAberration {
    fn fragment_shader() -> ShaderRef {
        "shaders/chromatic_aberration.wgsl".into()
    }
    fn node_edges() -> Vec<InternedRenderLabel> {
        vec![
            Node3d::Tonemapping.intern(),
            Self::node_label().intern(),
            Node3d::EndMainPassPostProcessing.intern(),
        ]
    }
}
```

## UI and text

- **Fonts:** Use `TextFont` with `FontFeatures` for OpenType features (for example `FontFeatureTag::STANDARD_LIGATURES`).
- **Interactivity:** UI text sections can be pickable and receive `Observer`s for hyperlink-like behavior.
- **Widgets:** Before custom widgets, prefer built-in logical widgets: `Popover`, `MenuPopup`, `ColorPlane`, `RadioButton` / `RadioGroup` (emits `ValueChange<bool>`).
- **Navigation:** Prefer Bevy’s automatic directional navigation for gamepad and keyboard in UIs.

## Cameras

Avoid one-off fly or pan cameras for dev workflows. Bevy 0.18 ships first-party controllers:

- **3D fly:** `FreeCameraPlugin` and `FreeCamera`.
- **2D pan/zoom:** `PanCameraPlugin` and `PanCamera`.

## Everly map scale (hypermap)

When touching **`src/map/hypermap_world.rs`**, **`src/map/world_map.rs`**, **`src/map/floor_level.rs`**, **`src/edit/map_selection.rs`**, or floor/camera height:

- **1 world unit = 1 m** for map geometry; each tile is **1 m × 1 m** in XZ.
- **Wall slabs:** thickness **`WALL_THICKNESS` = 0.2** (one-fifth of a cell); one thin box per wall bitmask edge via `for_each_wall_segment`.
- **Wall height:** **`HYPERMAP_WALL_HEIGHT` = 3.0** — vertical extent of wall meshes.
- **Storey spacing:** **`HYPERMAP_FLOOR_HEIGHT` = `HYPERMAP_WALL_HEIGHT + 0.03`** — use for camera `focus.y`, upper-floor road quads, and picking floor index; keep wall meshes tied to **`HYPERMAP_WALL_HEIGHT`** only so storeys stay visually separated.
- **Chunk floor meshes** include **every non-void cell** (road **and** wall): a shared floor quad plus separate wall geometry (see `docs/rendering-pipeline.md`).

Authoring reference: **`docs/tilemap.md`**, agent checklist: **`.claude/SKILLS/map-creator/SKILL.md`**.

Procedural chunk generation (`src/map/map_generator/`, `fill_procedural_chunk`): **`.claude/SKILLS/map-generator/SKILL.md`**, **`docs/map-generator.md`**.

Inner union corner pillars (`c*`, concave elbows, `corner_pillars.rs`): **`docs/corners.md`** (with map-generator skill for pipeline step 8).

## Project workflow

- **ECS:** Keep data in `Component`s and logic in systems; avoid god objects.
- **Cargo features:** Prefer Bevy’s high-level feature bundles in `Cargo.toml` (for example 2D, 3D, UI) instead of hand-picking many granular sub-crate features.
- **Compile times:** If not already configured, suggest `lld` or `mold` (or `zld` on macOS) plus Cranelift in `.cargo/config.toml` for faster debug iteration.

## Chunk overlays (`src/map/chunk_overlay.rs`)

The overlay system provides per-chunk, subtile-resolution RGBA textures (640×640 texels = 128 tiles × 5 subtiles) floating above the floor. Two independent layers exist: a **generic** writable canvas and an **occupancy** debug layer (toggled with F4).

To paint the generic layer from any system:

1. Take `Res<ChunkOverlayState>`, `ResMut<Assets<Image>>`, `ResMut<Assets<StandardMaterial>>`.
2. Convert world tile `(wx, wy)` → `(ChunkCoord, LocalCoord)` via `world_to_chunk_local(wx, wy)`.
3. Get the image with `state.image_for(coord)`, write RGBA at byte index `(py * 640 + px) * 4` where `px = local.x * 5 + sx`, `py = local.y * 5 + sy`.
4. **Must** touch the material via `materials.get_mut(state.material_for(coord))` after writes (Bevy issue #20269).
5. Use `state.iter_coords()` to iterate all visible chunk coords (useful for per-frame clears).

Full docs: **`docs/chunk-overlay.md`**. Source: **`src/map/chunk_overlay.rs`**.

## Pathfinding (`src/map/hypermap_pathfind.rs`)

A* on the static passability hypermap (`Hypermap<f32>`, tile walkable iff `> 0.0`). 4-neighbor grid, unit step cost, Manhattan heuristic.

Key functions:

- `astar_shortest_world_path(map, start, goal, limits) -> HypermapPathResult` — bounded A* returning `Found { path, expansions }`, `NoPath`, or `LimitExceeded`. Path includes start and goal as `(i32, i32)` world tile coords.
- `explore_walkable_tiles_limited(map, start, limits) -> HypermapExploreResult` — uniform-cost flood from a tile.
- `HypermapSearchLimits { max_expanded }` — caps node expansions (default 50 000).

Access the static passability map from ECS via `Res<HypermapRuntime>` → `hypermap.static_passability_map`.

Full docs: see pathfinding tests in source. Source: **`src/map/hypermap_pathfind.rs`**.

## Actors (`src/actor/`)

Trait-based actor system. Each actor type implements `Actor` (state accessors, optional `think_low_level`, `blocked_flags` for traversal). Wrapped in `ActorObject` (a `Component` holding `Box<dyn Actor>`).

Per-frame pipeline (run by `process_actors`): clear error → think → prepare → `try_move` (collision gate) → flush passability.

Movement uses **dual channels**: `tile_delta` (float, smooth rendering) and `subtile_shift` (integer, collision grid). Accumulate float displacement across frames; emit integer steps only on subtile boundary crossings. `1 tile = 5 subtiles`.

Actor classes override `blocked_flags()` to declare traversal rules:

| Class | `blocked_flags` | Crosses void? |
|---|---|---|
| Ground walker (default) | `FLAG_BLOCKED \| FLAG_VOID` | No |
| Flyer (`GlitchBot`) | `FLAG_BLOCKED` | Yes |

To add a new actor: follow the checklist in `docs/actor.md#new-actor-checklist`. Read **`.claude/SKILLS/actor-engineer/SKILL.md`** for invariants, coordinate rules, and common pitfalls.

Full docs: **`docs/actor.md`**. Skill: **`.claude/SKILLS/actor-engineer/SKILL.md`**. Source: **`src/actor/mod.rs`**, **`src/actor/glitch_bot.rs`**, **`src/actor/black_bot.rs`**.

## Official references

- Release notes: https://bevy.org/news/bevy-0-18/
- Examples: https://bevyengine.org/examples/ (verify `fullscreen_material`, atmosphere, Solari, and new UI widgets)
- Docs: https://docs.rs/bevy/latest/bevy/

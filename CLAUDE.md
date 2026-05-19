# CLAUDE.md

Guidance for Claude (and other coding agents) working in this repo.

**Always read docs before reading code.** For whatever you are changing,
read the relevant material under `docs/`, `README.md`, and any skills
this file tells you to use *before* opening `src/` or scripts — unless
the task is a one-line fix with no behavioral surface.

## Project

**Everly** — a 3D strategy-camera sandbox built with the
[Bevy Engine](https://bevy.org) v0.18 in Rust. Currently a starter
scene: tilted overhead camera, camera ambient fill, ground plane, and a
handful of randomly scattered colored boxes.

## Tech stack

- **Language:** Rust (edition 2024, stable toolchain).
- **Engine:** `bevy = "0.18.1"` with default features. Do not pin to an
  older version or hand-pick low-level sub-crate features — prefer
  Bevy's high-level feature bundles.
- **Other deps:** `rand = "0.8"` (uses `gen_range`, not the 0.9 API).

Always read `.claude/SKILLS/bevy-engineer/SKILL.md` before touching
Rust, `Cargo.toml`, or `*.wgsl` files. It is the source of truth for
Bevy 0.18 idioms (observers, atmospheres, post-processing, UI, cameras,
features, fast-compile setup).

For the tilemap text format and **map world units** (1 m cells, wall thickness,
wall height vs storey spacing), see `docs/tilemap.md` and `src/map/floor_level.rs`.
When authoring or refactoring `world_map.txt`, `world_map_floor1.txt`, or map
encoding, read `.claude/SKILLS/map-creator/SKILL.md` first.

When editing procedural chunk generation (`src/map/map_generator/`), room
outlines, or `fill_procedural_chunk`, read `.claude/SKILLS/map-generator/SKILL.md`
and `docs/map-generator.md` first. For inner corner pillars (`c*`,
`corner_pillars.rs`), also read `docs/corners.md`.

When editing actor runtime code (`src/actor/`) or actor/passability movement
integration, read `.claude/SKILLS/actor-engineer/SKILL.md` first.

When editing hypermap fields (dirt, actor deposits, field overlays), read
`.claude/SKILLS/field-interactions/SKILL.md` and `docs/field-interactions.md` first.

## Repository layout

```
everly/
├── Cargo.toml            # bevy + rand, dev profile tuned for Bevy
├── .cargo/config.toml    # safe defaults + commented lld blocks
├── README.md             # user-facing docs (run, controls, license)
├── CLAUDE.md             # this file
├── world_map.txt         # startup map input (2 chars per cell)
├── scripts/              # e.g. generate_world_map.py → regen world_map.txt
├── docs/
│   ├── README.md         # index of behavior docs
│   ├── tilemap.md        # encoding + world units (1 m cells, walls, storeys)
│   ├── hypermap.md       # chunks, multi-floor, visibility, water
│   ├── rendering-pipeline.md  # batched meshes, floor vs wall layers
│   └── map-editor.md     # in-game tile paint + chunk remesh
├── .claude/SKILLS/       # repo-local skills (read these first)
│   ├── bevy-engineer/
│   ├── map-creator/
│   ├── map-generator/
│   └── actor-engineer/
└── src/
    ├── main.rs                   # window setup + DefaultPlugins + GamePlugin
    ├── lib.rs                    # GamePlugin wires every subsystem
    ├── scene/                    # how the world is presented
    │   ├── camera.rs             #   StrategyCameraPlugin (RTS cam + post-fx stack)
    │   └── sun.rs                #   SunPlugin (directional light)
    ├── hud/                      # 2D UI overlay
    │   └── game_hud.rs           #   GameHudPlugin (bottom bar, floor selector, edit toggle)
    ├── menu/                     # pre-gameplay screens (run only in MainMenu state)
    │   └── main_menu.rs          #   MainMenuPlugin + GameState (MainMenu / InGame), level picker
    ├── map/                      # world data + rendering + pathfinding
    │   ├── hypermap.rs           #   chunked, concurrent tile store
    │   ├── floor_level.rs        #   ActiveFloorLevel + storey-height constants
    │   ├── world_map.rs          #   `.txt` parser, CellType, wall masks
    │   ├── level.rs              #   LevelPlugin + level save/load (`docs/level-persistence.md`)
    │   ├── hypermap_world.rs     #   HypermapWorldPlugin (chunk meshing + water)
    │   └── hypermap_pathfind.rs  #   A* over hypermap floors
    └── edit/                     # in-game editing tools
        ├── map_edit.rs           #   MapEditPlugin (HUD palette, paint, remesh queue)
        └── map_selection.rs      #   MapSelectionPlugin (click-to-select + highlight)
```

## Architecture conventions

- **One subsystem = one module = one `Plugin`.** Never bolt new
  startup logic onto an unrelated module. If a feature doesn't fit an
  existing plugin, add a new `src/<group>/<feature>.rs` (under
  `scene/`, `hud/`, `menu/`, `map/`, or `edit/` — or create a new
  top-level group folder) exposing `pub struct <Feature>Plugin`,
  declare it in the group's `mod.rs`, and register it inside
  `GamePlugin` in `src/lib.rs`.
- **Gate gameplay on `GameState::InGame`.** The menu (`MainMenu`) is
  the default state. Spawning camera/HUD/world entities and
  per-frame gameplay systems must use `OnEnter(GameState::InGame)` /
  `.run_if(in_state(GameState::InGame))` so the menu starts with no
  world entities and no half-initialized resources.
- **`main.rs` stays tiny.** It owns window/`DefaultPlugins`
  configuration and nothing else. All gameplay wiring goes through
  `GamePlugin`.
- **ECS first.** Data lives in `Component`s, behavior lives in
  systems. Avoid god structs. Markers (e.g. `Ground`,
  `ScatterBox`, `StrategyCamera`) are how systems find their entities.
- **Public surface area is minimal.** Plugins are public; internal
  systems and helpers stay `fn` (private). Constants like
  `ground::GROUND_SIZE` are `pub` only when another module needs them.
- **Names matter.** Every spawned entity gets a `Name::new(...)` so
  the world is readable in inspector tooling.
- **Determinism by default.** Anything random (e.g. `boxes.rs`) uses a
  seeded `StdRng`, never `thread_rng`, so scenes are reproducible.

## Bevy 0.18 specifics worth remembering

These are the exact pitfalls hit while bootstrapping the project. They
cost a recompile each — don't repeat them.

- **Events vs messages.** Use `MessageReader<T>` /
  `MessageWriter<T>`, not `EventReader` / `EventWriter`. The old names
  were renamed in 0.17/0.18; `Event` is now reserved for the observer
  system (`On<MyEvent>`).
- **Observer signature.** Use `On<MyEvent>` (not the older
  `Trigger<MyEvent>`).
- **`AmbientLight` is a Component, not a Resource.** Attach it to the
  camera entity. Use the `GlobalAmbientLight` resource only for a
  default fallback.
- **Bundles are gone.** Use the required-components style:
  `(Camera3d::default(), Transform::..., ...)`,
  `(Mesh3d(..), MeshMaterial3d(..), Transform::..)`. No
  `Camera3dBundle` / `PbrBundle`.
- **`WindowResolution::new` takes `u32, u32`**, not floats.
- **Time API:** `time.delta_secs()` (the old `delta_seconds()` is
  removed).
- **Entity component access:** for safe multi-component mutation
  outside queries, use the stable `entity.get_components_mut::<(&mut A, &mut B)>()`.
- **Cameras:** prefer Bevy's first-party `FreeCameraPlugin` /
  `PanCameraPlugin` for dev tooling. The custom `StrategyCamera` here
  exists because gameplay needs an RTS-style controller; don't replace
  it with a fly camera.
- **Post-processing:** prefer the `FullscreenMaterial` trait +
  `FullscreenMaterialPlugin` over hand-rolled render graph nodes.
- **Atmospheres:** use the `ScatteringMedium` asset; do not hardcode
  earth-only parameters.
- **UI:** prefer built-in widgets (`Popover`, `MenuPopup`,
  `ColorPlane`, `RadioButton` / `RadioGroup`, automatic directional
  navigation) before writing custom ones. Use `TextFont` +
  `FontFeatures` for OpenType features.

## Strategy camera (`src/scene/camera.rs`)

The camera entity also carries `AmbientLight` (no directional sun),
`ScreenSpaceAmbientOcclusion`, `Bloom`, `Hdr`, `Tonemapping`, and optional
`ScreenSpaceReflections` — keep post/view components here, not in `main.rs`.

The camera is parameterized by a `StrategyCamera` component holding
`focus`, `distance`, `yaw`, `pitch`, plus speed/clamp params. Three
systems implement the controller:

- `pan_camera` — reads WASD **and** arrow keys via
  `keys.any_pressed([..])`, normalizes the input, and accelerates
  `pan_velocity` toward a max speed along the ground-plane basis derived
  from `yaw`. With no input, velocity decays exponentially (`pan_drag`)
  so the camera coasts briefly (inertia). `focus` integrates
  `pan_velocity` each frame. Speed scales with
  `distance / PAN_REFERENCE_DISTANCE` so it feels constant at any zoom
  level.
- `zoom_camera` — drains `MessageReader<MouseWheel>`, normalizing
  `Line` and `Pixel` scroll units, and clamps `distance` between
  `min_distance` and `max_distance`.
- `sync_camera_transform` — uses `Changed<StrategyCamera>` so the
  `Transform` is only rebuilt when params actually change.

`focus.y` eases toward the active hypermap floor height each frame (exponential blend in `src/scene/camera.rs`, rate `map::floor_level::CAMERA_FLOOR_Y_SMOOTH_PER_S`), not an instant snap.

When extending the camera (e.g. rotation, edge-pan, follow target),
add new systems and component fields rather than special-casing inside
the existing systems.

## Commands

```sh
cargo check                  # fast type-check; run after every edit
cargo run                    # debug build
cargo run --release          # smoothest playback
cargo run --features dev     # iterative dev with bevy/dynamic_linking
```

There are no tests yet. If you add gameplay logic that warrants them,
prefer pure-function unit tests in the same module under
`#[cfg(test)] mod tests { ... }`. Avoid headless `App` tests unless
strictly necessary — they're slow.

## Workflow expectations

1. **Docs before code** (see the rule at the top of this file).
2. **Read the bevy-engineer skill first** when touching Rust, Cargo,
   or shaders.
3. **Run `cargo check`** after substantive edits and before declaring
   work done. Fix all warnings you introduce.
4. **Keep modules narrowly scoped.** Splitting a growing module is
   preferred over letting it become a kitchen sink.
5. **Don't add comments that narrate code.** Comments should explain
   non-obvious intent, trade-offs, or constraints — never restate what
   the line below already says.
6. **Don't introduce new top-level dependencies casually.** Prefer
   what Bevy already bundles. If a new crate is genuinely required,
   pick a maintained, mainstream one and pin a recent version.
7. **Verify Bevy API shapes against `https://docs.rs/bevy/0.18.1/`**
   before guessing. The 0.17 → 0.18 jump renamed enough things that
   training-data recall is unreliable.

# IMPORTANT

**Never use python for map generation, it always fails**

# Everly

A 3D strategy-camera sandbox built with the [Bevy Engine](https://bevy.org)
v0.18 in Rust.

The starter scene gives you:

- A tilted overhead **strategy camera** you drive with WASD or arrow keys
  (mouse wheel zooms in and out).
- **Screen-space ambient occlusion** on the main camera (contact shading, no
  directional sun).
- A large **ground plane** and **121** columns on a **strict integer grid**
  (heights **3–8** in whole units; random occupied cells in the ~73×73 field;
  some dark, some emissive) to fly over.

## Map authoring

On launch the player lands on a **main menu** that lists every folder
under `levels/level_*/`. Picking one sets the active level and drops
you into the world. Levels live under `levels/level_{name}/` (geometry per chunk, binary dirt/temperature,
actors, camera). Chunks missing on disk are generated procedurally in memory; use the
map editor **Save** button to persist (no autosave). The center chunk overlays
`world_map.txt` only when it has no saved geometry file.

For token format, **world scale** (1 m tiles, 0.2 m wall thickness, 3 m walls, storey spacing slightly above 3 m), and rendering semantics, see:

- `docs/tilemap.md`
- `docs/hypermap.md`
- `docs/rendering-pipeline.md`
- `docs/level-persistence.md` — save/load layout, load order, binary field format
- `docs/map-editor.md` — in-game hypermap paint mode (Edit button, palette, preview, remesh)
- `docs/actor.md` — actor low-level runtime, footprint/collision rules, and examples

## Project layout

The game is split into small focused submodules grouped by concern,
each (where applicable) exposing a single Bevy `Plugin`:

| Module                          | Responsibility                                                    |
| ------------------------------- | ----------------------------------------------------------------- |
| `src/main.rs`                   | App entry point and window setup.                                 |
| `src/lib.rs`                    | Top-level `GamePlugin` that wires every subsystem.                |
| `src/actor/mod.rs`              | `ActorPlugin` + trait-based low-level actor loop and movement.    |
| `src/menu/main_menu.rs`         | `MainMenuPlugin` + `GameState` (MainMenu / InGame), level picker. |
| `src/scene/camera.rs`           | `StrategyCameraPlugin` — RTS pan/zoom + post-processing stack.    |
| `src/scene/sun.rs`              | `SunPlugin` — directional sun light.                              |
| `src/hud/game_hud.rs`           | `GameHudPlugin` — bottom HUD (floor selector, edit toggle).       |
| `src/map/hypermap.rs`           | Generic chunked, concurrent tile store.                           |
| `src/map/floor_level.rs`        | `FloorLevelPlugin` — `ActiveFloorLevel` + storey-height constants.|
| `src/map/world_map.rs`          | `world_map.txt` parser, `CellType`, wall masks.                   |
| `src/map/level.rs`              | `LevelPlugin` — `LevelName` + `levels/level_{name}/geometry/` I/O.  |
| `src/map/hypermap_world.rs`     | `HypermapWorldPlugin` — chunk meshing + water tiles.              |
| `src/map/hypermap_pathfind.rs`  | A* over hypermap floors.                                          |
| `src/edit/map_edit.rs`          | `MapEditPlugin` — paint palette, preview, remesh queue.           |
| `src/edit/map_selection.rs`     | `MapSelectionPlugin` — click-to-select cell + highlight.          |

## Getting started

You will need a recent Rust toolchain (`rustup default stable` is fine).

```sh
# Debug build with stock settings
cargo run

# Release build (recommended for actually playing)
cargo run --release

# Iterative dev build with Bevy's dynamic linking enabled
cargo run --features dev
```

### Controls

| Action      | Keys                        |
| ----------- | --------------------------- |
| Pan camera  | `W` `A` `S` `D` or arrows   |
| Zoom        | Mouse wheel / trackpad scroll (disabled while placing **Wall** or **Corner** in map edit mode; wheel then cycles variants) |

## Faster iterative builds

Bevy 0.18 compiles fastest with a modern linker. The repo ships
`.cargo/config.toml` with safe defaults and commented-out, ready-to-use
blocks for `lld` (Linux/macOS/Windows). Pick the block that matches your
platform, install the prerequisites it lists, and uncomment it.

For the largest speed-up combine that with the `dev` feature flag, which
toggles `bevy/dynamic_linking` so most of the engine is built once and
reused on every incremental rebuild.

## License

This project is dual-licensed under either:

- MIT License (<http://opensource.org/licenses/MIT>)
- Apache License, Version 2.0 (<http://www.apache.org/licenses/LICENSE-2.0>)

at your option.

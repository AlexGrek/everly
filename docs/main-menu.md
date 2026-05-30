# Main menu

The first screen the player sees on launch. Implemented as
`MainMenuPlugin` in `src/menu/main_menu.rs`, it owns the app-wide
[`GameState`](#gamestate-flag) and is the only plugin allowed to
transition into gameplay.

## GameState flag

```rust
#[derive(States, Default, ...)]
pub enum GameState {
    #[default]
    MainMenu,
    InGame,
}
```

Every other plugin gates its setup and per-frame work on
`GameState::InGame`:

- Spawning entities (camera, sun, HUD, hypermap chunks, edit palette,
  preview material) → `OnEnter(GameState::InGame)`.
- Update systems that drive the world → `.run_if(in_state(GameState::InGame))`.

This keeps the menu free of any world entities, post-processing, or
half-initialized resources (e.g. `HypermapRuntime` only exists once the
player has chosen a level).

## Layout

- A dedicated `Camera2d` is spawned for the menu and tagged
  `MainMenuEntity`; the gameplay `StrategyCamera` does not exist yet.
- A centered column shows the title, a subtitle, and a panel listing
  every discovered level as a button.
- Background uses a dark flat color (`MENU_BG`) so the menu reads as a
  separate view, not a transparent overlay on the world.

`OnExit(GameState::MainMenu)` despawns every entity tagged
`MainMenuEntity` (the camera and the UI root, which transitively takes
its children with it).

## Level discovery

`scan_available_levels` runs on `OnEnter(GameState::MainMenu)` and walks
`levels/` looking for subdirectories whose names start with `level_`.
The text after the prefix becomes the level name displayed on a button.

- The folder is read from the **process working directory** (same as
  `levels/level_{name}/geometry/{x}_{y}.txt` in
  [`map-editor.md`](map-editor.md)). Run the game from the repo root
  for the bundled `level_default` to show up.
- The list is sorted, deduplicated, and falls back to a single
  `default` entry when `levels/` is missing or empty so the player can
  always start a fresh sandbox.
- The scan re-runs every time the player returns to `MainMenu`, so
  newly created level folders show up without restarting.

## Loading a level

`main_menu_load_buttons` listens for `Interaction::Pressed` on each
`LoadLevelButton(level_name)`. On a press it:

1. Writes the chosen name into the
   [`LevelName`](../src/map/level.rs) resource (default `default`).
2. Sets `NextState::Pending(GameState::InGame)`.

On the next frame, every plugin's `OnEnter(GameState::InGame)` setup
runs in order, including `setup_hypermap_runtime` →
`setup_hypermap_assets`. Actors and camera load from disk when their YAML
files exist; geometry and tile fields load lazily per chunk. See
[`level-persistence.md`](level-persistence.md) for the full timeline and file
layout.

As chunks come into view, `ensure_chunk_generated`
in `src/map/hypermap_world.rs` looks for
`levels/level_{name}/geometry/{x}_{y}.txt` first; missing chunks fall
back to procedural generation (with `world_map.txt` overlaying only the
center chunk when that chunk has no geometry file).

## Creating a new level

A green **+ New level** button sits beneath the level list. Pressing it:

1. Picks the next free name `new_NNN` (3-digit, zero-padded) by skipping
   any `levels/level_*/` folder already discovered (case-insensitive).
2. Calls `create_new_level_with_road_origin` in `src/map/level.rs`,
   which creates `levels/level_{name}/geometry/` and writes a single
   chunk file `0_0.txt` whose floor `0` is **completely [`CellType::Road`]**
   (every upper floor stays void, so they are omitted from the file).
3. Writes the new name into [`LevelName`](../src/map/level.rs) and sets
   `NextState::Pending(GameState::InGame)`.

Every other chunk is generated lazily by `ensure_chunk_generated` the
first time the camera sees it, exactly like a loaded level — only the
origin chunk is authored up front, and only as plain road, so the
player has a guaranteed empty starting plaza without houses or void.

`world_map.txt` / `world_map_floor1.txt` are **not** applied to a new
level's `0_0` chunk: the existence of the saved file makes the load path
authoritative, bypassing both the procedural fill and the center-chunk
overlay.

## Related code

| Piece | Role |
|-------|------|
| `GameState` (`src/menu/main_menu.rs`) | App-wide menu vs gameplay flag |
| `MainMenuPlugin` (`src/menu/main_menu.rs`) | Spawn / despawn UI, scan levels, handle button input |
| `AvailableLevels` (`src/menu/main_menu.rs`) | Cached list of `levels/level_*/` names |
| `LevelName` (`src/map/level.rs`) | Active level folder, set by the menu before transitioning |
| `ensure_chunk_generated` (`src/map/hypermap_world.rs`) | Loads `geometry/{x}_{y}.txt` per chunk on demand |

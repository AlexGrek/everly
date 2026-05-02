# CLAUDE.md

Guidance for Claude (and other coding agents) working in this repo.

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

## Repository layout

```
everly/
├── Cargo.toml            # bevy + rand, dev profile tuned for Bevy
├── .cargo/config.toml    # safe defaults + commented lld blocks
├── README.md             # user-facing docs (run, controls, license)
├── CLAUDE.md             # this file
├── .claude/SKILLS/       # repo-local skills (read these first)
└── src/
    ├── main.rs           # window setup + DefaultPlugins + GamePlugin
    ├── lib.rs            # GamePlugin wires every submodule
    ├── camera.rs         # StrategyCameraPlugin
    ├── ground.rs         # GroundPlugin (200×200 plane)
    └── boxes.rs          # BoxesPlugin (sparse grid columns + emissive mix)
```

## Architecture conventions

- **One subsystem = one module = one `Plugin`.** Never bolt new
  startup logic onto an unrelated module. If a feature doesn't fit an
  existing plugin, add a new `src/<feature>.rs` exposing
  `pub struct <Feature>Plugin` and register it inside `GamePlugin` in
  `src/lib.rs`.
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

## Strategy camera (`src/camera.rs`)

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

1. **Read the bevy-engineer skill first** when touching Rust, Cargo,
   or shaders.
2. **Run `cargo check`** after substantive edits and before declaring
   work done. Fix all warnings you introduce.
3. **Keep modules narrowly scoped.** Splitting a growing module is
   preferred over letting it become a kitchen sink.
4. **Don't add comments that narrate code.** Comments should explain
   non-obvious intent, trade-offs, or constraints — never restate what
   the line below already says.
5. **Don't introduce new top-level dependencies casually.** Prefer
   what Bevy already bundles. If a new crate is genuinely required,
   pick a maintained, mainstream one and pin a recent version.
6. **Verify Bevy API shapes against `https://docs.rs/bevy/0.18.1/`**
   before guessing. The 0.17 → 0.18 jump renamed enough things that
   training-data recall is unreliable.

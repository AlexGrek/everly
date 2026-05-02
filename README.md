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

## Project layout

The game is split into small focused submodules, each exposing a single
Bevy `Plugin`:

| Module          | Responsibility                                       |
| --------------- | ---------------------------------------------------- |
| `src/main.rs`   | App entry point and window setup.                    |
| `src/lib.rs`    | Top-level `GamePlugin` that wires everything up.     |
| `src/camera.rs` | `StrategyCameraPlugin` — pan + zoom controller.      |
| `src/ground.rs` | `GroundPlugin` — large flat ground plane.            |
| `src/boxes.rs`  | `BoxesPlugin` — 121 random grid cells, full span, heights 3–8. |

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
| Zoom        | Mouse wheel / trackpad scroll |

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

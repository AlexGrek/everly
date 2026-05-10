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
---

# Bevy engineer (v0.18)

## Context

This project targets **Bevy v0.18**. When generating, editing, or refactoring Rust for this repo, follow the v0.18 API, a data-driven ECS architecture, and current Bevy practices.

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

## Project workflow

- **ECS:** Keep data in `Component`s and logic in systems; avoid god objects.
- **Cargo features:** Prefer Bevy’s high-level feature bundles in `Cargo.toml` (for example 2D, 3D, UI) instead of hand-picking many granular sub-crate features.
- **Compile times:** If not already configured, suggest `lld` or `mold` (or `zld` on macOS) plus Cranelift in `.cargo/config.toml` for faster debug iteration.

## Official references

- Release notes: https://bevy.org/news/bevy-0-18/
- Examples: https://bevyengine.org/examples/ (verify `fullscreen_material`, atmosphere, Solari, and new UI widgets)
- Docs: https://docs.rs/bevy/latest/bevy/

---
name: gpu-bevy
description: >-
  Everything needed to write Bevy 0.18 shaders — WGSL fundamentals, fullscreen /
  post-processing materials, and especially GPU **compute shaders** (pipeline,
  bind-group layouts, storage buffers, render-graph nodes, ExtractResource, and
  GPU→CPU readback). Use when authoring or debugging `*.wgsl`, compute pipelines,
  render-graph nodes, storage/uniform buffers, or anything in the render world.
  Worked example in this repo: `src/map/temperature_diffusion.rs`.
paths:
  - "**/*.wgsl"
  - "src/map/temperature_diffusion.rs"
  - "docs/temperature-diffusion.md"
---

# GPU & shaders in Bevy 0.18

This project targets **Bevy v0.18**. The 0.17→0.18 jump renamed enough render APIs
that training-data recall is unreliable — **verify against `https://docs.rs/bevy/0.18.1/`**
and the version-pinned examples below before guessing. Read the relevant `docs/`
file and `bevy-engineer` skill first (docs-before-code rule).

> Worked, compiling reference in this repo: the GPU temperature diffusion system
> ([`src/map/temperature_diffusion.rs`](../../src/map/temperature_diffusion.rs) +
> [`assets/shaders/temperature_diffusion.wgsl`](../../assets/shaders/temperature_diffusion.wgsl),
> docs: [`docs/temperature-diffusion.md`](../../docs/temperature-diffusion.md)). It is a
> full compute-pipeline + storage-buffer + render-graph-node + readback loop — copy its
> shape for new compute work.

## Assets & the shader path (gotcha)

- Shaders are normal assets: `asset_server.load::<Shader>("shaders/foo.wgsl")`. They live
  under the project **`assets/`** dir (`DefaultPlugins`' `AssetPlugin` serves `assets/` by
  default; create the dir if missing — this repo's `assets/shaders/` was added for this).
- **Asset root depends on launch method.** Under `cargo run`, the root is the project dir
  (`CARGO_MANIFEST_DIR`). Running the **raw binary** (`./target/debug/everly`) resolves the
  root to the executable's dir → `target/debug/assets/...` → `Path not found`. Always
  validate shaders with **`cargo run`**, not the bare binary.
- A WGSL compile/validation error shows up as a `naga` / pipeline `CachedPipelineState::Err`
  in the log. If your render-graph node *silently* skips on a missing pipeline, a broken
  shader looks like "nothing happens" — grep the log for `naga`/`wgsl`/`pipeline`/`Path not
  found`, or temporarily `panic!` on `CachedPipelineState::Err` while debugging (the
  `compute_shader_game_of_life` example does this).
- There is no `timeout` on macOS — bound a manual run with `perl -e 'alarm 30; exec @ARGV' cargo run`.

## WGSL fundamentals (0.18)

- `@group(N) @binding(M) var<...> name: T;` — binding indices must match the bind-group
  layout order exactly (`BindGroupLayoutEntries::sequential` assigns 0,1,2… in tuple order).
- Address spaces: `var<uniform>` (uniform buffer), `var<storage, read>` /
  `var<storage, read_write>` (storage buffer), `var<workgroup>`, plain `var` (function-local).
- Compute entry: `@compute @workgroup_size(X, Y, Z) fn main(@builtin(global_invocation_id) gid: vec3<u32>)`.
  Always bounds-check `gid` against the real extent (dispatch is rounded **up** to whole
  workgroups, so excess invocations run).
- Import Bevy's shared shader defs with `#import bevy_pbr::...` / `#import bevy_render::...`
  (the `naga_oil` preprocessor). For standalone compute you usually need none.
- `entry_point` in `ComputePipelineDescriptor` selects the `fn` (e.g. `Some("init".into())`);
  default is `"main"`.

## Fullscreen / post-processing (prefer high-level)

Per the `bevy-engineer` skill: for fullscreen effects use the **`FullscreenMaterial` trait +
`FullscreenMaterialPlugin`** rather than hand-rolled render-graph nodes. Implement
`fragment_shader()` and `node_edges()` (e.g. between `Node3d::Tonemapping` and
`Node3d::EndMainPassPostProcessing`). For surface materials, implement `Material` /
`MaterialExtension` and supply a `fragment_shader()`/`vertex_shader()` `ShaderRef`.
Atmospheres use the `ScatteringMedium` asset (no hardcoded earth params).

## Compute shaders — the full pattern (verified, 0.18)

A compute feature is **one `Plugin`** that wires both worlds. Canonical imports:

```rust
use bevy::render::{
    extract_resource::{ExtractResource, ExtractResourcePlugin},
    graph::CameraDriverLabel,
    render_asset::RenderAssets,
    render_graph::{self, RenderGraph, RenderLabel},
    render_resource::{
        binding_types::{storage_buffer, storage_buffer_read_only, uniform_buffer, texture_storage_2d},
        *, // BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
           // BufferUsages, CachedComputePipelineId, ComputePassDescriptor,
           // ComputePipelineDescriptor, PipelineCache, ShaderStages, ShaderType, UniformBuffer, ...
    },
    renderer::{RenderContext, RenderDevice, RenderQueue},
    storage::{GpuShaderStorageBuffer, ShaderStorageBuffer},
    texture::GpuImage,
    Render, RenderApp, RenderStartup, RenderSystems,
};
```

### 1. Plugin wiring (main world ↔ render world)

```rust
impl Plugin for MyComputePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ExtractResourcePlugin::<MyGpu>::default());   // main → render each frame
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else { return; };
        render_app
            .add_systems(RenderStartup, init_pipeline)                 // build pipeline+layout once
            .add_systems(Render, prepare_bind_group.in_set(RenderSystems::PrepareBindGroups));
        let mut graph = render_app.world_mut().resource_mut::<RenderGraph>();
        graph.add_node(MyNodeLabel, MyNode);                          // top-level = runs once/frame
        graph.add_node_edge(MyNodeLabel, CameraDriverLabel);          // order before the camera driver
    }
}
```

- **`ExtractResourcePlugin::<T>`** copies a `#[derive(Resource, Clone, ExtractResource)]`
  resource from the main world into the render world every frame (handles, dims, params).
  Render-world systems read *that* copy, never the main-world one.
- Render schedules: **`RenderStartup`** (one-time setup, has `RenderDevice`/`PipelineCache`),
  **`Render`** with `RenderSystems::{PrepareAssets, PrepareResources, PrepareBindGroups, …}` sets.
  Build/refresh bind groups in `PrepareBindGroups` (runs after `RenderAssets` are ready).

### 2. Pipeline + bind-group layout

```rust
let layout = BindGroupLayoutDescriptor::new("MyLayout", &BindGroupLayoutEntries::sequential(
    ShaderStages::COMPUTE,
    (
        storage_buffer::<Vec<f32>>(false),            // binding 0: read_write storage
        storage_buffer::<Vec<f32>>(false),            // binding 1: read_write storage
        storage_buffer_read_only::<Vec<f32>>(false),  // binding 2: read storage
        uniform_buffer::<MyParams>(false),            // binding 3: uniform   (bool = has_dynamic_offset)
    ),
));
let shader = asset_server.load("shaders/my.wgsl");
let pipeline = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
    label: Some("my compute".into()),
    layout: vec![layout.clone()],
    shader,
    ..default() // entry_point defaults to "main"
});
// store BOTH: keep the descriptor (`layout`) AND the CachedComputePipelineId (`pipeline`).
```

- The layout stored is a **descriptor**; resolve the concrete layout at bind-group build time
  with `pipeline_cache.get_bind_group_layout(&descriptor)`.
- `binding_types`: `storage_buffer`, `storage_buffer_read_only`, `uniform_buffer`,
  `texture_storage_2d(format, StorageTextureAccess::{ReadOnly|WriteOnly|ReadWrite})`,
  `texture_2d`, `sampler`. The generic on `storage_buffer::<T>` only sizes the binding; a
  runtime `array<f32>` in WGSL is fine.

### 3. Storage buffers vs. textures

- **`ShaderStorageBuffer`** (asset): `ShaderStorageBuffer::from(vec![0f32; N])`; set usages with
  `buf.buffer_description.usage |= BufferUsages::COPY_SRC | BufferUsages::COPY_DST;`. In the
  render world get the GPU side via `Res<RenderAssets<GpuShaderStorageBuffer>>` → `.get(&handle)`
  → `gpu.buffer.as_entire_buffer_binding()` for a bind-group entry.
- **Storage textures** (`Image`): `Image::new_uninit(..., TextureFormat::R32Float, RenderAssetUsages::RENDER_WORLD)`
  then `image.texture_descriptor.usage |= TextureUsages::COPY_SRC | STORAGE_BINDING | TEXTURE_BINDING;`
  GPU side via `Res<RenderAssets<GpuImage>>` → `gpu.texture_view`. Use textures when you also
  sample/display the result; buffers when you index a flat grid and read it back.
- **Uniforms:** build per-frame in the prepare system: `let mut ub = UniformBuffer::from(params);
  ub.write_buffer(&render_device, &render_queue);` then pass `&ub` (or `ub.binding().unwrap()`)
  to `BindGroupEntries::sequential`.
- **Two ways to upload changing data:** (a) mutate the `ShaderStorageBuffer` asset's `data` bytes
  via `Assets::get_mut` (marks it changed → Bevy re-uploads; may reallocate the GPU buffer, so
  rebuild the bind group each frame); (b) keep a persistent buffer and
  `RenderQueue::write_buffer(&gpu.buffer, 0, bytes)` in a render-world prepare system (no realloc,
  bind group stays valid). Use `bytemuck::cast_slice(&[f32])` for the bytes.

### 4. Render-graph node (the dispatch)

```rust
#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)] struct MyNodeLabel;
struct MyNode;
impl render_graph::Node for MyNode {
    // optional fn update(&mut self, world: &mut World) — advance a state machine / check pipeline state
    fn run(&self, _g: &mut render_graph::RenderGraphContext, ctx: &mut RenderContext, world: &World)
        -> Result<(), render_graph::NodeRunError> {
        let pcache = world.resource::<PipelineCache>();
        let Some(pipeline) = pcache.get_compute_pipeline(world.resource::<MyPipeline>().pipeline) else {
            return Ok(()); // not compiled yet (or shader error) — skip silently
        };
        let bind_group = &world.resource::<MyBindGroup>().0;
        {
            let mut pass = ctx.command_encoder().begin_compute_pass(&ComputePassDescriptor::default());
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
        } // drop the pass before issuing copies on the encoder
        // ctx.command_encoder().copy_buffer_to_buffer(&src.buffer, 0, &dst.buffer, 0, bytes);
        Ok(())
    }
}
```

- A **top-level** `graph.add_node` runs once per frame; `add_node_edge(label, CameraDriverLabel)`
  orders it before the main camera driver. (`RenderGraphApp::add_render_graph_node` exists but
  can't add top-level nodes — use `RenderGraph::add_node` directly.)
- Guard everything with `world.get_resource::<…>()` — render-world resources may be absent before
  the first extract / when a feature is inactive.
- **Ping-pong & barriers:** WebGPU does **not** guarantee storage writes from one dispatch are
  visible to the next *within the same compute pass*. For iterative sims, either run **one
  compute pass per step** (begin/drop a pass each iteration — simplest, safe) or use separate
  passes; alternate **two bind groups** that swap read/write buffers (`[A→B], [B→A]`). Track parity
  so the final result lands in a known buffer (even step count ⇒ back in A).

### 5. GPU → CPU readback

```rust
use bevy::render::gpu_readback::{Readback, ReadbackComplete}; // GpuReadbackPlugin is in DefaultPlugins
commands.spawn(Readback::buffer(handle.clone()))   // or Readback::texture(image), Readback::buffer_range(..)
    .observe(|ev: On<ReadbackComplete>, /* + any main-world system params */| {
        let data: Vec<f32> = ev.to_shader_type();   // interpret bytes as a ShaderType
        // write back into your CPU source of truth
    });
```

- The buffer/texture must have `COPY_SRC` usage. The observer runs in the **main world** and
  fires **every frame** while the entity exists (despawn to stop). It is **asynchronous** (≥1
  frame latency).
- **Gate the apply** when your GPU input changes per frame: only apply once the corresponding
  upload's parameters (window origin/size, generation id) are known to match — e.g. require the
  layout to be *stable for N frames*, or carry a generation tag and ignore stale results.
  Otherwise you apply data that no longer lines up with the current CPU state.
- Shutdown logs `Failed to send readback result: sending into a closed channel` — harmless.

## Common pitfalls (most cost a recompile each)

- WGSL not under `assets/`, or testing via the raw binary → `Path not found`.
- Binding index / type mismatch between WGSL `@binding(n)` and the layout tuple order → pipeline
  validation error in `naga`.
- Missing `COPY_SRC`/`COPY_DST`/`STORAGE_BINDING` usage flags → bind-group or copy failures.
- Issuing `copy_*` on the encoder while a compute/render pass is still alive (must drop the pass).
- Assuming intra-pass dispatch ordering syncs storage memory (it doesn't — use separate passes).
- Mutating an asset every frame *and* caching the bind group with
  `run_if(not(resource_exists::<BindGroup>))` → stale bind group after the GPU buffer reallocs.
  Either rebuild every frame, or upload via `RenderQueue::write_buffer` into a persistent buffer.
- Forgetting to bounds-check `global_invocation_id` against the real extent.
- Per-frame GPU readback is a real cost/latency hit — it was a deliberate design choice for the
  temperature field (CPU stays source of truth); don't add it casually on a hot path.

## Reference links (version-pinned)

**Official examples (pin to `release-0.18.0`):**
- Compute + readback: `https://github.com/bevyengine/bevy/blob/release-0.18.0/examples/shader/gpu_readback.rs`
- Compute ping-pong (Game of Life): `https://github.com/bevyengine/bevy/blob/release-0.18.0/examples/shader/compute_shader_game_of_life.rs`
- All shader examples: `https://github.com/bevyengine/bevy/tree/release-0.18.0/examples/shader`
- Live example gallery: `https://bevyengine.org/examples/` (Shaders section)

**Docs (`docs.rs/bevy/0.18.1`):**
- GPU readback: `https://docs.rs/bevy/0.18.1/bevy/render/gpu_readback/index.html`
- Render resources (pipelines, buffers, bind groups): `https://docs.rs/bevy/0.18.1/bevy/render/render_resource/index.html`
- `PipelineCache`: `https://docs.rs/bevy/0.18.1/bevy/render/render_resource/struct.PipelineCache.html`
- `ComputePipelineDescriptor`: `https://docs.rs/bevy/0.18.1/bevy/render/render_resource/struct.ComputePipelineDescriptor.html`
- Render graph: `https://docs.rs/bevy/0.18.1/bevy/render/render_graph/index.html`
- Storage buffers: `https://docs.rs/bevy/0.18.1/bevy/render/storage/struct.ShaderStorageBuffer.html`
- `ExtractResource`: `https://docs.rs/bevy/0.18.1/bevy/render/extract_resource/index.html`

**Background reading:**
- Bevy 0.18 release notes: `https://bevy.org/news/bevy-0-18/`
- WGSL spec: `https://www.w3.org/TR/WGSL/`
- WebGPU spec (memory/sync model): `https://www.w3.org/TR/webgpu/`
- `naga_oil` preprocessor (`#import`): `https://github.com/bevyengine/naga_oil`

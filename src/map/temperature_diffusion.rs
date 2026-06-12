//! GPU-compute temperature **spread** over the ground-floor temperature field.
//!
//! The CPU [`TemperatureMap`](crate::map::temperature::TemperatureMap) stays the source
//! of truth. Each frame, while in-game, we pack the bounding box of the currently visible
//! chunks into one contiguous tile **window**, upload it to the GPU, run a few explicit
//! diffusion substeps (`assets/shaders/temperature_diffusion.wgsl`), and **read the result
//! back** into the CPU field. Because all visible chunks share one packed grid, heat flows
//! seamlessly across 128×128 chunk borders. Walls/void (passability `0`) insulate, and the
//! field relaxes slowly toward ambient.
//!
//! The GPU is treated as a **stateless per-tick step function**: it never retains state
//! between frames, so camera panning / chunk load-unload needs no preservation logic — we
//! simply re-pack from the (authoritative) CPU field every frame. Readback is asynchronous
//! (≥1 frame), so results are only applied to the CPU once the window has been *stable* for
//! a few frames (`SETTLE_FRAMES`), guaranteeing the in-flight result still matches the
//! current window origin/dimensions.
//!
//! See `docs/temperature-diffusion.md`.

use bevy::prelude::*;
use bevy::render::{
    extract_resource::{ExtractResource, ExtractResourcePlugin},
    graph::CameraDriverLabel,
    render_asset::RenderAssets,
    render_graph::{self, RenderGraph, RenderLabel},
    render_resource::{
        binding_types::{storage_buffer, storage_buffer_read_only, uniform_buffer},
        BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
        BufferUsages, CachedComputePipelineId, ComputePassDescriptor, ComputePipelineDescriptor,
        PipelineCache, ShaderStages, ShaderType, UniformBuffer,
    },
    renderer::{RenderContext, RenderDevice, RenderQueue},
    storage::{GpuShaderStorageBuffer, ShaderStorageBuffer},
    Render, RenderApp, RenderStartup, RenderSystems,
};

use crate::hud::perf_timings::{SystemTimings, TimedSystem};
use crate::map::hypermap::{ChunkCoord, LocalCoord, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::temperature::{TemperatureMap, TEMP_MAX_C, TEMP_MIN_C, TEMP_ZERO_C};
use crate::menu::main_menu::GameState;

const SHADER_ASSET_PATH: &str = "shaders/temperature_diffusion.wgsl";
const WORKGROUP_SIZE: u32 = 8;

/// Max simulated window edge, in chunks. The visible set is only ~3 chunks (a 2×2 bbox);
/// 3×3 leaves headroom. Larger bounding boxes are clipped to this around their centre.
const MAX_WINDOW_CHUNKS: i32 = 3;
const MAX_WINDOW_DIM: usize = (MAX_WINDOW_CHUNKS * HYPERMAP_CHUNK_SIZE) as usize;
const WINDOW_CAPACITY: usize = MAX_WINDOW_DIM * MAX_WINDOW_DIM;

/// Diffusion substeps per dispatch (ping-pong). **Must be even** so the final result lands
/// back in buffer A (the readback copy source). Decouples spread speed from frame cadence.
const SUBSTEPS: u32 = 8;
/// Explicit-diffusion rate per substep (≤ 0.25 for 4-neighbour stability).
const ALPHA: f32 = 0.18;
/// Ambient relaxation per substep.
const BETA: f32 = 0.0025;
const AMBIENT_C: f32 = TEMP_ZERO_C;

/// Frames a window must remain unchanged before readback is trusted (covers readback latency).
const SETTLE_FRAMES: u32 = 4;

/// Shader uniform mirroring `Params` in the WGSL.
#[derive(Clone, Copy, ShaderType)]
struct DiffusionParams {
    width: u32,
    height: u32,
    alpha: f32,
    beta: f32,
    ambient: f32,
    temp_min: f32,
    temp_max: f32,
}

/// Render-world view of the GPU resources + current window (extracted from the main world).
#[derive(Resource, Clone, ExtractResource)]
struct DiffusionGpu {
    temp_a: Handle<ShaderStorageBuffer>,
    temp_b: Handle<ShaderStorageBuffer>,
    mask: Handle<ShaderStorageBuffer>,
    out: Handle<ShaderStorageBuffer>,
    width: u32,
    height: u32,
    params: DiffusionParams,
    /// `false` when there is nothing to simulate (no visible chunks, or not in-game).
    active: bool,
}

/// Main-world-only window bookkeeping consumed by the readback observer.
#[derive(Resource, Default)]
struct DiffusionWindow {
    origin_x: i32,
    origin_y: i32,
    width: usize,
    height: usize,
    active: bool,
    /// Consecutive frames the window has been identical (origin + size).
    stable_frames: u32,
}

/// Reused pack scratch (allocation-free steady state).
#[derive(Resource)]
struct DiffusionScratch {
    temps: Vec<f32>,
    mask: Vec<f32>,
}

/// Marks the entity carrying the [`Readback`](bevy::render::gpu_readback::Readback) request.
#[derive(Component)]
struct DiffusionReadback;

pub struct TemperatureDiffusionPlugin;

impl Plugin for TemperatureDiffusionPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ExtractResourcePlugin::<DiffusionGpu>::default())
            .add_systems(OnEnter(GameState::InGame), setup_diffusion)
            .add_systems(OnExit(GameState::InGame), teardown_diffusion)
            .add_systems(
                Update,
                diffusion_tick.run_if(in_state(GameState::InGame)),
            );

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .add_systems(RenderStartup, init_diffusion_pipeline)
            .add_systems(
                Render,
                prepare_diffusion_bind_groups.in_set(RenderSystems::PrepareBindGroups),
            );
        let mut render_graph = render_app.world_mut().resource_mut::<RenderGraph>();
        render_graph.add_node(DiffusionNodeLabel, DiffusionNode);
        render_graph.add_node_edge(DiffusionNodeLabel, CameraDriverLabel);
    }
}

// ───────────────────────────── main world ─────────────────────────────

fn new_window_buffer(usage: BufferUsages) -> ShaderStorageBuffer {
    let mut buffer = ShaderStorageBuffer::from(vec![AMBIENT_C; WINDOW_CAPACITY]);
    buffer.buffer_description.usage |= usage;
    buffer
}

fn setup_diffusion(
    mut commands: Commands,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
    existing: Query<Entity, With<DiffusionReadback>>,
) {
    for entity in existing.iter() {
        commands.entity(entity).despawn();
    }

    // A is re-uploaded each frame and also receives the final substep, so it is both a
    // storage target and the copy source for readback.
    let temp_a = buffers.add(new_window_buffer(BufferUsages::COPY_SRC));
    let temp_b = buffers.add(new_window_buffer(BufferUsages::empty()));
    let mask = buffers.add(new_window_buffer(BufferUsages::empty()));
    // `out` is the readback target: written by copy (COPY_DST), read to the CPU (COPY_SRC).
    let out = buffers.add(new_window_buffer(BufferUsages::COPY_DST | BufferUsages::COPY_SRC));

    commands.spawn((
        Name::new("Temperature diffusion readback"),
        DiffusionReadback,
        bevy::render::gpu_readback::Readback::buffer(out.clone()),
    ))
    .observe(apply_diffusion_readback);

    commands.insert_resource(DiffusionGpu {
        temp_a,
        temp_b,
        mask,
        out,
        width: 0,
        height: 0,
        params: DiffusionParams {
            width: 0,
            height: 0,
            alpha: ALPHA,
            beta: BETA,
            ambient: AMBIENT_C,
            temp_min: TEMP_MIN_C,
            temp_max: TEMP_MAX_C,
        },
        active: false,
    });
    commands.insert_resource(DiffusionWindow::default());
    commands.insert_resource(DiffusionScratch {
        temps: vec![AMBIENT_C; WINDOW_CAPACITY],
        mask: vec![0.0; WINDOW_CAPACITY],
    });
}

fn teardown_diffusion(
    mut commands: Commands,
    existing: Query<Entity, With<DiffusionReadback>>,
    gpu: Option<ResMut<DiffusionGpu>>,
    window: Option<ResMut<DiffusionWindow>>,
) {
    for entity in existing.iter() {
        commands.entity(entity).despawn();
    }
    if let Some(mut gpu) = gpu {
        gpu.active = false;
    }
    if let Some(mut window) = window {
        window.active = false;
        window.stable_frames = 0;
    }
}

/// Chunk-snapped bounding box of the visible chunks, clipped to [`MAX_WINDOW_CHUNKS`].
/// Returns `(origin_chunk, chunks_w, chunks_h)` or `None` when nothing is visible.
fn window_chunk_bounds(coords: &[ChunkCoord]) -> Option<(ChunkCoord, i32, i32)> {
    let first = coords.first()?;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (first.x, first.y, first.x, first.y);
    for c in coords {
        min_x = min_x.min(c.x);
        min_y = min_y.min(c.y);
        max_x = max_x.max(c.x);
        max_y = max_y.max(c.y);
    }
    // Clip span to the capacity, keeping the bbox centre.
    let clip = |min: &mut i32, max: &mut i32| {
        let span = *max - *min + 1;
        if span > MAX_WINDOW_CHUNKS {
            let centre = (*min + *max) / 2;
            *min = centre - (MAX_WINDOW_CHUNKS - 1) / 2;
            *max = *min + MAX_WINDOW_CHUNKS - 1;
        }
    };
    clip(&mut min_x, &mut max_x);
    clip(&mut min_y, &mut max_y);
    Some((
        ChunkCoord::new(min_x, min_y),
        max_x - min_x + 1,
        max_y - min_y + 1,
    ))
}

fn diffusion_tick(
    runtime: Res<HypermapRuntime>,
    temperature: Res<TemperatureMap>,
    mut gpu: ResMut<DiffusionGpu>,
    mut window: ResMut<DiffusionWindow>,
    mut scratch: ResMut<DiffusionScratch>,
    mut buffers: ResMut<Assets<ShaderStorageBuffer>>,
 timings: Res<SystemTimings>) {
    let _t = timings.scope(TimedSystem::TempDiffusion);
    let coords = runtime.desired_chunk_coords();
    let Some((origin_chunk, chunks_w, chunks_h)) = window_chunk_bounds(&coords) else {
        gpu.active = false;
        window.active = false;
        window.stable_frames = 0;
        return;
    };

    let origin_x = origin_chunk.x * HYPERMAP_CHUNK_SIZE;
    let origin_y = origin_chunk.y * HYPERMAP_CHUNK_SIZE;
    let width = (chunks_w * HYPERMAP_CHUNK_SIZE) as usize;
    let height = (chunks_h * HYPERMAP_CHUNK_SIZE) as usize;
    let len = width * height;
    debug_assert!(len <= WINDOW_CAPACITY);

    pack_window(
        &runtime,
        &temperature,
        origin_chunk,
        chunks_w,
        chunks_h,
        width,
        &mut scratch,
    );

    upload_buffer(&mut buffers, &gpu.temp_a, &scratch.temps[..len]);
    upload_buffer(&mut buffers, &gpu.mask, &scratch.mask[..len]);

    // Track window stability so the async readback only lands while origin/size are fixed.
    let same_window = window.active
        && window.origin_x == origin_x
        && window.origin_y == origin_y
        && window.width == width
        && window.height == height;
    window.stable_frames = if same_window { window.stable_frames + 1 } else { 0 };
    window.origin_x = origin_x;
    window.origin_y = origin_y;
    window.width = width;
    window.height = height;
    window.active = true;

    gpu.width = width as u32;
    gpu.height = height as u32;
    gpu.params.width = width as u32;
    gpu.params.height = height as u32;
    gpu.active = true;
}

/// Fills `scratch.temps` / `scratch.mask` from the CPU field + static passability, one chunk
/// at a time (chunk handle resolved once per chunk, not per tile). Tiles in unseeded chunks
/// read ambient and are masked off (insulated).
fn pack_window(
    runtime: &HypermapRuntime,
    temperature: &TemperatureMap,
    origin_chunk: ChunkCoord,
    chunks_w: i32,
    chunks_h: i32,
    width: usize,
    scratch: &mut DiffusionScratch,
) {
    let temp_read = temperature.read_map();
    let passability = &runtime.static_passability_map;

    for ccy in 0..chunks_h {
        for ccx in 0..chunks_w {
            let coord = ChunkCoord::new(origin_chunk.x + ccx, origin_chunk.y + ccy);
            let base_x = (ccx * HYPERMAP_CHUNK_SIZE) as usize;
            let base_y = (ccy * HYPERMAP_CHUNK_SIZE) as usize;

            let filled = temp_read.with_chunk_read(coord, |temp_chunk| {
                passability.with_chunk_read(coord, |pass_chunk| {
                    for ly in 0..HYPERMAP_CHUNK_SIZE {
                        for lx in 0..HYPERMAP_CHUNK_SIZE {
                            let local = LocalCoord::new(lx, ly);
                            let win_idx =
                                (base_y + ly as usize) * width + (base_x + lx as usize);
                            scratch.temps[win_idx] = *temp_chunk.get_local(local);
                            scratch.mask[win_idx] =
                                if *pass_chunk.get_local(local) > 0.0 { 1.0 } else { 0.0 };
                        }
                    }
                });
            });

            // Unseeded chunk: ambient + insulated so it neither shows nor drains heat.
            if filled.is_none() {
                for ly in 0..HYPERMAP_CHUNK_SIZE as usize {
                    let row = (base_y + ly) * width + base_x;
                    for col in 0..HYPERMAP_CHUNK_SIZE as usize {
                        scratch.temps[row + col] = AMBIENT_C;
                        scratch.mask[row + col] = 0.0;
                    }
                }
            }
        }
    }
}

/// Overwrites the active prefix of a storage-buffer asset in place (no realloc). Marking the
/// asset changed re-uploads it to the GPU before the compute pass reads it.
fn upload_buffer(
    buffers: &mut Assets<ShaderStorageBuffer>,
    handle: &Handle<ShaderStorageBuffer>,
    values: &[f32],
) {
    let Some(buffer) = buffers.get_mut(handle) else {
        return;
    };
    let Some(data) = buffer.data.as_mut() else {
        return;
    };
    let bytes = values.len() * std::mem::size_of::<f32>();
    if bytes <= data.len() {
        data[..bytes].copy_from_slice(bytemuck::cast_slice(values));
    }
}

/// Readback observer (main world). Applies the diffused window back to the CPU field once the
/// window has been stable long enough for the in-flight result to match the current origin.
fn apply_diffusion_readback(
    event: On<bevy::render::gpu_readback::ReadbackComplete>,
    window: Res<DiffusionWindow>,
    temperature: Res<TemperatureMap>,
) {
    if !window.active || window.stable_frames < SETTLE_FRAMES {
        return;
    }
    let len = window.width * window.height;
    if len == 0 {
        return;
    }
    let data: Vec<f32> = event.to_shader_type();
    if data.len() < len {
        return;
    }
    temperature.apply_window_readback(
        window.origin_x,
        window.origin_y,
        window.width,
        window.height,
        &data[..len],
    );
}

// ───────────────────────────── render world ─────────────────────────────

#[derive(Resource)]
struct DiffusionPipeline {
    layout: BindGroupLayoutDescriptor,
    pipeline: CachedComputePipelineId,
}

fn init_diffusion_pipeline(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    pipeline_cache: Res<PipelineCache>,
) {
    let layout = BindGroupLayoutDescriptor::new(
        "TemperatureDiffusion",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::COMPUTE,
            (
                storage_buffer::<Vec<f32>>(false),
                storage_buffer::<Vec<f32>>(false),
                storage_buffer_read_only::<Vec<f32>>(false),
                uniform_buffer::<DiffusionParams>(false),
            ),
        ),
    );
    let shader = asset_server.load(SHADER_ASSET_PATH);
    let pipeline = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some("temperature diffusion".into()),
        layout: vec![layout.clone()],
        shader,
        ..default()
    });
    commands.insert_resource(DiffusionPipeline { layout, pipeline });
}

/// Two ping-pong bind groups: `[0]` reads A→writes B, `[1]` reads B→writes A.
#[derive(Resource)]
struct DiffusionBindGroups {
    groups: [BindGroup; 2],
}

fn prepare_diffusion_bind_groups(
    mut commands: Commands,
    pipeline: Res<DiffusionPipeline>,
    gpu: Option<Res<DiffusionGpu>>,
    ssbos: Res<RenderAssets<GpuShaderStorageBuffer>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    pipeline_cache: Res<PipelineCache>,
) {
    let Some(gpu) = gpu else {
        commands.remove_resource::<DiffusionBindGroups>();
        return;
    };
    if !gpu.active {
        commands.remove_resource::<DiffusionBindGroups>();
        return;
    }
    let (Some(a), Some(b), Some(mask)) = (
        ssbos.get(&gpu.temp_a),
        ssbos.get(&gpu.temp_b),
        ssbos.get(&gpu.mask),
    ) else {
        return;
    };

    let mut params = UniformBuffer::from(gpu.params);
    params.write_buffer(&render_device, &render_queue);
    let Some(params_binding) = params.binding() else {
        return;
    };

    let layout = pipeline_cache.get_bind_group_layout(&pipeline.layout);
    let group_ab = render_device.create_bind_group(
        None,
        &layout,
        &BindGroupEntries::sequential((
            a.buffer.as_entire_buffer_binding(),
            b.buffer.as_entire_buffer_binding(),
            mask.buffer.as_entire_buffer_binding(),
            params_binding.clone(),
        )),
    );
    let group_ba = render_device.create_bind_group(
        None,
        &layout,
        &BindGroupEntries::sequential((
            b.buffer.as_entire_buffer_binding(),
            a.buffer.as_entire_buffer_binding(),
            mask.buffer.as_entire_buffer_binding(),
            params_binding,
        )),
    );
    commands.insert_resource(DiffusionBindGroups {
        groups: [group_ab, group_ba],
    });
}

#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
struct DiffusionNodeLabel;

struct DiffusionNode;

impl render_graph::Node for DiffusionNode {
    fn run(
        &self,
        _graph: &mut render_graph::RenderGraphContext,
        render_context: &mut RenderContext,
        world: &World,
    ) -> Result<(), render_graph::NodeRunError> {
        let (Some(gpu), Some(bind_groups), Some(pipeline)) = (
            world.get_resource::<DiffusionGpu>(),
            world.get_resource::<DiffusionBindGroups>(),
            world.get_resource::<DiffusionPipeline>(),
        ) else {
            return Ok(());
        };
        if !gpu.active || gpu.width == 0 || gpu.height == 0 {
            return Ok(());
        }
        let pipeline_cache = world.resource::<PipelineCache>();
        let Some(compute_pipeline) = pipeline_cache.get_compute_pipeline(pipeline.pipeline) else {
            return Ok(());
        };

        let groups_x = gpu.width.div_ceil(WORKGROUP_SIZE);
        let groups_y = gpu.height.div_ceil(WORKGROUP_SIZE);

        // One pass per substep so storage writes are visible to the next substep.
        for step in 0..SUBSTEPS {
            let mut pass = render_context
                .command_encoder()
                .begin_compute_pass(&ComputePassDescriptor {
                    label: Some("temperature diffusion substep"),
                    ..default()
                });
            pass.set_pipeline(compute_pipeline);
            pass.set_bind_group(0, &bind_groups.groups[(step % 2) as usize], &[]);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
        }

        // SUBSTEPS is even → the final result is in buffer A. Copy it to the readback target.
        let ssbos = world.resource::<RenderAssets<GpuShaderStorageBuffer>>();
        if let (Some(a), Some(out)) = (ssbos.get(&gpu.temp_a), ssbos.get(&gpu.out)) {
            let bytes = (gpu.width as u64) * (gpu.height as u64) * std::mem::size_of::<f32>() as u64;
            render_context
                .command_encoder()
                .copy_buffer_to_buffer(&a.buffer, 0, &out.buffer, 0, bytes);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CPU mirror of the WGSL kernel — single source of truth for the math under test.
    fn diffusion_step_cpu(
        temps: &[f32],
        mask: &[f32],
        width: usize,
        height: usize,
        params: &DiffusionParams,
    ) -> Vec<f32> {
        let mut out = vec![0.0f32; temps.len()];
        for y in 0..height {
            for x in 0..width {
                let idx = y * width + x;
                let c = temps[idx];
                if mask[idx] <= 0.0 {
                    out[idx] = c;
                    continue;
                }
                let mut sum = 0.0;
                let mut accumulate = |n: usize| {
                    if mask[n] > 0.0 {
                        sum += temps[n] - c;
                    }
                };
                if x > 0 {
                    accumulate(idx - 1);
                }
                if x + 1 < width {
                    accumulate(idx + 1);
                }
                if y > 0 {
                    accumulate(idx - width);
                }
                if y + 1 < height {
                    accumulate(idx + width);
                }
                let mut v = c + params.alpha * sum;
                v += params.beta * (params.ambient - v);
                out[idx] = v.clamp(params.temp_min, params.temp_max);
            }
        }
        out
    }

    fn test_params(width: u32, height: u32) -> DiffusionParams {
        DiffusionParams {
            width,
            height,
            alpha: ALPHA,
            beta: BETA,
            ambient: AMBIENT_C,
            temp_min: TEMP_MIN_C,
            temp_max: TEMP_MAX_C,
        }
    }

    #[test]
    fn heat_spreads_to_cold_neighbours_and_conserves_when_no_ambient() {
        // 3×1 strip, all conducting, beta=0 so heat is conserved.
        let temps = [30.0, 0.0, 0.0];
        let mask = [1.0, 1.0, 1.0];
        let mut params = test_params(3, 1);
        params.beta = 0.0;
        let next = diffusion_step_cpu(&temps, &mask, 3, 1, &params);

        assert!(next[0] < temps[0], "hot cell cools");
        assert!(next[1] > temps[1], "neighbour warms");
        let before: f32 = temps.iter().sum();
        let after: f32 = next.iter().sum();
        assert!((before - after).abs() < 1e-4, "heat conserved without ambient term");
    }

    #[test]
    fn walls_do_not_conduct() {
        // Hot | wall | cold — the wall (mask 0) blocks all exchange.
        let temps = [30.0, 0.0, -30.0];
        let mask = [1.0, 0.0, 1.0];
        let next = diffusion_step_cpu(&temps, &mask, 3, 1, &test_params(3, 1));

        assert_eq!(next[1], temps[1], "wall holds its value");
        // The hot/cold cells only border the wall, so they have no conducting neighbour.
        assert!((next[0] - 30.0).abs() < 0.2, "hot cell barely changes (ambient pull only)");
        assert!((next[2] + 30.0).abs() < 0.2, "cold cell barely changes (ambient pull only)");
    }

    #[test]
    fn ambient_relaxation_pulls_toward_zero() {
        let temps = [20.0];
        let mask = [1.0];
        let next = diffusion_step_cpu(&temps, &mask, 1, 1, &test_params(1, 1));
        assert!(next[0] < 20.0 && next[0] > 0.0, "isolated warm cell drifts toward ambient");
    }

    #[test]
    fn substeps_is_even_for_buffer_a_final() {
        assert_eq!(SUBSTEPS % 2, 0);
    }
}

# Temperature diffusion (GPU compute)

The ground-floor temperature field ([`TemperatureMap`](../src/map/temperature.rs)) **spreads**
over time: hot/cold patches bleed into their surroundings, heat flows through open floor but not
through walls/void, and the field relaxes slowly toward ambient (0 °C). The diffusion runs on the
GPU via a compute shader; the CPU hypermap remains the source of truth.

Implementation: [`src/map/temperature_diffusion.rs`](../src/map/temperature_diffusion.rs) +
[`assets/shaders/temperature_diffusion.wgsl`](../assets/shaders/temperature_diffusion.wgsl).

## Data flow (CPU-authoritative, GPU is a stateless step)

```
CPU TemperatureMap ──pack window──► upload ──► GPU diffuses N substeps ──► readback ──► CPU TemperatureMap
       (truth)                                  (temperature_diffusion.wgsl)              (apply_window_readback)
```

Each in-game frame ([`diffusion_tick`](../src/map/temperature_diffusion.rs)):

1. Take the **bounding box of `HypermapRuntime::desired_chunk_coords()`**, snapped to whole chunks
   and clipped to `MAX_WINDOW_CHUNKS` (3×3). This is the simulation **window** — one contiguous
   tile grid. Because every visible chunk shares the grid, heat flows **seamlessly across chunk
   borders** with no halo plumbing.
2. **Pack** the window (one chunk handle resolved per chunk, not per tile):
   - `temps[i]` ← CPU temperature read buffer.
   - `mask[i]` ← `static_passability_map` (`>0` ⇒ conducts). Unseeded chunks are ambient + masked
     off, so unloaded space neither shows nor drains heat.
3. **Upload** `temps`→buffer A and `mask`→mask buffer (in-place overwrite of the storage-buffer
   asset's bytes; marking it changed re-uploads it before the compute pass).
4. Render world: ping-pong A↔B for `SUBSTEPS` (even) explicit-diffusion passes — one compute pass
   per substep so each step's writes are visible to the next — then `copy_buffer_to_buffer(A → out)`.
5. The [`Readback`](https://docs.rs/bevy/0.18/bevy/render/gpu_readback/) on `out` fires
   `ReadbackComplete`; [`apply_diffusion_readback`](../src/map/temperature_diffusion.rs) writes the
   result back into the CPU field via
   [`TemperatureMap::apply_window_readback`](../src/map/temperature.rs) (→
   [`TileFieldMap::apply_window_to_read`](../src/map/tile_field.rs)) and marks chunks dirty so the
   F5 overlay repaints.

## Async readback gating

Readback is asynchronous (≥1 frame). The GPU is **stateless per dispatch** — re-packed from the
authoritative CPU field every frame — so panning / chunk load-unload needs no state-preservation
logic. To make sure an in-flight result still matches the live window, results are applied only
after the window (origin + size) has been **stable for `SETTLE_FRAMES`** frames. During a window
change (crossing a chunk boundary) a few results are dropped — spread briefly pauses, no glitch.

There is no per-frame state machine: every frame uploads, dispatches, and reads back. Until the
CPU field actually receives a result, the same input is re-uploaded and re-stepped (idempotent —
`step(S)` is deterministic and applying `S₁` onto `S₁` is a no-op), so the simulation advances
monotonically at roughly the readback cadence without races or regressions.

## Shader math

Per conducting tile (`mask>0`): `v = c + alpha·Σ(neighbourᵢ − c)` over the 4 in-window conducting
neighbours, then `v += beta·(ambient − v)`, clamped to `[TEMP_MIN_C, TEMP_MAX_C]`. Insulators and
window edges are no-flux (skipped), so heat is conserved apart from the deliberate ambient term.
Constants in [`temperature_diffusion.rs`](../src/map/temperature_diffusion.rs): `ALPHA=0.18`,
`BETA=0.0025`, `SUBSTEPS=8`. The CPU mirror `diffusion_step_cpu` (test module) pins this math.

## Persistence & determinism

`temperature.bin` saves whatever the CPU field holds at save time — including live spread, since
readback keeps the CPU current. GPU float diffusion is **not** bit-identical across GPUs; this is a
visual/physical field (not seeded gameplay RNG), so it does not violate the determinism rule.

## Related

- [`field-interactions.md`](field-interactions.md) — actor ↔ field coupling, overlay dirty chunks.
- [`level-persistence.md`](level-persistence.md) — `temperature.bin` format.
- `OPTIMIZATION.md` — per-tick upload/readback cost and the order-independent kernel.

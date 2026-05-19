# Chunk Overlay System

`src/map/chunk_overlay.rs` — `ChunkOverlayPlugin`

## What it does

The overlay system maintains transparent RGBA planes per visible chunk,
floating above the floor mesh. Layers are driven from the CPU.

| Layer | Y offset | Purpose |
|---|---|---|
| Temperature | 0.0004 m | Warm tint from [`TemperatureMap`](../src/map/temperature.rs) (`temperature.bin` on Save) |
| Dirt | 0.0005 m | Black stains from [`DirtMap`](../src/map/dirt.rs) (persisted in `dirt.bin` on Save — [`level-persistence.md`](level-persistence.md)) |
| Generic | 0.001 m | Writable canvas for any system |
| Occupancy | 0.002 m | Debug: subtile passability flags |

## Subtile overlays (generic + occupancy)

| Property | Value |
|---|---|
| Resolution | `OVERLAY_RES × OVERLAY_RES` = 640 × 640 (`CHUNK_SIZE=128 × SUBTILE_COUNT=5`) |
| Texel footprint | 0.2 m × 0.2 m (one subtile) |
| Format | `Rgba8UnormSrgb` |
| Material | `unlit`, `AlphaMode::Blend`, `cull_mode: None` |

Pixel address for subtile `(sx, sy)` inside chunk-local tile `(tx, ty)`:

```
px  = tx * SUBTILE_COUNT + sx   (0..OVERLAY_RES)
py  = ty * SUBTILE_COUNT + sy   (0..OVERLAY_RES)
idx = (py * OVERLAY_RES as usize + px) * 4
```

---

## Tile field overlays (dirt, temperature)

Shared layout via [`tile_field`](../src/map/tile_field.rs) — **one texel per world tile**:

| Property | Value |
|---|---|
| Resolution | `TILE_FIELD_OVERLAY_RES` = 128 × 128 per chunk |
| Texel footprint | 1 m × 1 m |
| Storage | `DoubleBufferedHypermap<f32>` — one scalar per tile (floor `0`) |
| Flush | [`flush_merge`](../src/map/hypermap.rs) when write buffer non-empty |

### Dirt

`DirtOverlayPlugin` + `DirtMapPlugin` — black RGB, alpha = dirt × 255. Seeding ~6% of non-void tiles at `0.1..=0.3`; actors add [`DIRT_TRACK_DEPOSIT`](../src/map/dirt.rs) on tiles they leave (`docs/field-interactions.md`).

### Temperature (heatmap)

`TemperatureOverlayPlugin` + `TemperatureMapPlugin` — values in **°C** on [−30, +30]. Colormap: **blue** (cold) → **white** (0) → **yellow** → **red** (hot). Off by default; HUD **Heat** button or **F5**. Seeding: ~4% cold patches (−26..−6 °C), ~4% warm (6..26 °C), rest 0 °C.

---

## Writing to the generic layer

### System parameters

```rust
Res<ChunkOverlayState>,
ResMut<Assets<Image>>,
ResMut<Assets<StandardMaterial>>,
```

### Pattern

```rust
fn my_overlay_system(
    state: Res<ChunkOverlayState>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let coord = ChunkCoord::new(0, 0);

    let (Some(img_h), Some(mat_h)) = (state.image_for(coord), state.material_for(coord))
    else { return; };

    let Some(image) = images.get_mut(img_h) else { return; };
    if let Some(data) = image.data.as_mut() {
        let px: usize = /* subtile column in the chunk texture */;
        let py: usize = /* subtile row in the chunk texture */;
        let idx = (py * OVERLAY_RES as usize + px) * 4;
        data[idx]     = r;   // red   0–255
        data[idx + 1] = g;   // green 0–255
        data[idx + 2] = b;   // blue  0–255
        data[idx + 3] = a;   // alpha: 0 = transparent, 255 = opaque
    }

    // Required: touch the material after every image write.
    materials.get_mut(mat_h);
}
```

### Coordinate mapping

To paint a specific **world tile** `(world_x, world_y)`:

```rust
use crate::map::hypermap::{world_to_chunk_local, HYPERMAP_CHUNK_SIZE};
use crate::map::passability::SUBTILE_COUNT;

let (coord, local) = world_to_chunk_local(world_x, world_y);

for sy in 0..SUBTILE_COUNT {
    for sx in 0..SUBTILE_COUNT {
        let px = local.x as usize * SUBTILE_COUNT + sx;
        let py = local.y as usize * SUBTILE_COUNT + sy;
        let idx = (py * OVERLAY_RES as usize + px) * 4;
        // write data[idx..idx+4]
    }
}
```

For **tile fields** (dirt / temperature), `px = local.x`, `py = local.y` at
`TILE_FIELD_OVERLAY_RES` (128).

### Lifetime

`ChunkOverlayState` mirrors the set of chunks in
`HypermapRuntime::desired_chunk_coords()`.  Entries appear when a chunk
enters the visibility window and are removed when it leaves.  Always guard
with `image_for` / `material_for` returning `Some` before writing.

---

## Occupancy layer

Reads the **read buffer** of `DynamicPassabilityMap` (last frame's combined
snapshot of static geometry + actor footprints) and colours each of the
640 × 640 subtile texels:

| Flags on subtile | Colour | Meaning |
|---|---|---|
| `FLAG_BLOCKED \| FLAG_CREATURE` | Red | Actor body occupying this subtile |
| `FLAG_BLOCKED` only | Orange | Static wall or corner geometry |
| `FLAG_VOID` | Blue | Void floor (no ground) |
| `0` | Transparent | Fully passable |

### Toggle

Click **"Occ"** in the HUD bar, or press **F4**.  Off by default.

### Update cadence and locking

Refreshed at **~15 Hz** to limit GPU upload bandwidth.

One `DoubleBufferedHypermap::with_chunk_read` call snapshots an entire
128 × 128 tile block under a single `RwLock::read`, then the inner loop
is pure array access.

---

## Known limitation — Bevy #20269

`MeshMaterial3d` does not listen for `AssetEvent<Image>` and will not
re-upload a texture to the GPU when only the image asset changes.  Call
`materials.get_mut(mat_handle)` after each image write.

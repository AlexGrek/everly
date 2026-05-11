# Chunk Overlay System

`src/map/chunk_overlay.rs` — `ChunkOverlayPlugin`

## What it does

The overlay system maintains two transparent RGBA planes per visible chunk,
floating above the floor mesh.  Both are driven entirely from the CPU.

| Layer | Y offset | Purpose |
|---|---|---|
| Generic | 0.001 m | Writable canvas for any system |
| Occupancy | 0.002 m | Debug: subtile passability flags |

## Texture layout

Both layers share the same geometry and texture format:

| Property | Value |
|---|---|
| Resolution | `OVERLAY_RES × OVERLAY_RES` = 640 × 640 (`CHUNK_SIZE=128 × SUBTILE_COUNT=5`) |
| Texel footprint | 0.2 m × 0.2 m (one subtile) |
| Format | `Rgba8UnormSrgb` |
| Default state | All texels transparent (`alpha = 0`) |
| Material | `unlit`, `AlphaMode::Blend`, `cull_mode: None` |

Pixel address for subtile `(sx, sy)` inside chunk-local tile `(tx, ty)`:

```
px  = tx * SUBTILE_COUNT + sx   (0..OVERLAY_RES)
py  = ty * SUBTILE_COUNT + sy   (0..OVERLAY_RES)
idx = (py * OVERLAY_RES as usize + px) * 4
```

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
    // MeshMaterial3d does not re-upload the texture unless the material is
    // also marked dirty (Bevy bug #20269).
    materials.get_mut(mat_h);
}
```

### Coordinate mapping

To paint a specific **world tile** `(world_x, world_y)`:

```rust
use crate::map::hypermap::{world_to_chunk_local, HYPERMAP_CHUNK_SIZE};
use crate::map::passability::SUBTILE_COUNT;

let (coord, local) = world_to_chunk_local(world_x, world_y);
// local.x / local.y are in 0..HYPERMAP_CHUNK_SIZE

// To shade the whole tile, iterate its 5×5 subtile block:
for sy in 0..SUBTILE_COUNT {
    for sx in 0..SUBTILE_COUNT {
        let px = local.x as usize * SUBTILE_COUNT + sx;
        let py = local.y as usize * SUBTILE_COUNT + sy;
        let idx = (py * OVERLAY_RES as usize + px) * 4;
        // write data[idx..idx+4]
    }
}
```

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
The resource can also be set from code:

```rust
fn enable(mut enabled: ResMut<OccupancyOverlayEnabled>) {
    enabled.0 = true;
}
```

### Update cadence and locking

Refreshed at **~15 Hz** to limit GPU upload bandwidth
(~14 MB/tick across 9 visible chunks).

One `DoubleBufferedHypermap::with_chunk_read` call snapshots an entire
128 × 128 tile block under a single `RwLock::read`, then the inner loop
is pure array access — no per-subtile lock overhead.  If a passability
chunk is absent (area never touched), the overlay is cleared to
transparent, which correctly represents an all-passable area.

---

## Known limitation — Bevy #20269

`MeshMaterial3d` does not listen for `AssetEvent<Image>` and will not
re-upload a texture to the GPU when only the image asset changes.  The
workaround applied throughout this module is to call
`materials.get_mut(mat_handle)` after each image write, which marks the
material dirty and forces a re-upload.  Both `image_for` and `material_for`
on `ChunkOverlayState` exist precisely so external painters can apply the
same workaround.  When the upstream bug is fixed, the `materials.get_mut`
calls can be removed.

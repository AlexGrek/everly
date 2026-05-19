# Tile fields (dirt, temperature)

Scalar hypermaps with **one `f32` per world tile** on ground floor `0`. Shared
implementation: [`src/map/tile_field.rs`](../src/map/tile_field.rs).

## Storage

| Map | Resource | Default | Range | Seeding |
|-----|----------|---------|-------|---------|
| Dirt | `DirtMap` | `0.0` | `0..=1` | ~6% tiles → `0.1..=0.3` |
| Temperature | `TemperatureMap` | `0` °C | `−30..=+30` °C | ~4% cold / ~4% warm patches |

Both use `DoubleBufferedHypermap<f32>`:

- Writers (seed, field interaction) → **write** buffer
- `flush_*_map` → `flush_merge` when write buffer non-empty
- Overlays and `get_tile` → **read** buffer

## Overlays

128×128 RGBA per chunk (`TILE_FIELD_OVERLAY_RES`), one texel per tile. See
`docs/chunk-overlay.md` for Y stacking and colours.

## API sketch

```rust
dirt.get_tile(wx, wy);
dirt.set_tile(wx, wy, value);
dirt.add_tile_dirt(wx, wy, delta);

temperature.get_tile_c(wx, wy);
temperature.set_tile_c(wx, wy, celsius);
temperature.add_tile_c(wx, wy, delta_c);
```

Actor dirt deposits: `docs/field-interactions.md`.

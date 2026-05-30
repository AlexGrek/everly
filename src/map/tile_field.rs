//! One `f32` sample per world tile (ground floor) in a double-buffered hypermap.
//!
//! Used by dirt, temperature, and future tile-resolution fields. Overlay texels are
//! [`TILE_FIELD_OVERLAY_RES`]×[`TILE_FIELD_OVERLAY_RES`] (one texel per tile).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::map::hypermap::{
    world_to_chunk_local, ChunkCoord, DoubleBufferedHypermap, Hypermap, LocalCoord,
    HYPERMAP_CHUNK_SIZE,
};

/// Overlay texture size per chunk edge — one texel per world tile.
pub const TILE_FIELD_OVERLAY_RES: u32 = crate::map::hypermap::HYPERMAP_CHUNK_SIZE as u32;

/// Double-buffered hypermap with one scalar per world tile (floor `0`).
#[derive(Debug)]
pub struct TileFieldMap {
    inner: Arc<DoubleBufferedHypermap<f32>>,
    dirty_chunks: Mutex<HashSet<ChunkCoord>>,
    default_value: f32,
    clamp_min: f32,
    clamp_max: f32,
}

impl TileFieldMap {
    pub fn new(default_value: f32, clamp_max: f32) -> Self {
        Self::new_ranged(default_value, 0.0, clamp_max)
    }

    pub fn new_ranged(default_value: f32, clamp_min: f32, clamp_max: f32) -> Self {
        Self {
            inner: Arc::new(DoubleBufferedHypermap::new(default_value)),
            dirty_chunks: Mutex::new(HashSet::new()),
            default_value,
            clamp_min,
            clamp_max,
        }
    }

    #[inline]
    fn clamp_value(&self, value: f32) -> f32 {
        value.clamp(self.clamp_min, self.clamp_max)
    }

    pub fn inner(&self) -> &DoubleBufferedHypermap<f32> {
        &self.inner
    }

    pub fn read_map(&self) -> &Hypermap<f32> {
        self.inner.read_map()
    }

    pub fn default_value(&self) -> f32 {
        self.default_value
    }

    pub fn get_tile(&self, world_x: i32, world_y: i32) -> f32 {
        self.inner.get(world_x, world_y)
    }

    pub fn set_tile(&self, world_x: i32, world_y: i32, value: f32) {
        let (coord, _) = world_to_chunk_local(world_x, world_y);
        self.cow_write_chunk(coord);
        let v = self.clamp_value(value);
        self.inner.set(world_x, world_y, v);
        self.mark_dirty(coord);
    }

    pub fn add_tile(&self, world_x: i32, world_y: i32, delta: f32) {
        let (coord, _) = world_to_chunk_local(world_x, world_y);
        self.cow_write_chunk(coord);
        self.inner.update(world_x, world_y, |v| {
            *v = self.clamp_value(*v + delta);
        });
        self.mark_dirty(coord);
    }

    /// Copy-on-write: if the write chunk for `coord` hasn't been touched this frame,
    /// seed it from the read chunk so that `flush_merge` doesn't overwrite unmodified
    /// tiles with the write buffer's default (0.0).
    fn cow_write_chunk(&self, coord: ChunkCoord) {
        if self.inner.write_map().has_chunk(coord) {
            return;
        }
        let Some(read_handle) = self.inner.read_map().get_chunk(coord) else {
            return;
        };
        let read_guard = read_handle.read().expect("chunk lock poisoned");
        self.inner.write_map().with_chunk_write(coord, |write_chunk| {
            for y in 0..HYPERMAP_CHUNK_SIZE {
                for x in 0..HYPERMAP_CHUNK_SIZE {
                    let local = LocalCoord::new(x, y);
                    write_chunk.set_local(local, *read_guard.get_local(local));
                }
            }
        });
    }

    pub fn mark_dirty(&self, coord: ChunkCoord) {
        self.dirty_chunks
            .lock()
            .expect("tile_field dirty_chunks lock poisoned")
            .insert(coord);
    }

    /// Resets every tile in `coord` to [`Self::default_value`] and drops write-buffer
    /// data so the next [`DoubleBufferedHypermap::flush_if_pending`] promotes clean state.
    pub fn reset_chunk(&self, coord: ChunkCoord) {
        let default = self.default_value;
        if self.inner.write_map().has_chunk(coord) {
            self.inner.write_map().with_chunk_write(coord, |chunk| {
                for ly in 0..HYPERMAP_CHUNK_SIZE {
                    for lx in 0..HYPERMAP_CHUNK_SIZE {
                        chunk.set_local(LocalCoord::new(lx, ly), default);
                    }
                }
            });
        }
        self.mark_dirty(coord);
        self.flush_if_pending();
    }

    pub fn take_dirty_chunks(&self) -> HashSet<ChunkCoord> {
        std::mem::take(
            &mut *self
                .dirty_chunks
                .lock()
                .expect("tile_field dirty_chunks lock poisoned"),
        )
    }

    /// Merges write→read when the write buffer has any chunks.
    pub fn flush_if_pending(&self) {
        if self.inner.write_map().loaded_chunk_count() == 0 {
            return;
        }
        let pending = self.inner.write_map().loaded_chunks();
        self.inner.flush_merge();
        let mut dirty = self
            .dirty_chunks
            .lock()
            .expect("tile_field dirty_chunks lock poisoned");
        dirty.extend(pending);
    }

    /// Writes a packed row-major window (`width`×`height`, origin at world tile
    /// `(origin_x, origin_y)`) **directly into the read buffer**, clamped to the field
    /// range. Only chunks already loaded on the read side are touched — unseeded chunks
    /// are skipped so this never materializes empty geometry. Every touched chunk is
    /// marked dirty so overlays repaint. Used by the GPU diffusion readback to push the
    /// evolved field back onto the CPU source of truth (`src/map/temperature_diffusion.rs`).
    pub fn apply_window_to_read(
        &self,
        origin_x: i32,
        origin_y: i32,
        width: usize,
        height: usize,
        data: &[f32],
    ) {
        debug_assert_eq!(data.len(), width.saturating_mul(height));
        if width == 0 || height == 0 {
            return;
        }
        let (min_chunk, _) = world_to_chunk_local(origin_x, origin_y);
        let (max_chunk, _) =
            world_to_chunk_local(origin_x + width as i32 - 1, origin_y + height as i32 - 1);

        for cy in min_chunk.y..=max_chunk.y {
            for cx in min_chunk.x..=max_chunk.x {
                let coord = ChunkCoord::new(cx, cy);
                if !self.inner.read_map().has_chunk(coord) {
                    continue;
                }
                let chunk_origin_x = cx * HYPERMAP_CHUNK_SIZE;
                let chunk_origin_y = cy * HYPERMAP_CHUNK_SIZE;
                self.inner.read_map().with_chunk_write(coord, |chunk| {
                    for ly in 0..HYPERMAP_CHUNK_SIZE {
                        let wy = chunk_origin_y + ly;
                        let win_y = wy - origin_y;
                        if win_y < 0 || win_y >= height as i32 {
                            continue;
                        }
                        for lx in 0..HYPERMAP_CHUNK_SIZE {
                            let wx = chunk_origin_x + lx;
                            let win_x = wx - origin_x;
                            if win_x < 0 || win_x >= width as i32 {
                                continue;
                            }
                            let idx = win_y as usize * width + win_x as usize;
                            let value = self.clamp_value(data[idx]);
                            chunk.set_local(LocalCoord::new(lx, ly), value);
                        }
                    }
                });
                self.mark_dirty(coord);
            }
        }
    }

    /// Writes one chunk of tile samples into an RGBA8 image (`TILE_FIELD_OVERLAY_RES`²).
    pub fn paint_chunk_to_rgba(
        data: &mut [u8],
        chunk: &crate::map::hypermap::HypermapChunk<f32>,
        sample_to_rgba: impl Fn(f32) -> [u8; 4],
    ) {
        let res = HYPERMAP_CHUNK_SIZE as usize;
        for ly in 0..res {
            for lx in 0..res {
                let local = LocalCoord::new(lx as i32, ly as i32);
                let value = *chunk.get_local(local);
                let idx = (ly * res + lx) * 4;
                data[idx..idx + 4].copy_from_slice(&sample_to_rgba(value));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_window_writes_loaded_chunk_clamped_and_marks_dirty() {
        let field = TileFieldMap::new(0.0, 1.0);
        // Promote chunk (0,0) onto the read side so the window write targets it.
        field.set_tile(0, 0, 0.0);
        field.flush_if_pending();
        let _ = field.take_dirty_chunks();

        // 2×2 window at origin (0,0): row-major, with one over-range value to clamp.
        let data = [0.25_f32, 5.0, -1.0, 0.75];
        field.apply_window_to_read(0, 0, 2, 2, &data);

        assert_eq!(field.get_tile(0, 0), 0.25);
        assert_eq!(field.get_tile(1, 0), 1.0, "clamped to max");
        assert_eq!(field.get_tile(0, 1), 0.0, "clamped to min");
        assert_eq!(field.get_tile(1, 1), 0.75);

        let dirty = field.take_dirty_chunks();
        assert!(dirty.contains(&ChunkCoord::new(0, 0)));
    }

    #[test]
    fn apply_window_skips_unloaded_chunks() {
        let field = TileFieldMap::new(0.0, 1.0);
        // No chunk loaded on the read side.
        field.apply_window_to_read(0, 0, 2, 2, &[0.5, 0.5, 0.5, 0.5]);
        assert_eq!(field.read_map().loaded_chunk_count(), 0, "must not materialize chunks");
        assert_eq!(field.get_tile(0, 0), 0.0);
    }
}

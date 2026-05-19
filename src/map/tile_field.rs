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
    clamp_max: f32,
}

impl TileFieldMap {
    pub fn new(default_value: f32, clamp_max: f32) -> Self {
        Self {
            inner: Arc::new(DoubleBufferedHypermap::new(default_value)),
            dirty_chunks: Mutex::new(HashSet::new()),
            default_value,
            clamp_max,
        }
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
        let v = value.clamp(0.0, self.clamp_max);
        self.inner.set(world_x, world_y, v);
        self.mark_dirty(coord);
    }

    pub fn add_tile(&self, world_x: i32, world_y: i32, delta: f32) {
        let (coord, _) = world_to_chunk_local(world_x, world_y);
        self.inner.update(world_x, world_y, |v| {
            *v = (*v + delta).clamp(0.0, self.clamp_max);
        });
        self.mark_dirty(coord);
    }

    pub fn mark_dirty(&self, coord: ChunkCoord) {
        self.dirty_chunks
            .lock()
            .expect("tile_field dirty_chunks lock poisoned")
            .insert(coord);
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

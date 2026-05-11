//! Dynamic passability map — per-tile 5×5 boolean micro-grid stored in a
//! [`DoubleBufferedHypermap`].
//!
//! Each world tile is subdivided into `SUBTILE_COUNT × SUBTILE_COUNT` (5×5)
//! sub-cells. A sub-cell value of `true` means passable; `false` means blocked.
//! The write buffer collects obstacle state each tick; [`flush`](DynamicPassabilityMap::flush)
//! promotes it to the read side for consumers (future pathfinding, AI queries).
//!
//! This map is **not** wired into pathfinding yet — it only provides the data store.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use bevy::prelude::*;

use crate::map::hypermap::DoubleBufferedHypermap;

/// Number of boolean sub-cells along each axis of a single world tile.
pub const SUBTILE_COUNT: usize = 5;

/// Per-tile micro-grid of passability booleans.
///
/// Indexed `grid[row][col]` where `row` and `col` are in `0..SUBTILE_COUNT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubtilePassability {
    pub grid: [[bool; SUBTILE_COUNT]; SUBTILE_COUNT],
}

impl SubtilePassability {
    pub const ALL_PASSABLE: Self = Self {
        grid: [[true; SUBTILE_COUNT]; SUBTILE_COUNT],
    };

    pub const ALL_BLOCKED: Self = Self {
        grid: [[false; SUBTILE_COUNT]; SUBTILE_COUNT],
    };

    #[inline]
    pub fn is_passable(&self, row: usize, col: usize) -> bool {
        self.grid[row][col]
    }

    #[inline]
    pub fn set(&mut self, row: usize, col: usize, passable: bool) {
        self.grid[row][col] = passable;
    }
}

impl Default for SubtilePassability {
    fn default() -> Self {
        Self::ALL_PASSABLE
    }
}

/// Bevy resource wrapping a double-buffered hypermap of [`SubtilePassability`].
///
/// Systems that place dynamic obstacles write into the write buffer via the
/// delegated `set*` / `update` / `with_chunk_write` methods. At a chosen sync
/// point, call [`flush`](Self::flush) to promote writes to the read side.
#[derive(Resource)]
pub struct DynamicPassabilityMap {
    inner: Arc<DoubleBufferedHypermap<SubtilePassability>>,
}

/// Absolute world-subtile coordinates that an actor currently occupies.
pub type ActorFootprint = Vec<IVec2>;

/// Error produced when applying a footprint update to the dynamic map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryUpdateFootprintError {
    /// The requested circular radius is invalid.
    InvalidRadius(i32),
    /// Target footprint intersects an already-blocked subtile that is not in
    /// the actor's previous footprint.
    BlockedByOccupancy { world_subtile: IVec2 },
}

impl DynamicPassabilityMap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DoubleBufferedHypermap::new(SubtilePassability::ALL_PASSABLE)),
        }
    }

    pub fn inner(&self) -> &DoubleBufferedHypermap<SubtilePassability> {
        &self.inner
    }

    pub fn flush(&self) {
        self.inner.flush();
    }

    /// Tries to update actor occupancy and returns the new footprint to store.
    ///
    /// This method centralizes movement-collision logic so actor code stays
    /// simple. It:
    ///
    /// 1. Builds the actor's circular shadow at `next_center_subtile`.
    /// 2. Checks collisions against the passability **read** buffer.
    /// 3. Ignores collisions with `previous_footprint` (self-overlap).
    /// 4. On success, writes the new footprint as blocked (`false`) into the
    ///    passability **write** buffer and returns it.
    /// 5. On failure, re-stamps `previous_footprint` into the write buffer so
    ///    the actor keeps occupying its old cells for the next frame.
    pub fn try_update_footprint(
        &self,
        next_center_subtile: IVec2,
        radius_subtiles: i32,
        previous_footprint: &[IVec2],
    ) -> Result<ActorFootprint, TryUpdateFootprintError> {
        if radius_subtiles < 0 {
            return Err(TryUpdateFootprintError::InvalidRadius(radius_subtiles));
        }

        let subtile_map = SubtilePassabilityMap::new(self);
        let previous_set: HashSet<IVec2> = previous_footprint.iter().copied().collect();
        let mut new_footprint = Vec::new();

        for offset in baked_circle_shadow(radius_subtiles) {
            let target = next_center_subtile + *offset;
            let is_passable = subtile_map.subtile_xy(0, 0, target.x, target.y);
            let is_self_overlap = previous_set.contains(&target);
            if !is_passable && !is_self_overlap {
                self.write_footprint(previous_footprint);
                return Err(TryUpdateFootprintError::BlockedByOccupancy {
                    world_subtile: target,
                });
            }
            new_footprint.push(target);
        }

        self.write_footprint(&new_footprint);
        Ok(new_footprint)
    }

    /// Writes a footprint as blocked (`false`) into the passability write buffer.
    pub fn write_footprint(&self, footprint: &[IVec2]) {
        let subtile_map = SubtilePassabilityMap::new(self);
        for sub in footprint {
            subtile_map.set_subtile_xy(0, 0, sub.x, sub.y, false);
        }
    }
}

/// Subtile-level view over a [`DoubleBufferedHypermap<SubtilePassability>`].
///
/// Addresses individual boolean sub-cells using a **(tile, shift)** scheme:
/// the caller supplies a reference tile coordinate and an arbitrary signed
/// subtile offset. The offset is **not** clamped to `0..SUBTILE_COUNT` — it
/// freely overflows into neighboring tiles, so relative addressing from an
/// object's center tile works without manual tile arithmetic.
///
/// # Coordinate resolution
///
/// Given tile `(tx, ty)` and subtile shift `(sx, sy)`:
///
/// ```text
/// global_sub_x = tx * SUBTILE_COUNT + sx
/// resolved_tile_x = floor_div(global_sub_x, SUBTILE_COUNT)
/// resolved_local_x = floor_mod(global_sub_x, SUBTILE_COUNT)   // 0..SUBTILE_COUNT
/// ```
///
/// Same for the Y axis. This lets `shift = (-3, 12)` transparently reach
/// a subtile one tile to the left and two tiles down from `(tx, ty)`.
///
/// # Example
///
/// ```ignore
/// let view = SubtilePassabilityMap::new(&dynamic_passability_map);
///
/// // Query 2 subtiles to the right and 1 subtile up from tile (10, 20), center sub-cell.
/// let passable = view.subtile_xy(10, 20, 4, -1);
///
/// // Same query with tuple pairs:
/// let passable = view.subtile((10, 20), (4, -1));
/// ```
pub struct SubtilePassabilityMap<'a> {
    map: &'a DoubleBufferedHypermap<SubtilePassability>,
}

impl<'a> SubtilePassabilityMap<'a> {
    pub fn new(source: &'a DynamicPassabilityMap) -> Self {
        Self { map: source.inner() }
    }

    pub fn from_raw(map: &'a DoubleBufferedHypermap<SubtilePassability>) -> Self {
        Self { map }
    }

    /// Read a single subtile's passability.
    ///
    /// `tile_index` is the world tile `(x, y)`. `shift` is a signed subtile
    /// offset `(dx, dy)` relative to that tile's `(0, 0)` sub-cell. The shift
    /// may exceed `±SUBTILE_COUNT` — it will resolve to whichever tile and
    /// local sub-cell the global subtile coordinate lands on.
    #[inline]
    pub fn subtile(&self, tile_index: (i32, i32), shift: (i32, i32)) -> bool {
        self.subtile_xy(tile_index.0, tile_index.1, shift.0, shift.1)
    }

    /// Scalar-argument form of [`subtile`](Self::subtile).
    ///
    /// `tile_x, tile_y` — world tile coordinate.
    /// `shift_x, shift_y` — signed subtile offset (unbounded).
    #[inline]
    pub fn subtile_xy(&self, tile_x: i32, tile_y: i32, shift_x: i32, shift_y: i32) -> bool {
        let (resolved_tile_x, local_x) = resolve_subtile(tile_x, shift_x);
        let (resolved_tile_y, local_y) = resolve_subtile(tile_y, shift_y);
        let cell = self.map.get(resolved_tile_x, resolved_tile_y);
        cell.is_passable(local_y, local_x)
    }

    /// Write a single subtile's passability into the **write** buffer.
    ///
    /// Same addressing rules as [`subtile`](Self::subtile).
    #[inline]
    pub fn set_subtile(&self, tile_index: (i32, i32), shift: (i32, i32), passable: bool) {
        self.set_subtile_xy(tile_index.0, tile_index.1, shift.0, shift.1, passable);
    }

    /// Scalar-argument form of [`set_subtile`](Self::set_subtile).
    #[inline]
    pub fn set_subtile_xy(
        &self,
        tile_x: i32,
        tile_y: i32,
        shift_x: i32,
        shift_y: i32,
        passable: bool,
    ) {
        let (resolved_tile_x, local_x) = resolve_subtile(tile_x, shift_x);
        let (resolved_tile_y, local_y) = resolve_subtile(tile_y, shift_y);
        self.map.update(resolved_tile_x, resolved_tile_y, |cell| {
            cell.set(local_y, local_x, passable);
        });
    }
}

/// Resolve a tile coordinate + signed subtile shift into the actual tile and
/// the in-tile local index (`0..SUBTILE_COUNT`).
#[inline]
fn resolve_subtile(tile: i32, shift: i32) -> (i32, usize) {
    let sc = SUBTILE_COUNT as i32;
    let global = tile * sc + shift;
    let resolved_tile = global.div_euclid(sc);
    let local = global.rem_euclid(sc) as usize;
    (resolved_tile, local)
}

/// Returns cached integer offsets for a filled circle of `radius_subtiles`.
///
/// Includes every `(dx, dy)` where `dx*dx + dy*dy <= r*r`.
fn baked_circle_shadow(radius_subtiles: i32) -> &'static [IVec2] {
    static CACHE: OnceLock<Mutex<HashMap<i32, &'static [IVec2]>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let mut guard = cache.lock().expect("passability shadow cache lock poisoned");
    if let Some(existing) = guard.get(&radius_subtiles) {
        return existing;
    }

    let r = radius_subtiles.max(0);
    let rr = r * r;
    let mut offsets = Vec::new();
    for y in -r..=r {
        for x in -r..=r {
            if x * x + y * y <= rr {
                offsets.push(IVec2::new(x, y));
            }
        }
    }

    let leaked: &'static [IVec2] = Box::leak(offsets.into_boxed_slice());
    guard.insert(radius_subtiles, leaked);
    leaked
}

pub struct PassabilityMapPlugin;

impl Plugin for PassabilityMapPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(DynamicPassabilityMap::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtile_default_is_all_passable() {
        let s = SubtilePassability::default();
        for r in 0..SUBTILE_COUNT {
            for c in 0..SUBTILE_COUNT {
                assert!(s.is_passable(r, c));
            }
        }
    }

    #[test]
    fn subtile_set_and_query() {
        let mut s = SubtilePassability::ALL_PASSABLE;
        s.set(2, 3, false);
        assert!(!s.is_passable(2, 3));
        assert!(s.is_passable(0, 0));
    }

    #[test]
    fn dynamic_map_write_read_flush_cycle() {
        let map = DynamicPassabilityMap::new();

        let mut blocked = SubtilePassability::ALL_PASSABLE;
        blocked.set(0, 0, false);
        blocked.set(4, 4, false);
        map.inner().set(10, 20, blocked);

        assert_eq!(
            map.inner().get(10, 20),
            SubtilePassability::ALL_PASSABLE,
            "read side still default before flush"
        );

        map.flush();

        let read = map.inner().get(10, 20);
        assert!(!read.is_passable(0, 0));
        assert!(!read.is_passable(4, 4));
        assert!(read.is_passable(2, 2));
    }

    #[test]
    fn dynamic_map_flush_clears_write() {
        let map = DynamicPassabilityMap::new();
        map.inner().set(5, 5, SubtilePassability::ALL_BLOCKED);
        map.flush();

        assert_eq!(map.inner().get(5, 5), SubtilePassability::ALL_BLOCKED);

        map.flush();
        assert_eq!(
            map.inner().get(5, 5),
            SubtilePassability::ALL_PASSABLE,
            "second flush with no writes resets to clean"
        );
    }

    // --- SubtilePassabilityMap ---

    #[test]
    fn subtile_map_read_within_tile() {
        let map = DynamicPassabilityMap::new();
        let mut tile = SubtilePassability::ALL_PASSABLE;
        tile.set(2, 3, false);
        map.inner().set(10, 20, tile);
        map.flush();

        let view = SubtilePassabilityMap::new(&map);
        assert!(!view.subtile((10, 20), (3, 2)));
        assert!(view.subtile((10, 20), (0, 0)));
    }

    #[test]
    fn subtile_map_positive_overflow_into_neighbor() {
        let map = DynamicPassabilityMap::new();
        let mut tile = SubtilePassability::ALL_PASSABLE;
        tile.set(0, 1, false);
        map.inner().set(11, 20, tile);
        map.flush();

        let view = SubtilePassabilityMap::new(&map);
        // shift_x=6 from tile 10 → tile 11, local_x=1; shift_y=0 → local_y=0
        assert!(!view.subtile_xy(10, 20, 6, 0));
    }

    #[test]
    fn subtile_map_negative_overflow_into_neighbor() {
        let map = DynamicPassabilityMap::new();
        let mut tile = SubtilePassability::ALL_PASSABLE;
        tile.set(4, 4, false);
        map.inner().set(9, 19, tile);
        map.flush();

        let view = SubtilePassabilityMap::new(&map);
        // shift_x=-1 from tile 10 → tile 9, local_x=4; shift_y=-1 → tile 19, local_y=4
        assert!(!view.subtile_xy(10, 20, -1, -1));
    }

    #[test]
    fn subtile_map_large_shift_crosses_multiple_tiles() {
        let map = DynamicPassabilityMap::new();
        let mut tile = SubtilePassability::ALL_PASSABLE;
        // shift (12, 2) from tile (10, 20):
        //   x: global = 10*5+12 = 62 → tile 12, local_x 2
        //   y: global = 20*5+2  = 102 → tile 20, local_y 2
        // → is_passable(row=2, col=2)
        tile.set(2, 2, false);
        map.inner().set(12, 20, tile);
        map.flush();

        let view = SubtilePassabilityMap::new(&map);
        assert!(!view.subtile((10, 20), (12, 2)));
    }

    #[test]
    fn subtile_map_set_via_shifted_address() {
        let map = DynamicPassabilityMap::new();
        let view = SubtilePassabilityMap::new(&map);

        // Write through the subtile view into the write buffer.
        view.set_subtile((5, 5), (7, -2), false);
        map.flush();

        // shift_x=7 from tile 5 → tile 6, local_x=2
        // shift_y=-2 from tile 5 → tile 4, local_y=3
        let cell = map.inner().get(6, 4);
        assert!(!cell.is_passable(3, 2));
    }

    #[test]
    fn resolve_subtile_basic() {
        assert_eq!(resolve_subtile(0, 0), (0, 0));
        assert_eq!(resolve_subtile(0, 4), (0, 4));
        assert_eq!(resolve_subtile(0, 5), (1, 0));
        assert_eq!(resolve_subtile(0, -1), (-1, 4));
        assert_eq!(resolve_subtile(3, -6), (1, 4));
    }

    #[test]
    fn try_update_footprint_writes_and_returns_shape() {
        let map = DynamicPassabilityMap::new();
        let new_fp = map
            .try_update_footprint(IVec2::new(20, 20), 1, &[])
            .expect("footprint should be writable");
        map.flush();
        let view = SubtilePassabilityMap::new(&map);

        assert!(!new_fp.is_empty());
        for sub in &new_fp {
            assert!(
                !view.subtile_xy(0, 0, sub.x, sub.y),
                "every returned subtile must be blocked in read buffer"
            );
        }
    }

    #[test]
    fn try_update_footprint_ignores_previous_self_overlap() {
        let map = DynamicPassabilityMap::new();
        let old_fp = map
            .try_update_footprint(IVec2::new(40, 40), 2, &[])
            .expect("initial footprint");
        map.flush();

        // Move by one subtile to force large overlap with previous footprint.
        let moved = map.try_update_footprint(IVec2::new(41, 40), 2, &old_fp);
        assert!(moved.is_ok(), "self-overlap should not block movement");
    }

    #[test]
    fn try_update_footprint_blocks_on_foreign_occupancy() {
        let map = DynamicPassabilityMap::new();
        map.write_footprint(&[IVec2::new(50, 50)]);
        map.flush();

        let err = map
            .try_update_footprint(IVec2::new(50, 50), 0, &[])
            .expect_err("blocked cell should reject footprint");
        assert!(matches!(
            err,
            TryUpdateFootprintError::BlockedByOccupancy { world_subtile }
                if world_subtile == IVec2::new(50, 50)
        ));
    }
}

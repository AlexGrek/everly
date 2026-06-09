//! Unified per-subtile passability map — both static geometry (walls, void)
//! and dynamic obstacles (creature bodies) are stored in one
//! [`DoubleBufferedHypermap`] whose cells hold `u64` flag bitmasks.
//!
//! ## Subtile grid
//!
//! Each world tile is subdivided into `SUBTILE_COUNT × SUBTILE_COUNT` (5×5)
//! sub-cells. A sub-cell value of `0` means fully passable; non-zero means
//! some kind of obstacle described by the flag bits below.
//!
//! ## Flag layout
//!
//! | Constant | Bit | Meaning |
//! |---|---|---|
//! | [`FLAG_BLOCKED`] | 0 | Hard obstacle — wall slab or creature body. **Impassable for all** actors. |
//! | [`FLAG_VOID`]    | 1 | Void space (no floor). Passable for flyers, blocked for walkers. |
//! | [`FLAG_CREATURE`]| 2 | The [`FLAG_BLOCKED`] bit was set by a creature, not static geometry. Enables distinguishing "wall" from "unit" collisions. |
//!
//! ## Frame lifecycle
//!
//! 1. [`DynamicPassabilityMap::flush`] — promotes the write buffer to read,
//!    resets write to all-zero.
//! 2. [`stamp_static_passability`] — iterates all loaded geometry chunks and
//!    ORs wall / void flags into the write buffer (subtile-accurate).
//! 3. Actor think systems fill their `move_buffer`.
//! 4. [`process_actors`](crate::actor::process_actors) — for each actor calls
//!    [`try_update_footprint`] which reads from **read** (has last frame's
//!    snapshot), then ORs `FLAG_BLOCKED | FLAG_CREATURE` into **write**.
//! 5. Next frame begins at step 1.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use bevy::prelude::*;

use crate::map::hypermap::{
    world_to_chunk_local, ChunkCoord, DoubleBufferedHypermap, Hypermap, HypermapChunkHandle,
};
use crate::map::world_map::{CellType, WallCorner, WallMask, MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};

/// Number of sub-cells along each axis of a single world tile.
pub const SUBTILE_COUNT: usize = 5;

// ---------------------------------------------------------------------------
// Passability flag constants
// ---------------------------------------------------------------------------

/// Hard obstacle — wall geometry or creature body. Impassable for **all** actors.
pub const FLAG_BLOCKED: u64 = 1 << 0;
/// Void space — no floor present. Passable for flyers; blocked for ground walkers.
pub const FLAG_VOID: u64 = 1 << 1;
/// Creature-body marker. When set together with [`FLAG_BLOCKED`], the block
/// was placed by a creature rather than static geometry. Callers can use this
/// to distinguish "wall collision" from "unit collision".
pub const FLAG_CREATURE: u64 = 1 << 2;

// ---------------------------------------------------------------------------
// Per-tile subtile grid
// ---------------------------------------------------------------------------

/// Per-tile micro-grid of `u64` passability flags.
///
/// Flat layout `cells[row * SUBTILE_COUNT + col]`; all-zero = fully passable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubtilePassability {
    pub cells: [u64; SUBTILE_COUNT * SUBTILE_COUNT],
}

impl SubtilePassability {
    pub const EMPTY: Self = Self { cells: [0; SUBTILE_COUNT * SUBTILE_COUNT] };

    #[inline]
    pub fn flags_at(&self, row: usize, col: usize) -> u64 {
        self.cells[row * SUBTILE_COUNT + col]
    }

    /// ORs `flags` into the cell at `(row, col)`. Multiple writers can safely
    /// accumulate different flag bits into the same cell.
    #[inline]
    pub fn or_flags(&mut self, row: usize, col: usize, flags: u64) {
        self.cells[row * SUBTILE_COUNT + col] |= flags;
    }
}

impl Default for SubtilePassability {
    fn default() -> Self {
        Self::EMPTY
    }
}

// ---------------------------------------------------------------------------
// DynamicPassabilityMap resource
// ---------------------------------------------------------------------------

/// Bevy resource wrapping a double-buffered hypermap of [`SubtilePassability`].
///
/// The write side accumulates this frame's obstacle state (static geometry
/// stamped by [`stamp_static_passability`] plus actor footprints); [`flush`]
/// then promotes it to the read side for the next frame's collision queries.
#[derive(Resource)]
pub struct DynamicPassabilityMap {
    inner: Arc<DoubleBufferedHypermap<SubtilePassability>>,
}

/// Absolute world-subtile coordinates that an actor currently occupies.
pub type ActorFootprint = Vec<IVec2>;

/// Error produced when applying a footprint update to the dynamic map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryUpdateFootprintError {
    /// The requested circular radius is invalid (negative).
    InvalidRadius(i32),
    /// Target footprint intersects an already-blocked subtile that is **not**
    /// in the actor's previous footprint, and the blocking was set by another
    /// creature (`FLAG_BLOCKED | FLAG_CREATURE` both set).
    BlockedByOccupancy { world_subtile: IVec2 },
    /// Target footprint intersects a static obstacle (wall slab or void for
    /// ground walkers — `FLAG_BLOCKED` or `FLAG_VOID` without `FLAG_CREATURE`).
    BlockedByStatic { world_subtile: IVec2 },
}

impl DynamicPassabilityMap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DoubleBufferedHypermap::new(SubtilePassability::EMPTY)),
        }
    }

    pub fn inner(&self) -> &DoubleBufferedHypermap<SubtilePassability> {
        &self.inner
    }

    pub fn flush(&self) {
        self.inner.flush();
    }

    /// Clears dynamic actor occupancy (`FLAG_BLOCKED | FLAG_CREATURE`) for a circular
    /// footprint on both buffers — use before despawning an actor (e.g. chunk regen).
    pub fn clear_creature_footprint(&self, center_subtile: IVec2, radius_subtiles: i32) {
        if radius_subtiles < 0 {
            return;
        }
        let shadow = baked_circle_shadow(radius_subtiles);
        let clear = FLAG_BLOCKED | FLAG_CREATURE;
        let sc = SUBTILE_COUNT as i32;
        for map in [self.inner.read_map(), self.inner.write_map()] {
            for offset in shadow.offsets {
                let target = center_subtile + *offset;
                let tile_x = target.x.div_euclid(sc);
                let tile_y = target.y.div_euclid(sc);
                let local_x = target.x.rem_euclid(sc) as usize;
                let local_y = target.y.rem_euclid(sc) as usize;
                map.update(tile_x, tile_y, |tile| {
                    tile.cells[local_y * SUBTILE_COUNT + local_x] &= !clear;
                });
            }
        }
    }

    /// Canonical actor-movement entry point: checks the candidate circular
    /// footprint against the static subtile cache **and** the dynamic **read**
    /// buffer, then stamps the new footprint into the **write** buffer.
    ///
    /// `actor_blocked` is a bitmask of [`FLAG_*`](FLAG_BLOCKED) values that
    /// this actor considers impassable. Evaluated against both static geometry
    /// flags (from `static_cache`) and dynamic creature flags (from the read
    /// buffer). Examples:
    /// - Ground walker: `FLAG_BLOCKED | FLAG_VOID`
    /// - Flyer: `FLAG_BLOCKED` (crosses void freely)
    ///
    /// Self-overlap (the actor's own previous footprint) is always bypassed —
    /// the actor never blocks itself regardless of `actor_blocked`.
    ///
    /// Allocation-free hot path: previous footprint is described compactly as
    /// `Option<(center_subtile, radius_subtiles)>` and tested via a cached
    /// [`CircleShadow`] bitmap. No per-frame `Vec`/`HashSet` is allocated.
    ///
    /// On failure the **previous** circle is re-stamped so occupancy persists
    /// into next frame even though this actor didn't move.
    pub fn try_update_footprint(
        &self,
        next_center_subtile: IVec2,
        radius_subtiles: i32,
        previous: Option<(IVec2, i32)>,
        actor_blocked: u64,
        static_cache: &Hypermap<SubtilePassability>,
    ) -> Result<(), TryUpdateFootprintError> {
        let write_map = self.inner.write_map();
        match self.probe_footprint(
            next_center_subtile,
            radius_subtiles,
            previous,
            actor_blocked,
            static_cache,
        ) {
            Ok(()) => {
                let new_shadow = baked_circle_shadow(radius_subtiles);
                write_circle(write_map, next_center_subtile, new_shadow);
                Ok(())
            }
            Err(e) => {
                // Persist previous occupancy into write buffer so the actor
                // still appears at its last accepted footprint next frame.
                if let Some((prev_center, prev_r)) = previous {
                    if prev_r >= 0 {
                        let prev_shadow = baked_circle_shadow(prev_r);
                        write_circle(write_map, prev_center, prev_shadow);
                    }
                }
                Err(e)
            }
        }
    }

    /// Non-writing variant of [`try_update_footprint`]: runs the same static +
    /// dynamic collision checks against the candidate footprint but never
    /// touches the write buffer.
    ///
    /// Used by callers that need to test multiple candidate placements per
    /// frame (e.g. axis-decomposed slide collision) and commit at most once.
    /// Calling [`try_update_footprint`] multiple times in the same frame for
    /// the same actor leaves stale stamps in the write buffer that would later
    /// flush into the read buffer and falsely block the actor on the next
    /// frame — probe + a single final commit avoids that.
    pub fn probe_footprint(
        &self,
        next_center_subtile: IVec2,
        radius_subtiles: i32,
        previous: Option<(IVec2, i32)>,
        actor_blocked: u64,
        static_cache: &Hypermap<SubtilePassability>,
    ) -> Result<(), TryUpdateFootprintError> {
        self.probe_footprint_inner(
            next_center_subtile,
            radius_subtiles,
            previous,
            actor_blocked,
            static_cache,
            None,
        )
    }

    /// Shared body of [`probe_footprint`] and [`try_claim_reentry_footprint`].
    ///
    /// `pending_dynamic`, when `Some`, is a second dynamic map (the **write**
    /// buffer) whose creature occupancy also blocks the candidate. The read
    /// buffer is the immutable per-frame snapshot; the write buffer additionally
    /// holds *this* frame's not-yet-flushed footprints. Checking it lets
    /// off-screen re-entrants placed sequentially after the parallel movement
    /// pass avoid both on-screen actors' new footprints and earlier re-entrants'
    /// just-claimed cells — neither of which the read buffer shows yet.
    fn probe_footprint_inner(
        &self,
        next_center_subtile: IVec2,
        radius_subtiles: i32,
        previous: Option<(IVec2, i32)>,
        actor_blocked: u64,
        static_cache: &Hypermap<SubtilePassability>,
        pending_dynamic: Option<&Hypermap<SubtilePassability>>,
    ) -> Result<(), TryUpdateFootprintError> {
        if radius_subtiles < 0 {
            return Err(TryUpdateFootprintError::InvalidRadius(radius_subtiles));
        }
        if let Some((_, prev_r)) = previous {
            if prev_r < 0 {
                return Err(TryUpdateFootprintError::InvalidRadius(prev_r));
            }
        }

        let new_shadow = baked_circle_shadow(radius_subtiles);
        let previous_info = previous.map(|(c, r)| (c, baked_circle_shadow(r)));

        // Chunk-local access: a compact footprint's subtiles almost always
        // share a single hypermap chunk, so each cursor resolves the chunk
        // (global table lock + `Arc` clone) at most once per distinct chunk and
        // reads each per-tile `SubtilePassability` by reference — no per-subtile
        // table lock, `Arc` clone, or 200-byte tile copy.
        let mut static_cursor = SubtileReadCursor::new(static_cache);
        let mut dynamic_cursor = SubtileReadCursor::new(self.inner.read_map());
        let mut pending_cursor = pending_dynamic.map(SubtileReadCursor::new);

        for offset in new_shadow.offsets {
            let target = next_center_subtile + *offset;

            let is_self_overlap = match previous_info {
                Some((prev_center, prev_shadow)) => {
                    prev_shadow.contains_offset(target - prev_center)
                }
                None => false,
            };
            if is_self_overlap {
                continue;
            }

            let static_flags = static_cursor.flags(target);
            if static_flags & actor_blocked != 0 {
                return Err(TryUpdateFootprintError::BlockedByStatic {
                    world_subtile: target,
                });
            }

            let dynamic_flags = dynamic_cursor.flags(target);
            if dynamic_flags & actor_blocked != 0 {
                return Err(TryUpdateFootprintError::BlockedByOccupancy {
                    world_subtile: target,
                });
            }

            if let Some(pending) = pending_cursor.as_mut() {
                if pending.flags(target) & actor_blocked != 0 {
                    return Err(TryUpdateFootprintError::BlockedByOccupancy {
                        world_subtile: target,
                    });
                }
            }
        }
        Ok(())
    }

    /// Stamps a **known-passable** circular footprint into the write buffer
    /// without any collision check.
    ///
    /// The caller must have already validated the placement this frame (e.g. via
    /// [`probe_footprint`]). Used to avoid re-probing a footprint a prior probe
    /// already proved clear — [`try_update_footprint`] always re-probes, which is
    /// wasted work when the placement is the same one just probed.
    pub fn commit_footprint(&self, center_subtile: IVec2, radius_subtiles: i32) {
        if radius_subtiles < 0 {
            return;
        }
        let shadow = baked_circle_shadow(radius_subtiles);
        write_circle(self.inner.write_map(), center_subtile, shadow);
    }

    /// Places a re-entering off-screen actor: probes the candidate footprint
    /// against static geometry, the dynamic **read** buffer, **and** the dynamic
    /// **write** buffer (this frame's pending occupancy), then stamps it into the
    /// write buffer on success.
    ///
    /// Unlike [`try_update_footprint`], the write-buffer check makes placement
    /// safe for several actors re-entering on the **same** frame: called after
    /// the parallel movement pass and **sequentially** (in a deterministic
    /// order), each successful claim is visible — via the write buffer — to the
    /// next, so two re-entrants can never be packed onto the same cell. There is
    /// no `previous`: an off-screen actor holds no stamped footprint to exempt.
    pub fn try_claim_reentry_footprint(
        &self,
        center_subtile: IVec2,
        radius_subtiles: i32,
        actor_blocked: u64,
        static_cache: &Hypermap<SubtilePassability>,
    ) -> Result<(), TryUpdateFootprintError> {
        self.probe_footprint_inner(
            center_subtile,
            radius_subtiles,
            None,
            actor_blocked,
            static_cache,
            Some(self.inner.write_map()),
        )?;
        self.commit_footprint(center_subtile, radius_subtiles);
        Ok(())
    }

    /// Stamps an arbitrary list of world-subtiles as creature-blocked in the
    /// write buffer (`FLAG_BLOCKED | FLAG_CREATURE`). Low-level escape hatch;
    /// for circular actor occupancy prefer [`try_update_footprint`].
    pub fn write_footprint(&self, footprint: &[IVec2]) {
        let subtile_map = SubtilePassabilityMap::new(self);
        for sub in footprint {
            subtile_map.or_flags_xy(0, 0, sub.x, sub.y, FLAG_BLOCKED | FLAG_CREATURE);
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// ORs `FLAG_BLOCKED | FLAG_CREATURE` for every subtile in `shadow` centered at
/// `center` into the given write-side map. Used to stamp actor footprints.
/// Chunk-local: the cursor resolves each chunk at most once for the whole circle.
#[inline]
fn write_circle(map: &Hypermap<SubtilePassability>, center: IVec2, shadow: &CircleShadow) {
    let mut cursor = SubtileWriteCursor::new(map);
    for offset in shadow.offsets {
        cursor.or_flags(center + *offset, FLAG_BLOCKED | FLAG_CREATURE);
    }
}

/// Resolves a world-subtile into `(chunk_coord, local_tile, local_subtile_row,
/// local_subtile_col)` for a `Hypermap<SubtilePassability>` (one tile = one
/// [`SubtilePassability`]; `SUBTILE_COUNT²` sub-cells per tile).
#[inline]
fn subtile_addr(
    world_subtile: IVec2,
) -> (ChunkCoord, crate::map::hypermap::LocalCoord, usize, usize) {
    let sc = SUBTILE_COUNT as i32;
    let tile_x = world_subtile.x.div_euclid(sc);
    let tile_y = world_subtile.y.div_euclid(sc);
    let local_sx = world_subtile.x.rem_euclid(sc) as usize;
    let local_sy = world_subtile.y.rem_euclid(sc) as usize;
    let (coord, local_tile) = world_to_chunk_local(tile_x, tile_y);
    (coord, local_tile, local_sy, local_sx)
}

/// Reads subtile flags from a `Hypermap<SubtilePassability>` while caching the
/// last-touched chunk handle. Since a footprint's subtiles nearly always share
/// one chunk, the expensive global chunk-table lookup + `Arc` clone is paid at
/// most once per distinct chunk; per-subtile cost is one uncontended per-chunk
/// read lock and a direct array index (no 200-byte tile clone).
struct SubtileReadCursor<'a> {
    map: &'a Hypermap<SubtilePassability>,
    /// `(coord, handle)`; the inner `Option` caches a *missing* chunk so absent
    /// regions don't re-hit the global table lock on every subtile.
    cached: Option<(ChunkCoord, Option<HypermapChunkHandle<SubtilePassability>>)>,
}

impl<'a> SubtileReadCursor<'a> {
    #[inline]
    fn new(map: &'a Hypermap<SubtilePassability>) -> Self {
        Self { map, cached: None }
    }

    #[inline]
    fn handle_for(&mut self, coord: ChunkCoord) -> Option<&HypermapChunkHandle<SubtilePassability>> {
        let hit = matches!(&self.cached, Some((c, _)) if *c == coord);
        if !hit {
            let handle = self.map.get_chunk(coord);
            self.cached = Some((coord, handle));
        }
        self.cached.as_ref().and_then(|(_, h)| h.as_ref())
    }

    #[inline]
    fn flags(&mut self, world_subtile: IVec2) -> u64 {
        let (coord, local_tile, row, col) = subtile_addr(world_subtile);
        match self.handle_for(coord) {
            // Missing chunk reads as the default (EMPTY) tile → no flags.
            None => 0,
            Some(handle) => {
                let guard = handle.read().expect("chunk lock poisoned");
                guard.get_local(local_tile).flags_at(row, col)
            }
        }
    }
}

/// Write-side counterpart to [`SubtileReadCursor`]: ORs flags into a
/// `Hypermap<SubtilePassability>`, caching the chunk handle (created lazily) so
/// a footprint stamps through a single chunk resolution. The per-chunk write
/// lock is taken per subtile (fine-grained), so concurrent actors stamping
/// other subtiles of the same chunk are not blocked for the whole footprint.
struct SubtileWriteCursor<'a> {
    map: &'a Hypermap<SubtilePassability>,
    cached: Option<(ChunkCoord, HypermapChunkHandle<SubtilePassability>)>,
}

impl<'a> SubtileWriteCursor<'a> {
    #[inline]
    fn new(map: &'a Hypermap<SubtilePassability>) -> Self {
        Self { map, cached: None }
    }

    #[inline]
    fn handle_for(&mut self, coord: ChunkCoord) -> &HypermapChunkHandle<SubtilePassability> {
        let hit = matches!(&self.cached, Some((c, _)) if *c == coord);
        if !hit {
            let handle = self.map.get_or_create_chunk(coord);
            self.cached = Some((coord, handle));
        }
        &self.cached.as_ref().expect("just populated").1
    }

    #[inline]
    fn or_flags(&mut self, world_subtile: IVec2, flags: u64) {
        let (coord, local_tile, row, col) = subtile_addr(world_subtile);
        let handle = self.handle_for(coord);
        let mut guard = handle.write().expect("chunk lock poisoned");
        guard.get_local_mut(local_tile).or_flags(row, col, flags);
    }
}

// ---------------------------------------------------------------------------
// SubtilePassabilityMap view
// ---------------------------------------------------------------------------

/// Subtile-level view over a [`DoubleBufferedHypermap<SubtilePassability>`].
///
/// Addresses individual sub-cells using a **(tile, shift)** scheme: the caller
/// supplies a reference tile coordinate and an arbitrary signed subtile offset.
/// The offset is **not** clamped — it freely overflows into neighboring tiles.
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
/// # Example
///
/// ```ignore
/// let view = SubtilePassabilityMap::new(&dynamic_passability_map);
///
/// // Read flags 2 subtiles right and 1 subtile up from tile (10, 20).
/// let flags = view.flags_xy(10, 20, 4, -1);
/// let blocked = flags & FLAG_BLOCKED != 0;
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

    /// Read flags for a single subtile from the **read** buffer.
    ///
    /// `tile_index` is the world tile `(x, y)`. `shift` is a signed subtile
    /// offset `(dx, dy)` relative to that tile's `(0, 0)` sub-cell.
    #[inline]
    pub fn flags(&self, tile_index: (i32, i32), shift: (i32, i32)) -> u64 {
        self.flags_xy(tile_index.0, tile_index.1, shift.0, shift.1)
    }

    /// Scalar-argument form of [`flags`](Self::flags).
    #[inline]
    pub fn flags_xy(&self, tile_x: i32, tile_y: i32, shift_x: i32, shift_y: i32) -> u64 {
        let (resolved_tile_x, local_x) = resolve_subtile(tile_x, shift_x);
        let (resolved_tile_y, local_y) = resolve_subtile(tile_y, shift_y);
        let cell = self.map.get(resolved_tile_x, resolved_tile_y);
        cell.flags_at(local_y, local_x)
    }

    /// OR `flags` into the **write** buffer for a single subtile.
    ///
    /// Same addressing rules as [`flags`](Self::flags).
    #[inline]
    pub fn or_flags(&self, tile_index: (i32, i32), shift: (i32, i32), flags: u64) {
        self.or_flags_xy(tile_index.0, tile_index.1, shift.0, shift.1, flags);
    }

    /// Scalar-argument form of [`or_flags`](Self::or_flags).
    #[inline]
    pub fn or_flags_xy(
        &self,
        tile_x: i32,
        tile_y: i32,
        shift_x: i32,
        shift_y: i32,
        flags: u64,
    ) {
        let (resolved_tile_x, local_x) = resolve_subtile(tile_x, shift_x);
        let (resolved_tile_y, local_y) = resolve_subtile(tile_y, shift_y);
        self.map.update(resolved_tile_x, resolved_tile_y, |cell| {
            cell.or_flags(local_y, local_x, flags);
        });
    }
}

// ---------------------------------------------------------------------------
// Subtile coordinate resolution
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// CircleShadow cache
// ---------------------------------------------------------------------------

/// Baked, immutable representation of a filled integer circle.
///
/// Holds both a flat offset list (`offsets`) for iteration and a dense bitmap
/// (`bitmap`) for `O(1)` membership checks. One instance per radius is cached
/// globally and leaked, keeping `&'static` references valid for the process
/// lifetime.
pub struct CircleShadow {
    /// Filled-circle offsets `(dx, dy)` with `dx²+dy² ≤ r²`.
    /// Iteration order is row-major (`y` outer, `x` inner).
    pub offsets: &'static [IVec2],
    radius: i32,
    /// Dense `(2r+1)²` membership bitmap, row-major.
    bitmap: &'static [bool],
}

impl CircleShadow {
    /// `true` iff `offset` is inside this circle (i.e. is one of `offsets`).
    #[inline]
    pub fn contains_offset(&self, offset: IVec2) -> bool {
        let r = self.radius;
        if offset.x < -r || offset.x > r || offset.y < -r || offset.y > r {
            return false;
        }
        let stride = (2 * r + 1) as usize;
        let idx = ((offset.y + r) as usize) * stride + (offset.x + r) as usize;
        self.bitmap[idx]
    }
}

/// Largest radius served by the lock-free fast-path table. Actor radii are a
/// handful of subtiles, so every real lookup lands here; only pathological
/// radii fall back to the slow, locked path below.
const SHADOW_FAST_CACHE_LEN: usize = 32;

/// Returns the cached [`CircleShadow`] for a given radius. Built once per
/// distinct radius, then leaked to `&'static`.
///
/// The steady-state hot path is **lock-free**: each radius `< SHADOW_FAST_CACHE_LEN`
/// has its own [`OnceLock`] slot, so a warm lookup is a single atomic load with
/// no contention. Multiple actors (including across threads) can bake/read
/// shadows concurrently — critical for parallelizing `process_actors`, which
/// otherwise serialized on one global `Mutex` ~7× per actor per frame. Only
/// oversized radii (rare) take a lock.
fn baked_circle_shadow(radius_subtiles: i32) -> &'static CircleShadow {
    let r = radius_subtiles.max(0);

    static FAST: [OnceLock<&'static CircleShadow>; SHADOW_FAST_CACHE_LEN] =
        [const { OnceLock::new() }; SHADOW_FAST_CACHE_LEN];

    if (r as usize) < SHADOW_FAST_CACHE_LEN {
        return FAST[r as usize].get_or_init(|| build_circle_shadow(r));
    }

    // Rare oversized-radius path: a lock is acceptable because it is virtually
    // never hit and never on the per-frame collision hot path.
    static SLOW: OnceLock<Mutex<HashMap<i32, &'static CircleShadow>>> = OnceLock::new();
    let cache = SLOW.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().expect("passability shadow cache lock poisoned");
    *guard.entry(r).or_insert_with(|| build_circle_shadow(r))
}

/// Builds a filled-circle [`CircleShadow`] of radius `r` (`r >= 0`) and leaks it
/// to `&'static`. Called at most once per distinct radius.
fn build_circle_shadow(r: i32) -> &'static CircleShadow {
    let rr = r * r;
    let stride = (2 * r + 1) as usize;
    let bitmap_len = stride * stride;
    let mut bitmap_vec = vec![false; bitmap_len];
    let mut offsets = Vec::new();
    for y in -r..=r {
        for x in -r..=r {
            if x * x + y * y <= rr {
                offsets.push(IVec2::new(x, y));
                let idx = ((y + r) as usize) * stride + (x + r) as usize;
                bitmap_vec[idx] = true;
            }
        }
    }

    let leaked_offsets: &'static [IVec2] = Box::leak(offsets.into_boxed_slice());
    let leaked_bitmap: &'static [bool] = Box::leak(bitmap_vec.into_boxed_slice());
    Box::leak(Box::new(CircleShadow {
        offsets: leaked_offsets,
        radius: r,
        bitmap: leaked_bitmap,
    }))
}

// ---------------------------------------------------------------------------
// Static geometry helpers — subtile-accurate flags from CellType
// ---------------------------------------------------------------------------

/// Returns the passability flags for the subtile at `(local_x, local_y)`
/// within a tile of the given [`CellType`].
///
/// - `Road` / `Charger` → `0` (always passable)
/// - `Void` → [`FLAG_VOID`] for every subtile
/// - `Wall(mask)` → [`FLAG_BLOCKED`] for the edge-strip subtile(s) matching
///   the mask; `0` elsewhere
/// - `Corner(c)` → [`FLAG_BLOCKED`] for the single corner subtile; `0` elsewhere
///
/// `local_x` and `local_y` must be in `0..SUBTILE_COUNT`.
#[inline]
pub fn cell_subtile_flags(cell: CellType, local_x: usize, local_y: usize) -> u64 {
    match cell {
        CellType::Road | CellType::Charger(_) => 0,
        CellType::Void => FLAG_VOID,
        CellType::Wall(mask) => {
            if wall_mask_blocks_subtile(mask, local_x, local_y) {
                FLAG_BLOCKED
            } else {
                0
            }
        }
        CellType::Corner(corner) => {
            if corner_blocks_subtile(corner, local_x, local_y) {
                FLAG_BLOCKED
            } else {
                0
            }
        }
    }
}

#[inline]
fn wall_mask_blocks_subtile(mask: WallMask, local_x: usize, local_y: usize) -> bool {
    let bits = mask.bits();
    let max = SUBTILE_COUNT - 1;
    ((bits & MASK_NORTH) != 0 && local_y == 0)
        || ((bits & MASK_SOUTH) != 0 && local_y == max)
        || ((bits & MASK_EAST) != 0 && local_x == max)
        || ((bits & MASK_WEST) != 0 && local_x == 0)
}

#[inline]
fn corner_blocks_subtile(corner: WallCorner, local_x: usize, local_y: usize) -> bool {
    let max = SUBTILE_COUNT - 1;
    match corner {
        WallCorner::Nw => local_x == 0 && local_y == 0,
        WallCorner::Ne => local_x == max && local_y == 0,
        WallCorner::Sw => local_x == 0 && local_y == max,
        WallCorner::Se => local_x == max && local_y == max,
    }
}


// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct PassabilityMapPlugin;

impl Plugin for PassabilityMapPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(DynamicPassabilityMap::new());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtile_default_is_all_passable() {
        let s = SubtilePassability::default();
        for r in 0..SUBTILE_COUNT {
            for c in 0..SUBTILE_COUNT {
                assert_eq!(s.flags_at(r, c), 0, "default cell must have zero flags");
            }
        }
    }

    #[test]
    fn subtile_or_flags_and_query() {
        let mut s = SubtilePassability::EMPTY;
        s.or_flags(2, 3, FLAG_BLOCKED);
        assert_eq!(s.flags_at(2, 3), FLAG_BLOCKED);
        assert_eq!(s.flags_at(0, 0), 0);

        s.or_flags(2, 3, FLAG_CREATURE);
        assert_eq!(s.flags_at(2, 3), FLAG_BLOCKED | FLAG_CREATURE);
    }

    #[test]
    fn dynamic_map_write_read_flush_cycle() {
        let map = DynamicPassabilityMap::new();
        let view = SubtilePassabilityMap::new(&map);

        view.or_flags_xy(10, 20, 0, 0, FLAG_BLOCKED);
        view.or_flags_xy(10, 20, 4, 4, FLAG_VOID);

        assert_eq!(
            map.inner().get(10, 20).flags_at(0, 0),
            0,
            "read side still default before flush"
        );

        map.flush();

        let read = map.inner().get(10, 20);
        assert_eq!(read.flags_at(0, 0), FLAG_BLOCKED);
        assert_eq!(read.flags_at(4, 4), FLAG_VOID);
        assert_eq!(read.flags_at(2, 2), 0);
    }

    #[test]
    fn dynamic_map_flush_clears_write() {
        let map = DynamicPassabilityMap::new();
        let view = SubtilePassabilityMap::new(&map);
        view.or_flags_xy(5, 5, 0, 0, FLAG_BLOCKED);
        map.flush();

        assert_eq!(map.inner().get(5, 5).flags_at(0, 0), FLAG_BLOCKED);

        map.flush();
        assert_eq!(
            map.inner().get(5, 5).flags_at(0, 0),
            0,
            "second flush with no writes must reset to clean"
        );
    }

    // --- SubtilePassabilityMap ---

    #[test]
    fn subtile_map_read_within_tile() {
        let map = DynamicPassabilityMap::new();
        let view = SubtilePassabilityMap::new(&map);
        view.or_flags_xy(10, 20, 3, 2, FLAG_BLOCKED);
        map.flush();

        assert_ne!(view.flags_xy(10, 20, 3, 2) & FLAG_BLOCKED, 0);
        assert_eq!(view.flags_xy(10, 20, 0, 0), 0);
    }

    #[test]
    fn subtile_map_positive_overflow_into_neighbor() {
        let map = DynamicPassabilityMap::new();
        let view = SubtilePassabilityMap::new(&map);
        // shift_x=6 from tile 10 → tile 11, local_x=1; shift_y=0 → local_y=0
        view.or_flags_xy(11, 20, 1, 0, FLAG_BLOCKED);
        map.flush();

        assert_ne!(view.flags_xy(10, 20, 6, 0) & FLAG_BLOCKED, 0);
    }

    #[test]
    fn subtile_map_negative_overflow_into_neighbor() {
        let map = DynamicPassabilityMap::new();
        let view = SubtilePassabilityMap::new(&map);
        // shift_x=-1 from tile 10 → tile 9, local_x=4; shift_y=-1 → tile 19, local_y=4
        view.or_flags_xy(9, 19, 4, 4, FLAG_BLOCKED);
        map.flush();

        assert_ne!(view.flags_xy(10, 20, -1, -1) & FLAG_BLOCKED, 0);
    }

    #[test]
    fn subtile_map_large_shift_crosses_multiple_tiles() {
        let map = DynamicPassabilityMap::new();
        let view = SubtilePassabilityMap::new(&map);
        // shift (12, 2) from tile (10, 20):
        //   x: global = 10*5+12 = 62 → tile 12, local_x 2
        //   y: global = 20*5+2  = 102 → tile 20, local_y 2
        view.or_flags_xy(12, 20, 2, 2, FLAG_VOID);
        map.flush();

        assert_ne!(view.flags_xy(10, 20, 12, 2) & FLAG_VOID, 0);
    }

    #[test]
    fn subtile_map_or_flags_via_shifted_address() {
        let map = DynamicPassabilityMap::new();
        let view = SubtilePassabilityMap::new(&map);

        // shift_x=7 from tile 5 → tile 6, local_x=2; shift_y=-2 from tile 5 → tile 4, local_y=3
        view.or_flags_xy(5, 5, 7, -2, FLAG_BLOCKED | FLAG_CREATURE);
        map.flush();

        let cell = map.inner().get(6, 4);
        assert_eq!(cell.flags_at(3, 2), FLAG_BLOCKED | FLAG_CREATURE);
    }

    #[test]
    fn resolve_subtile_basic() {
        assert_eq!(resolve_subtile(0, 0), (0, 0));
        assert_eq!(resolve_subtile(0, 4), (0, 4));
        assert_eq!(resolve_subtile(0, 5), (1, 0));
        assert_eq!(resolve_subtile(0, -1), (-1, 4));
        assert_eq!(resolve_subtile(3, -6), (1, 4));
    }

    fn empty_cache() -> Hypermap<SubtilePassability> {
        Hypermap::new(SubtilePassability::EMPTY)
    }

    #[test]
    fn try_update_footprint_writes_circle_into_buffer() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_cache();
        let center = IVec2::new(20, 20);
        map.try_update_footprint(center, 1, None, FLAG_BLOCKED | FLAG_CREATURE, &sc)
            .expect("footprint should be writable");
        map.flush();
        let view = SubtilePassabilityMap::new(&map);
        let shadow = baked_circle_shadow(1);

        for offset in shadow.offsets {
            let sub = center + *offset;
            assert_ne!(
                view.flags_xy(0, 0, sub.x, sub.y) & FLAG_BLOCKED,
                0,
                "every subtile in the stamped circle must be blocked in read buffer"
            );
        }
    }

    #[test]
    fn try_update_footprint_ignores_previous_self_overlap() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_cache();
        let center0 = IVec2::new(40, 40);
        map.try_update_footprint(center0, 2, None, FLAG_BLOCKED | FLAG_CREATURE, &sc)
            .expect("initial footprint");
        map.flush();

        let moved = map.try_update_footprint(
            IVec2::new(41, 40),
            2,
            Some((center0, 2)),
            FLAG_BLOCKED | FLAG_CREATURE,
            &sc,
        );
        assert!(moved.is_ok(), "self-overlap should not block movement");
    }

    #[test]
    fn try_update_footprint_blocks_on_foreign_occupancy() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_cache();
        map.write_footprint(&[IVec2::new(50, 50)]);
        map.flush();

        let err = map
            .try_update_footprint(IVec2::new(50, 50), 0, None, FLAG_BLOCKED | FLAG_CREATURE, &sc)
            .expect_err("blocked cell should reject footprint");
        assert!(
            matches!(err, TryUpdateFootprintError::BlockedByOccupancy { world_subtile }
                if world_subtile == IVec2::new(50, 50)),
            "creature-blocked cell must produce BlockedByOccupancy, got {err:?}"
        );
    }

    #[test]
    fn try_update_footprint_blocks_on_static_obstacle() {
        let map = DynamicPassabilityMap::new();
        // Put a wall flag into the static cache.
        let sc = empty_cache();
        let mut tile = SubtilePassability::EMPTY;
        tile.or_flags(0, 0, FLAG_BLOCKED); // subtile (0,0) of tile (12,12)
        sc.set(12, 12, tile);

        let err = map
            .try_update_footprint(IVec2::new(60, 60), 0, None, FLAG_BLOCKED, &sc)
            .expect_err("static wall must reject footprint");
        assert!(
            matches!(err, TryUpdateFootprintError::BlockedByStatic { world_subtile }
                if world_subtile == IVec2::new(60, 60)),
            "wall-blocked cell must produce BlockedByStatic, got {err:?}"
        );
    }

    #[test]
    fn try_update_footprint_void_blocks_walker_not_flyer() {
        let map = DynamicPassabilityMap::new();
        // Put void into the static cache at subtile (70,70) → tile (14,14), local (0,0).
        let sc = empty_cache();
        let mut tile = SubtilePassability::EMPTY;
        tile.or_flags(0, 0, FLAG_VOID);
        sc.set(14, 14, tile);

        // Ground walker (blocked by FLAG_VOID) should fail.
        let err = map
            .try_update_footprint(IVec2::new(70, 70), 0, None, FLAG_BLOCKED | FLAG_VOID, &sc)
            .expect_err("walker must be blocked by void");
        assert!(matches!(err, TryUpdateFootprintError::BlockedByStatic { .. }));

        // Flyer (only blocked by FLAG_BLOCKED) should pass.
        let ok = map.try_update_footprint(IVec2::new(70, 70), 0, None, FLAG_BLOCKED, &sc);
        assert!(ok.is_ok(), "flyer must cross void freely");
    }

    #[test]
    fn try_update_footprint_with_static_ignores_previous_self_overlap() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_cache();
        let center0 = IVec2::new(70, 70);
        map.try_update_footprint(center0, 1, None, FLAG_BLOCKED | FLAG_CREATURE, &sc)
            .expect("initial footprint");
        map.flush();

        // Actor blocked by everything — movement must still succeed because
        // every candidate cell is part of the previous circle (self-overlap).
        let moved =
            map.try_update_footprint(center0, 1, Some((center0, 1)), FLAG_BLOCKED | FLAG_VOID, &sc);
        assert!(moved.is_ok(), "self-overlap subtiles must bypass all checks");
    }

    #[test]
    fn try_update_footprint_passes_with_empty_map() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_cache();
        let result = map.try_update_footprint(IVec2::new(80, 80), 2, None, FLAG_BLOCKED | FLAG_VOID, &sc);
        assert!(result.is_ok());
    }

    #[test]
    fn try_update_footprint_failure_restamps_previous_circle() {
        let map = DynamicPassabilityMap::new();
        let sc = empty_cache();
        let center0 = IVec2::new(90, 90);
        map.try_update_footprint(center0, 1, None, FLAG_BLOCKED | FLAG_CREATURE, &sc)
            .expect("initial circle");
        map.flush();

        // Block a cell in the candidate circle but NOT in the previous one.
        let blocker = center0 + IVec2::new(2, 0);
        map.write_footprint(&[blocker]);
        map.flush();

        let err = map
            .try_update_footprint(
                center0 + IVec2::new(1, 0),
                1,
                Some((center0, 1)),
                FLAG_BLOCKED | FLAG_CREATURE,
                &sc,
            )
            .expect_err("must be blocked by foreign occupancy");
        assert!(matches!(err, TryUpdateFootprintError::BlockedByOccupancy { .. }));

        // After flush the old circle must still be present (re-stamped on failure).
        map.flush();
        let view = SubtilePassabilityMap::new(&map);
        for offset in baked_circle_shadow(1).offsets {
            let sub = center0 + *offset;
            assert_ne!(
                view.flags_xy(0, 0, sub.x, sub.y) & FLAG_BLOCKED,
                0,
                "previous footprint must be preserved on failed movement"
            );
        }
    }

    #[test]
    fn circle_shadow_contains_offset_matches_offsets_list() {
        for r in 0..=5 {
            let shadow = baked_circle_shadow(r);
            for offset in shadow.offsets {
                assert!(shadow.contains_offset(*offset), "offset {offset:?} missing for r={r}");
            }
            let rr = (r + 2).max(1);
            for y in -rr..=rr {
                for x in -rr..=rr {
                    let offset = IVec2::new(x, y);
                    let in_list = shadow.offsets.iter().any(|o| *o == offset);
                    assert_eq!(
                        shadow.contains_offset(offset),
                        in_list,
                        "contains_offset disagrees with offsets list at {offset:?} for r={r}"
                    );
                }
            }
        }
    }

    #[test]
    fn cell_subtile_flags_road_is_zero() {
        for ly in 0..SUBTILE_COUNT {
            for lx in 0..SUBTILE_COUNT {
                assert_eq!(cell_subtile_flags(CellType::Road, lx, ly), 0);
            }
        }
    }

    #[test]
    fn cell_subtile_flags_void_all_flag_void() {
        for ly in 0..SUBTILE_COUNT {
            for lx in 0..SUBTILE_COUNT {
                assert_eq!(cell_subtile_flags(CellType::Void, lx, ly), FLAG_VOID);
            }
        }
    }

    #[test]
    fn cell_subtile_flags_wall_north_blocks_only_top_row() {
        let mask = WallMask::from_bits(MASK_NORTH).unwrap();
        for lx in 0..SUBTILE_COUNT {
            assert_eq!(
                cell_subtile_flags(CellType::Wall(mask), lx, 0),
                FLAG_BLOCKED,
                "top row must be blocked"
            );
            for ly in 1..SUBTILE_COUNT {
                assert_eq!(
                    cell_subtile_flags(CellType::Wall(mask), lx, ly),
                    0,
                    "other rows must be clear"
                );
            }
        }
    }

    #[test]
    fn cell_subtile_flags_corner_blocks_one_subtile() {
        let max = SUBTILE_COUNT - 1;
        assert_eq!(
            cell_subtile_flags(CellType::Corner(WallCorner::Se), max, max),
            FLAG_BLOCKED
        );
        assert_eq!(cell_subtile_flags(CellType::Corner(WallCorner::Se), 0, 0), 0);
        assert_eq!(
            cell_subtile_flags(CellType::Corner(WallCorner::Nw), 0, 0),
            FLAG_BLOCKED
        );
        assert_eq!(cell_subtile_flags(CellType::Corner(WallCorner::Nw), 1, 0), 0);
    }
}

//! Single-pass sequential movement pipeline shared by every actor.
//!
//! Replaces the old two-system "propose then arbitrate" split. Because the
//! propose step is already sequential (single-threaded — see the rationale in
//! `docs/movement.md`), collision detection is merged into the same pass: every
//! on-screen actor proposes its step and is placed against a within-frame owner
//! grid in one deterministic, entity-sorted sweep.
//!
//! The pass works in four stages inside [`process_actor_moves`]:
//!
//! 1. **Think + propose** (any order) — each actor runs `think_low_level` +
//!    `prepare_movement` + [`Actor::propose_move`], which validates the step
//!    against **static** geometry only and records its proposed footprint
//!    compactly in [`ActorShadow`] (`proposed_center` / `origin` + radius — never
//!    explicit cell lists). Off-screen actors `advance_unchecked` (no collision);
//!    re-entrants are queued.
//! 2. **Resolve** (sequential, entity-sorted) — every participant's *current*
//!    (last-accepted) footprint is pre-stamped into the [`OwnerGrid`], then each
//!    actor in turn releases its own origin and tries to claim its proposed
//!    footprint. If any cell is owned by another actor it **holds at its origin**
//!    and is marked collided. Because origins are pre-stamped, a mover can never
//!    steal a cell another actor still occupies, and every actor always has its
//!    own origin to fall back to — so **no teleport/squeeze is ever needed** for
//!    a jam (cf. the old back-off cascade + squeeze pool).
//! 3. **Apply + commit** — placed actors advance; collided actors hold and
//!    surface a `BlockedByOccupancy` error for the brain to react to next frame.
//!    Every final footprint is stamped into the [`DynamicPassabilityMap`] write
//!    buffer so the brain's avoidance views and the async pathfinder keep reading
//!    the same occupancy after the next `flush`.
//! 4. **Re-entry placement** — actors that just re-entered a rendered chunk from
//!    off-screen travel are teleported to a free cell
//!    ([`super::resolve_offscreen_collision`]). This is the only remaining
//!    non-local move, and it exists solely because off-screen actors travel
//!    without collision and may re-enter sitting inside static geometry.

use bevy::prelude::*;
use bevy::platform::collections::HashMap;

use crate::map::hypermap_world::HypermapRuntime;
use crate::map::passability::{baked_circle_shadow, DynamicPassabilityMap};

use super::{
    resolve_offscreen_collision, Actor, ActorMovementError, ActorObject, OffScreenActor,
};

// ---------------------------------------------------------------------------
// Per-actor shadow + transient proposal state
// ---------------------------------------------------------------------------

/// Per-actor footprint shadow plus the transient state the movement pipeline
/// refreshes every frame. Lives on [`ActorState`](super::ActorState); defaulted
/// on construction and **not** serialized.
///
/// Footprints are always baked circles, so they are stored compactly as a
/// center (`origin` for the fall-back target, `proposed_center` for this frame's
/// candidate) plus the actor's `radius_subtiles` — never as explicit cell lists
/// (OPTIMIZATION rule 4).
#[derive(Debug, Clone, Default)]
pub struct ActorShadow {
    /// Grid center of the last accepted footprint — the cell an actor holds at
    /// when its proposed step is blocked by another actor.
    pub origin: IVec2,
    /// Float `center` one frame ago — momentum seed for a re-entry teleport.
    pub world_previous: Vec2,
    /// Proposed grid center this frame (stage 1 output, after the static slide).
    pub proposed_center: IVec2,
    /// Float `center` delta applied if the proposal is accepted (after slide).
    pub proposed_delta: Vec2,
    /// Rotation delta to apply this frame.
    pub proposed_rotation: f32,
    /// `true` once a valid on-screen proposal was produced this frame, so the
    /// actor participates in occupancy resolution.
    pub participates: bool,
    /// First statically-blocked subtile found during the proposal, if any.
    pub static_block: Option<IVec2>,
    /// Set by the re-entry pass when the actor was teleported to a free cell
    /// after off-screen travel, so a planner (e.g. the BlackBot brain) can drop
    /// its stale plan and re-route from the new position.
    pub teleported: bool,
}

// ---------------------------------------------------------------------------
// Owner grid
// ---------------------------------------------------------------------------

/// Flat map from absolute world-subtile to the actor slot index that owns it
/// during one resolution pass. Absent entries are free.
///
/// The pass is entirely sequential so no locking is needed — all the
/// `RwLock`/`Arc`/`ArcSwap` overhead of a `Hypermap`-backed grid is replaced
/// by a plain foldhash `HashMap` (rule 1: SipHash is ~3× slower per op for
/// 8-byte keys and buys nothing single-threaded). Footprints arrive compactly
/// as `(center, radius)` and are expanded through the `&'static` baked circle
/// offsets — no cell lists (rule 4). The map is cleared at the start of each
/// pass and reused across frames, so capacity stabilises after the first few.
pub struct OwnerGrid {
    map: HashMap<IVec2, u32>,
}

impl OwnerGrid {
    fn new() -> Self {
        Self { map: HashMap::default() }
    }

    /// Clears all ownership records for the next frame.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// First cell of the radius-`radius` circle at `center` owned by someone
    /// other than `self_owner`, with that owner's slot index. `None` if every
    /// cell is free or self-owned.
    pub fn first_foreign(
        &self,
        center: IVec2,
        radius: i32,
        self_owner: u32,
    ) -> Option<(IVec2, u32)> {
        for offset in baked_circle_shadow(radius).offsets {
            let cell = center + *offset;
            if let Some(&o) = self.map.get(&cell) {
                if o != self_owner {
                    return Some((cell, o));
                }
            }
        }
        None
    }

    /// Stamps `owner` over every cell of the circle.
    pub fn stamp(&mut self, center: IVec2, radius: i32, owner: u32) {
        for offset in baked_circle_shadow(radius).offsets {
            self.map.insert(center + *offset, owner);
        }
    }

    /// Clears every circle cell currently owned by `owner` (leaves foreign
    /// cells alone).
    pub fn clear_cells(&mut self, center: IVec2, radius: i32, owner: u32) {
        for offset in baked_circle_shadow(radius).offsets {
            let cell = center + *offset;
            if self.map.get(&cell) == Some(&owner) {
                self.map.remove(&cell);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure resolution core
// ---------------------------------------------------------------------------

/// One actor's footprint candidates and resolution outcome for a frame.
/// Footprints are compact `(center, radius)` circles (rule 4) — plain `Copy`
/// data, no per-record buffers.
#[derive(Default, Clone, Copy)]
pub struct MoveRecord {
    /// Proposed footprint center this frame.
    pub current: IVec2,
    /// Previous (last accepted) footprint center — the hold-in-place target.
    pub previous: IVec2,
    /// Circle radius (subtiles) shared by both footprints.
    pub radius: i32,
    /// `true` if the actor could not take its proposal and held at `previous`.
    pub collided: bool,
    /// First conflicting subtile, for the actor's `last_movement_error`.
    pub conflict_cell: Option<IVec2>,
}

impl MoveRecord {
    fn reset(&mut self) {
        self.collided = false;
        self.conflict_cell = None;
    }
}

/// Runs the deterministic occupancy resolution over `records` (already in the
/// desired, e.g. entity-sorted, order). Clears and rebuilds `owners`.
///
/// **Precondition:** the `previous` footprints are pairwise disjoint — they are
/// last frame's accepted positions, which the previous pass guaranteed are
/// non-overlapping (and re-entry placement keeps re-entrants disjoint too). This
/// lets every actor's origin be pre-stamped as a guaranteed personal fall-back,
/// which is why the pass needs no back-off cascade and no squeeze/teleport.
pub fn arbitrate(records: &mut [MoveRecord], owners: &mut OwnerGrid) {
    owners.clear();
    for r in records.iter_mut() {
        r.reset();
    }

    // Pre-stamp every actor's currently-occupied footprint. A mover therefore
    // sees a not-yet-processed occupant's cell as taken and cannot steal it;
    // the occupant keeps its spot regardless of entity order.
    for i in 0..records.len() {
        owners.stamp(records[i].previous, records[i].radius, i as u32);
    }

    for i in 0..records.len() {
        let r = records[i];
        // Release our own origin so a footprint-overlapping small step does not
        // conflict with ourselves.
        owners.clear_cells(r.previous, r.radius, i as u32);
        match owners.first_foreign(r.current, r.radius, i as u32) {
            None => {
                // Proposed footprint is clear — advance.
                owners.stamp(r.current, r.radius, i as u32);
            }
            Some((cell, _)) => {
                // Occupied by another actor — hold at the (always-free) origin.
                owners.stamp(r.previous, r.radius, i as u32);
                records[i].collided = true;
                records[i].conflict_cell = Some(cell);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OccupancyArbiter resource (reused scratch)
// ---------------------------------------------------------------------------

/// Reused per-frame scratch for the sequential resolution pass. Holds the owner
/// grid, the move records, the entity-sorted participant list, and the re-entry
/// work list so the steady state allocates nothing (rule 4).
#[derive(Resource)]
pub struct OccupancyArbiter {
    pub owners: OwnerGrid,
    pub records: Vec<MoveRecord>,
    pub entities: Vec<Entity>,
    pub reentrants: Vec<Entity>,
}

impl Default for OccupancyArbiter {
    fn default() -> Self {
        Self {
            owners: OwnerGrid::new(),
            records: Vec::new(),
            entities: Vec::new(),
            reentrants: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Merged movement system
// ---------------------------------------------------------------------------

/// Single sequential movement pass: think, propose against static geometry,
/// resolve actor-vs-actor occupancy over the owner grid, apply each outcome,
/// stamp accepted footprints into the dynamic write buffer, and teleport
/// off-screen re-entrants.
///
/// **Deliberately sequential.** The per-bot work here is tiny (a static slide
/// probe — single-digit microseconds for the whole crowd). Running it on Bevy's
/// global `ComputeTaskPool` (`par_iter_mut`) was a net loss: that pool's `scope`
/// ticks the *global* executor while waiting for its own batches, absorbing
/// unrelated queued compute (render batching, etc.) and, at 60 Hz fixed
/// catch-up ticks, compounding into frame-rate-coupled stalls. Sequential
/// execution removes that coupling entirely. See `docs/movement.md`.
pub(crate) fn process_actor_moves(
    mut actors: Query<(Entity, &mut ActorObject, Option<&OffScreenActor>)>,
    hypermap: Res<HypermapRuntime>,
    mut arbiter: ResMut<OccupancyArbiter>,
    dynamic: Res<DynamicPassabilityMap>,
    mut commands: Commands,
) {
    let static_cache = hypermap.static_subtile_cache.as_ref();
    let hypermap = &*hypermap;

    arbiter.reentrants.clear();
    arbiter.entities.clear();

    // Stage 1: think, prepare, classify, and propose (static-only). Each actor
    // touches only its own state, so this is order-independent.
    for (entity, mut actor_obj, off_screen) in actors.iter_mut() {
        let actor = actor_obj.inner.as_mut();
        {
            let s = actor.state_mut();
            s.last_movement_error = None;
            s.shadow.participates = false;
            s.shadow.teleported = false;
        }

        actor.think_low_level();
        actor.prepare_movement();

        let center = actor.state().center;
        let is_rendered =
            hypermap.is_world_pos_rendered(center.x.floor() as i32, center.y.floor() as i32);
        let was_off_screen = off_screen.is_some();

        if is_rendered {
            if was_off_screen {
                commands.entity(entity).remove::<OffScreenActor>();
                arbiter.reentrants.push(entity);
            } else {
                actor.propose_move(static_cache);
                arbiter.entities.push(entity);
            }
        } else {
            if !was_off_screen {
                commands.entity(entity).insert(OffScreenActor);
            }
            actor.advance_unchecked();
        }
    }

    // Stage 2: deterministic occupancy resolution over the owner grid.
    arbiter.entities.sort_unstable();
    let n = arbiter.entities.len();
    while arbiter.records.len() < n {
        arbiter.records.push(MoveRecord::default());
    }
    for k in 0..n {
        let entity = arbiter.entities[k];
        let Ok((_, actor_obj, _)) = actors.get(entity) else { continue };
        let state = actor_obj.inner.state();
        let rec = &mut arbiter.records[k];
        rec.current = state.shadow.proposed_center;
        rec.previous = state.shadow.origin;
        rec.radius = state.radius_subtiles;
    }
    {
        let arb = &mut *arbiter;
        arbitrate(&mut arb.records[..n], &mut arb.owners);
    }
    // Stage 3: apply outcomes and stamp final footprints into the write buffer.
    for k in 0..n {
        let entity = arbiter.entities[k];
        let rec = arbiter.records[k];
        if let Ok((_, mut actor_obj, _)) = actors.get_mut(entity) {
            apply_outcome(actor_obj.inner.as_mut(), &rec);
        }
        let center = if rec.collided { rec.previous } else { rec.current };
        dynamic.commit_footprint(center, rec.radius);
    }

    // Stage 4: place off-screen re-entrants on a free cell (sorted for
    // determinism). The write buffer already holds this frame's footprints, so a
    // re-entrant never lands on a placed actor.
    arbiter.reentrants.sort_unstable();
    let reentrants = std::mem::take(&mut arbiter.reentrants);
    for &entity in reentrants.iter() {
        if let Ok((_, mut actor_obj, _)) = actors.get_mut(entity) {
            let actor = actor_obj.inner.as_mut();
            resolve_offscreen_collision(actor, &dynamic, static_cache);
            actor.state_mut().shadow.teleported = true;
        }
    }
    arbiter.reentrants = reentrants;
}

/// Applies one resolution outcome to an actor: advance on success, hold + error
/// on a dynamic conflict, surface a static slide error, and always apply rotation.
fn apply_outcome(actor: &mut dyn Actor, record: &MoveRecord) {
    let static_block = actor.state().shadow.static_block;
    let proposed_center = actor.state().shadow.proposed_center;
    let proposed_delta = actor.state().shadow.proposed_delta;
    let proposed_rotation = actor.state().shadow.proposed_rotation;
    let radius = actor.state().radius_subtiles;

    let s = actor.state_mut();
    s.rotation += proposed_rotation;

    if record.collided {
        // Dynamic conflict: hold at the previous footprint, occupancy error wins.
        if let Some(cell) = record.conflict_cell {
            s.last_movement_error = Some(ActorMovementError::BlockedByOccupancy {
                world_subtile_x: cell.x,
                world_subtile_y: cell.y,
            });
        }
        return;
    }

    // Advanced: take the proposal.
    s.center += proposed_delta;
    s.last_accepted_center_subtile = Some(proposed_center);
    s.last_accepted_radius_subtiles = radius;
    if let Some(cell) = static_block {
        // Slid along a wall this frame (one axis statically blocked).
        s.last_movement_error = Some(ActorMovementError::BlockedByStatic {
            world_subtile_x: cell.x,
            world_subtile_y: cell.y,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Radius-0 record: the footprint is exactly the single center cell.
    fn rec(current: IVec2, previous: IVec2) -> MoveRecord {
        MoveRecord { current, previous, radius: 0, ..Default::default() }
    }

    fn c(x: i32, y: i32) -> IVec2 {
        IVec2::new(x, y)
    }

    #[test]
    fn no_conflict_advances_all() {
        let mut owners = OwnerGrid::new();
        let mut records = vec![rec(c(0, 0), c(-5, 0)), rec(c(10, 10), c(10, 9))];
        arbitrate(&mut records, &mut owners);
        assert!(!records[0].collided);
        assert!(!records[1].collided);
    }

    #[test]
    fn two_into_one_cell_lower_index_wins() {
        // Both propose into (0,0); record 0 is processed first and keeps it.
        let mut owners = OwnerGrid::new();
        let mut records = vec![rec(c(0, 0), c(1, 0)), rec(c(0, 0), c(0, 5))];
        arbitrate(&mut records, &mut owners);
        assert!(!records[0].collided, "first claim wins its proposal");
        assert!(records[1].collided, "second holds at its (disjoint) previous");
        assert_eq!(records[1].conflict_cell, Some(c(0, 0)));
    }

    #[test]
    fn stationary_occupant_is_protected_from_mover() {
        // record 0 (mover) steps onto record 1's stationary cell; the occupant's
        // pre-stamped footprint blocks the mover regardless of entity order.
        let mut owners = OwnerGrid::new();
        let mut records = vec![
            rec(c(5, 5), c(4, 5)), // mover: was (4,5), wants (5,5)
            rec(c(5, 5), c(5, 5)), // stationary occupant of (5,5)
        ];
        arbitrate(&mut records, &mut owners);
        assert!(records[0].collided, "mover yields to the occupant");
        assert_eq!(records[0].conflict_cell, Some(c(5, 5)));
        assert!(!records[1].collided, "stationary occupant keeps its cell");
    }

    #[test]
    fn follower_takes_leader_vacated_cell_same_frame() {
        // Leader (index 0) advances and frees its origin; the follower (index 1,
        // processed after) can move into that freed cell the same frame.
        let mut owners = OwnerGrid::new();
        let mut records = vec![
            rec(c(1, 0), c(0, 0)),  // leader: (0,0) -> (1,0)
            rec(c(0, 0), c(-1, 0)), // follower: (-1,0) -> (0,0) (leader's old cell)
        ];
        arbitrate(&mut records, &mut owners);
        assert!(!records[0].collided, "leader advances");
        assert!(!records[1].collided, "follower takes the freed cell same frame");
    }

    #[test]
    fn follower_before_leader_ripples_one_frame() {
        // Same train as above but the follower has the lower index. It is
        // processed before the leader has moved, so it sees the leader's origin
        // still occupied and holds — a one-frame ripple, never an overlap.
        let mut owners = OwnerGrid::new();
        let mut records = vec![
            rec(c(0, 0), c(-1, 0)), // follower (index 0): wants leader's cell (0,0)
            rec(c(1, 0), c(0, 0)),  // leader (index 1): (0,0) -> (1,0)
        ];
        arbitrate(&mut records, &mut owners);
        assert!(records[0].collided, "follower holds for one frame");
        assert_eq!(records[0].conflict_cell, Some(c(0, 0)));
        assert!(!records[1].collided, "leader still advances");
    }

    #[test]
    fn small_step_does_not_self_collide() {
        // A radius-1 actor stepping one subtile: its proposed footprint overlaps
        // its origin, but releasing its own origin first means it never blocks
        // on itself.
        let mut owners = OwnerGrid::new();
        let mut records = vec![MoveRecord { current: c(1, 0), previous: c(0, 0), radius: 1, ..Default::default() }];
        arbitrate(&mut records, &mut owners);
        assert!(!records[0].collided, "overlapping self-step must advance");
    }
}

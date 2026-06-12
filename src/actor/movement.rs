//! Arbitrated movement pipeline shared by every actor.
//!
//! Replaces the old per-actor `try_move` (each actor checked last frame's
//! occupancy snapshot and OR-stamped its footprint in parallel, so two actors
//! could step into the same free cell and overlap for a frame). The new model
//! resolves occupancy **authoritatively within the frame** in three phases:
//!
//! 1. [`propose_actor_moves`] (parallel) — each on-screen actor runs
//!    `think_low_level` + `prepare_movement` + [`Actor::propose_move`], which
//!    validates the step against **static** geometry only (read-only, lock-free)
//!    and records its proposed footprint compactly in [`ActorShadow`]
//!    (`proposed_center` / `origin` + radius — never explicit cell lists).
//!    Off-screen actors `advance_unchecked`; re-entrants are queued.
//! 2. [`arbitrate_actor_moves`] (sequential) — the [`OccupancyArbiter`] stamps
//!    every proposal into an **owner grid** in a deterministic (entity-sorted)
//!    order. A cell already owned by another actor is a conflict: the moving
//!    actor is backed off to its previous footprint and marked collided; if the
//!    previous footprint also conflicts, the *touched* actor is recursively
//!    backed off (depth-capped at [`MAX_BACKOFF_DEPTH`]); a still-wedged actor at
//!    the cap goes to the squeeze pool.
//! 3. Apply + squeeze (still inside [`arbitrate_actor_moves`]) — placed actors
//!    advance, collided actors hold and surface a movement error for the brain to
//!    react to next frame, and squeezed actors / re-entrants are teleported to a
//!    free cell ([`super::resolve_offscreen_collision`]).
//!
//! Accepted footprints are stamped into the [`DynamicPassabilityMap`] write
//! buffer exactly as before, so the brain's avoidance views and the async
//! pathfinder keep reading the same occupancy after the next `flush`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bevy::prelude::*;
use bevy::platform::collections::HashMap;
use bevy::utils::Parallel;

use crate::hud::game_log::{GameLog, LogEntry};
use crate::hud::perf_timings::{SystemTimings, TimedSystem};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::passability::{baked_circle_shadow, DynamicPassabilityMap};

use super::{
    resolve_offscreen_collision, Actor, ActorMovementError, ActorObject, OffScreenActor,
};

/// Maximum depth of the back-off cascade. When resolving a conflict at this
/// depth, the touched actor is squeezed (its footprint removed) instead of being
/// backed off further. Matches the spec's "step 4" cap.
pub const MAX_BACKOFF_DEPTH: u32 = 4;

// ---------------------------------------------------------------------------
// Per-actor shadow + transient proposal state
// ---------------------------------------------------------------------------

/// Per-actor footprint shadow plus the transient state the movement pipeline
/// refreshes every frame. Lives on [`ActorState`](super::ActorState); defaulted
/// on construction and **not** serialized.
///
/// Footprints are always baked circles, so they are stored compactly as a
/// center (`origin` for the back-off target, `proposed_center` for this frame's
/// candidate) plus the actor's `radius_subtiles` — never as explicit cell lists
/// (OPTIMIZATION rule 4).
#[derive(Debug, Clone, Default)]
pub struct ActorShadow {
    /// Grid center of the last accepted footprint — the fall-back target an
    /// actor is backed off to on a dynamic conflict.
    pub origin: IVec2,
    /// Float `center` one frame ago — momentum seed for a squeeze teleport.
    pub world_previous: Vec2,
    /// Proposed grid center this frame (Step 1 output, after the static slide).
    pub proposed_center: IVec2,
    /// Float `center` delta applied if the proposal is accepted (after slide).
    pub proposed_delta: Vec2,
    /// Rotation delta to apply this frame.
    pub proposed_rotation: f32,
    /// `true` once a valid on-screen proposal was produced this frame, so the
    /// actor participates in occupancy arbitration.
    pub participates: bool,
    /// First statically-blocked subtile found during the proposal, if any.
    pub static_block: Option<IVec2>,
    /// Set by the squeeze pass when the actor was teleported out of a jam, so a
    /// planner (e.g. the BlackBot brain) can re-plan from the new position.
    pub teleported: bool,
}

// ---------------------------------------------------------------------------
// Owner grid
// ---------------------------------------------------------------------------

/// Flat map from absolute world-subtile to the actor slot index that owns it
/// during one arbitration pass. Absent entries are free.
///
/// The arbiter is entirely sequential so no locking is needed — all the
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
// Pure arbitration core
// ---------------------------------------------------------------------------

/// One actor's footprint candidates and arbitration outcome for a frame.
/// Footprints are compact `(center, radius)` circles (rule 4) — plain `Copy`
/// data, no per-record buffers.
#[derive(Default, Clone, Copy)]
pub struct MoveRecord {
    /// Proposed footprint center this frame.
    pub current: IVec2,
    /// Previous (last accepted) footprint center — the back-off target.
    pub previous: IVec2,
    /// Circle radius (subtiles) shared by both footprints.
    pub radius: i32,
    /// `true` once a footprint was stamped into the owner grid.
    pub placed: bool,
    /// Which center is currently stamped (`true` = `previous`).
    pub placed_previous: bool,
    /// `true` if the actor failed to take its proposal (backed off or squeezed).
    pub collided: bool,
    /// `true` if no footprint could be placed at all (wedged past the depth cap).
    pub squeezed: bool,
    /// First conflicting subtile, for the actor's `last_movement_error`.
    pub conflict_cell: Option<IVec2>,
}

impl MoveRecord {
    fn reset(&mut self) {
        self.placed = false;
        self.placed_previous = false;
        self.collided = false;
        self.squeezed = false;
        self.conflict_cell = None;
    }
}

/// Runs the deterministic occupancy arbitration over `records` (already in the
/// desired, e.g. entity-sorted, order). Clears and rebuilds `owners`; fills
/// `squeeze` with the indices of actors that could not be placed.
pub fn arbitrate(records: &mut [MoveRecord], owners: &mut OwnerGrid, squeeze: &mut Vec<usize>) {
    owners.clear();
    squeeze.clear();
    for r in records.iter_mut() {
        r.reset();
    }
    for i in 0..records.len() {
        let r = records[i];
        match owners.first_foreign(r.current, r.radius, i as u32) {
            None => {
                owners.stamp(r.current, r.radius, i as u32);
                records[i].placed = true;
                records[i].placed_previous = false;
            }
            Some((cell, _)) => {
                records[i].collided = true;
                records[i].conflict_cell = Some(cell);
                back_off(records, owners, squeeze, i, 0);
            }
        }
    }
}

/// Places actor `i` at its **previous** footprint, recursively backing off any
/// actor whose current placement touches it. Bounded by [`MAX_BACKOFF_DEPTH`].
fn back_off(
    records: &mut [MoveRecord],
    owners: &mut OwnerGrid,
    squeeze: &mut Vec<usize>,
    i: usize,
    depth: u32,
) {
    unplace(records, owners, i);
    // Track the last j we tried to displace. If j appears a second time it means
    // j's `previous` is the same cell as i's `previous` (j keeps landing back
    // where i wants to go after being backed off). Squeeze j to break the cycle.
    let mut last_j: Option<usize> = None;
    loop {
        let r = records[i];
        match owners.first_foreign(r.previous, r.radius, i as u32) {
            None => {
                owners.stamp(r.previous, r.radius, i as u32);
                records[i].placed = true;
                records[i].placed_previous = true;
                return;
            }
            Some((cell, j)) => {
                let j = j as usize;
                if records[j].conflict_cell.is_none() {
                    records[j].conflict_cell = Some(cell);
                }
                records[j].collided = true;
                if depth >= MAX_BACKOFF_DEPTH || Some(j) == last_j {
                    // At depth cap, or j keeps re-landing on i's cell after being
                    // backed off — squeeze j to free the cell unconditionally.
                    unplace(records, owners, j);
                    if !records[j].squeezed {
                        records[j].squeezed = true;
                        squeeze.push(j);
                    }
                } else {
                    last_j = Some(j);
                    back_off(records, owners, squeeze, j, depth + 1);
                }
                // The conflicting cell is now free (j moved or was squeezed);
                // the outer loop re-scans to verify.
            }
        }
    }
}

/// Removes actor `j`'s currently-stamped footprint from the owner grid.
fn unplace(records: &mut [MoveRecord], owners: &mut OwnerGrid, j: usize) {
    let r = records[j];
    if !r.placed {
        return;
    }
    let center = if r.placed_previous { r.previous } else { r.current };
    owners.clear_cells(center, r.radius, j as u32);
    records[j].placed = false;
}

// ---------------------------------------------------------------------------
// OccupancyArbiter resource (reused scratch)
// ---------------------------------------------------------------------------

/// Reused per-frame scratch for the sequential arbitration pass. Holds the owner
/// grid, the move records, and the squeeze / re-entry work lists so the steady
/// state allocates nothing (rule 4).
#[derive(Resource)]
pub struct OccupancyArbiter {
    pub owners: OwnerGrid,
    pub records: Vec<MoveRecord>,
    pub entities: Vec<Entity>,
    pub squeeze: Vec<usize>,
    pub reentrants: Vec<Entity>,
    pub placements: Vec<(Entity, bool)>,
}

impl Default for OccupancyArbiter {
    fn default() -> Self {
        Self {
            owners: OwnerGrid::new(),
            records: Vec::new(),
            entities: Vec::new(),
            squeeze: Vec::new(),
            reentrants: Vec::new(),
            placements: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Step 1 — parallel proposal
// ---------------------------------------------------------------------------

/// Parallel proposal pass. On-screen actors think, prepare, and propose a
/// statically-validated footprint; off-screen actors advance without collision;
/// re-entrants are queued for sequential placement. Touches only the read-only
/// static cache, so it is fully parallel and order-independent.
pub(crate) fn propose_actor_moves(
    mut actors: Query<(Entity, &mut ActorObject, Option<&OffScreenActor>)>,
    hypermap: Res<HypermapRuntime>,
    mut arbiter: ResMut<OccupancyArbiter>,
    par_commands: ParallelCommands,
    timings: Res<SystemTimings>,
) {
    let _t = timings.scope(TimedSystem::Propose);
    let static_cache = hypermap.static_subtile_cache.as_ref();
    let hypermap = &*hypermap;

    let reentering: Parallel<Vec<Entity>> = Parallel::default();
    // Per-frame aggregate CPU time per sub-stage. Stack-allocated; captured by
    // reference in the closure (AtomicU64: Sync). fetch_add with Relaxed ordering
    // — same guarantee as the existing timings atomics.
    let body_ns = AtomicU64::new(0);
    let think_ns = AtomicU64::new(0);
    let slide_ns = AtomicU64::new(0);
    let advance_ns = AtomicU64::new(0);

    {
        let _par = timings.scope(TimedSystem::ProposePar);
        actors
            .par_iter_mut()
            .for_each(|(entity, mut actor_obj, off_screen)| {
                let body_start = Instant::now();
                let actor = actor_obj.inner.as_mut();
                {
                    let s = actor.state_mut();
                    s.last_movement_error = None;
                    s.shadow.participates = false;
                    s.shadow.teleported = false;
                }

                let t = Instant::now();
                actor.think_low_level();
                actor.prepare_movement();
                think_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);

                let center = actor.state().center;
                let is_rendered = hypermap
                    .is_world_pos_rendered(center.x.floor() as i32, center.y.floor() as i32);
                let was_off_screen = off_screen.is_some();

                if is_rendered {
                    if was_off_screen {
                        par_commands.command_scope(|mut commands| {
                            commands.entity(entity).remove::<OffScreenActor>();
                        });
                        reentering.borrow_local_mut().push(entity);
                    } else {
                        let t = Instant::now();
                        actor.propose_move(static_cache);
                        slide_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    }
                } else {
                    if !was_off_screen {
                        par_commands.command_scope(|mut commands| {
                            commands.entity(entity).insert(OffScreenActor);
                        });
                    }
                    let t = Instant::now();
                    actor.advance_unchecked();
                    advance_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                }
                body_ns.fetch_add(body_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            });
    }

    arbiter.reentrants.clear();
    let mut reentering = reentering;
    reentering.drain_into(&mut arbiter.reentrants);

    timings.record(TimedSystem::ProposeBody, body_ns.load(Ordering::Relaxed));
    timings.record(TimedSystem::ProposeThink, think_ns.load(Ordering::Relaxed));
    timings.record(TimedSystem::ProposeSlide, slide_ns.load(Ordering::Relaxed));
    timings.record(TimedSystem::ProposeAdvance, advance_ns.load(Ordering::Relaxed));
}

// ---------------------------------------------------------------------------
// Step 2 + 3 — sequential arbitration, apply, and squeeze
// ---------------------------------------------------------------------------

/// Sequential arbitration: build entity-sorted records, resolve occupancy over
/// the owner grid, apply each actor's outcome, stamp accepted footprints into
/// the dynamic write buffer, and teleport squeezed actors / re-entrants.
pub(crate) fn arbitrate_actor_moves(
    mut actors: Query<(Entity, &mut ActorObject, Option<&Name>)>,
    mut arbiter: ResMut<OccupancyArbiter>,
    dynamic: Res<DynamicPassabilityMap>,
    hypermap: Res<HypermapRuntime>,
    game_log: Res<GameLog>,
    timings: Res<SystemTimings>,
) {
    let static_cache = hypermap.static_subtile_cache.as_ref();

    // Stage: collect + snapshot + owner-grid conflict resolution.
    let n;
    {
        let _t = timings.scope(TimedSystem::ArbConflict);

        // Collect participating entities and sort for deterministic arbitration.
        arbiter.entities.clear();
        for (entity, actor_obj, _) in actors.iter() {
            if actor_obj.inner.state().shadow.participates {
                arbiter.entities.push(entity);
            }
        }
        arbiter.entities.sort_unstable();
        n = arbiter.entities.len();
        while arbiter.records.len() < n {
            arbiter.records.push(MoveRecord::default());
        }

        // Snapshot each actor's compact footprint into the records.
        for k in 0..n {
            let entity = arbiter.entities[k];
            let Ok((_, actor_obj, _)) = actors.get(entity) else { continue };
            let state = actor_obj.inner.state();
            let rec = &mut arbiter.records[k];
            rec.current = state.shadow.proposed_center;
            rec.previous = state.shadow.origin;
            rec.radius = state.radius_subtiles;
        }

        let arb = &mut *arbiter;
        arbitrate(&mut arb.records[..n], &mut arb.owners, &mut arb.squeeze);
    }

    // Stage: apply outcomes and stamp accepted footprints into the dynamic write buffer.
    {
        let _t = timings.scope(TimedSystem::ArbApply);
        for k in 0..n {
            let entity = arbiter.entities[k];
            if let Ok((_, mut actor_obj, _)) = actors.get_mut(entity) {
                apply_outcome(actor_obj.inner.as_mut(), &arbiter.records[k]);
            }
            let rec = arbiter.records[k];
            if rec.placed && !rec.squeezed {
                let center = if rec.placed_previous { rec.previous } else { rec.current };
                dynamic.commit_footprint(center, rec.radius);
            }
        }
    }

    // Stage: teleport squeezed actors and off-screen re-entrants.
    {
        let _t = timings.scope(TimedSystem::ArbSqueeze);
        {
            let arb = &mut *arbiter;
            arb.placements.clear();
            for &k in &arb.squeeze {
                arb.placements.push((arb.entities[k], true));
            }
            for &entity in &arb.reentrants {
                arb.placements.push((entity, false));
            }
            arb.placements.sort_unstable_by_key(|(e, _)| *e);
        }

        let placements = std::mem::take(&mut arbiter.placements);
        for (entity, squeezed) in placements.iter().copied() {
            let Ok((_, mut actor_obj, name)) = actors.get_mut(entity) else { continue };
            let actor = actor_obj.inner.as_mut();
            resolve_offscreen_collision(actor, &dynamic, static_cache);
            if squeezed {
                actor.state_mut().shadow.teleported = true;
                let tile = crate::actor::actor_main_tile(actor.state().center);
                let label = name.map(|n| n.to_string()).unwrap_or_default();
                game_log.push_world(tile.x, tile.y, LogEntry::BotSqueezedOut { name: label }, false);
            }
        }
        arbiter.placements = placements;
    }
}

/// Applies one arbitration outcome to an actor: advance on success, hold + error
/// on a dynamic conflict, surface a static slide error, and always apply rotation.
fn apply_outcome(actor: &mut dyn Actor, record: &MoveRecord) {
    let static_block = actor.state().shadow.static_block;
    let proposed_center = actor.state().shadow.proposed_center;
    let proposed_delta = actor.state().shadow.proposed_delta;
    let proposed_rotation = actor.state().shadow.proposed_rotation;
    let radius = actor.state().radius_subtiles;

    let s = actor.state_mut();
    s.rotation += proposed_rotation;

    if record.squeezed {
        // Position handled by the squeeze pass; still report the jam.
        if let Some(cell) = record.conflict_cell {
            s.last_movement_error = Some(ActorMovementError::BlockedByOccupancy {
                world_subtile_x: cell.x,
                world_subtile_y: cell.y,
            });
        }
        return;
    }

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
    fn no_conflict_places_all_at_current() {
        let mut owners = OwnerGrid::new();
        let mut squeeze = Vec::new();
        let mut records = vec![rec(c(0, 0), c(-5, 0)), rec(c(10, 10), c(10, 9))];
        arbitrate(&mut records, &mut owners, &mut squeeze);
        assert!(records[0].placed && !records[0].placed_previous && !records[0].collided);
        assert!(records[1].placed && !records[1].placed_previous && !records[1].collided);
        assert!(squeeze.is_empty());
    }

    #[test]
    fn two_into_one_cell_lower_index_wins() {
        // Both propose into (0,0); record 0 is processed first and keeps it.
        let mut owners = OwnerGrid::new();
        let mut squeeze = Vec::new();
        let mut records = vec![rec(c(0, 0), c(1, 0)), rec(c(0, 0), c(0, 5))];
        arbitrate(&mut records, &mut owners, &mut squeeze);
        assert!(records[0].placed && !records[0].collided, "first claim wins its proposal");
        assert!(
            records[1].placed && records[1].placed_previous && records[1].collided,
            "second is backed off to its (disjoint) previous and marked collided"
        );
        assert_eq!(records[1].conflict_cell, Some(c(0, 0)));
        assert!(squeeze.is_empty());
    }

    #[test]
    fn occupant_priority_mover_yields_via_backoff() {
        // record 0 (mover) steps onto record 1's stationary cell; record 1's
        // previous == current == its cell. The mover must be bounced back.
        let mut owners = OwnerGrid::new();
        let mut squeeze = Vec::new();
        let mut records = vec![
            rec(c(5, 5), c(4, 5)), // mover: was (4,5), wants (5,5)
            rec(c(5, 5), c(5, 5)), // stationary occupant of (5,5)
        ];
        arbitrate(&mut records, &mut owners, &mut squeeze);
        // Processing order: 0 takes (5,5); 1 conflicts, backs off to previous
        // (5,5) which is owned by 0, so 0 is backed off to (4,5); 1 takes (5,5).
        assert!(records[1].placed && records[1].placed_previous);
        assert!(records[0].placed && records[0].placed_previous && records[0].collided);
        assert!(squeeze.is_empty());
    }

    #[test]
    fn deep_chain_squeezes_bot_at_depth_cap() {
        // A chain of N bots each sitting where the next wants to go, all forced
        // onto a single contested cell so the cascade exceeds the depth cap.
        // Construct a worst case: every bot's current AND previous is the SAME
        // cell (no escape), so back-off can never free it and the cap triggers.
        let mut owners = OwnerGrid::new();
        let mut squeeze = Vec::new();
        let cell = c(3, 3);
        let mut records: Vec<MoveRecord> =
            (0..8).map(|_| rec(cell, cell)).collect();
        arbitrate(&mut records, &mut owners, &mut squeeze);
        // Exactly one bot holds the cell; the rest cannot be placed and at least
        // one is squeezed once the back-off cap is hit.
        let placed = records.iter().filter(|r| r.placed).count();
        assert_eq!(placed, 1, "only one bot can own the single shared cell");
        assert!(!squeeze.is_empty(), "wedged bots past the depth cap are squeezed");
        for &k in &squeeze {
            assert!(records[k].squeezed && !records[k].placed);
        }
    }

    #[test]
    fn backed_off_bot_leaves_no_ghost_owner() {
        // After a bot is backed off its old current cells must be free for others.
        let mut owners = OwnerGrid::new();
        let mut squeeze = Vec::new();
        // 0 moves (0,0)->(1,0); 1 sits at (1,0); 2 wants (0,0) (0's vacated cell).
        let mut records = vec![
            rec(c(1, 0), c(0, 0)),
            rec(c(1, 0), c(1, 0)),
            rec(c(0, 0), c(-5, 0)),
        ];
        arbitrate(&mut records, &mut owners, &mut squeeze);
        // 1 keeps (1,0); 0 backed off to (0,0); 2 then conflicts on (0,0) and is
        // backed off to its previous (-5,0). No ghost ownership remains.
        assert!(records[1].placed);
        assert!(records[0].placed && records[0].placed_previous);
        assert!(records[2].placed, "third bot still finds a home at its previous");
        assert!(squeeze.is_empty());
    }
}

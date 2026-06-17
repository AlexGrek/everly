//! DISPATCH_QUEUE — the global repair-request board plus the bot **inventory**
//! that fixer bots use to carry parts.
//!
//! When a BlackBot's movement engine or control plane breaks it is immobilized
//! and *stranded*. While stranded it posts a [`RepairRequest`] to the shared
//! [`DispatchQueue`] (what part it needs + where it is). A
//! [`Fixer`](crate::actor::black_bot::BotSpecialization::Fixer) bot loitering
//! near its home parts depot **claims** a random open request, fetches the
//! part from the depot into its [`BotInventory`] (rendered as a floating marker
//! above the sphere), drives to the stranded bot, and repairs that part on
//! contact.
//!
//! The queue is interior-mutable (a `Mutex`) so the sequential brain tick can
//! claim / release / complete requests through a shared `&DispatchQueue`, the
//! same pattern the async pathfinding queue uses. The actual world mutation
//! (resetting the stranded bot's wear) is returned as a
//! [`BrainEffect`](crate::actor::brain::BrainEffects) and applied by the owning
//! system, because it touches a *different* entity's components.

use std::sync::Mutex;

use bevy::prelude::*;

use crate::rng::{self, StdRng};

use crate::actor::black_bot::{BlackBotVisual, Breakable};
use crate::actor::charge::Charge;
use crate::actor::{actor_main_tile, ActorObject};
use crate::hud::game_log::BreakableSystem;
use crate::menu::main_menu::GameState;

/// Height (world units) of the carried-part marker above a bot's center.
const INVENTORY_MARKER_Y: f32 = 1.35;
/// Side length of the carried-part marker cube.
const INVENTORY_MARKER_SIZE: f32 = 0.22;

/// Something a fixer can carry from the parts depot and deliver to a stranded
/// bot. The three breakable kinds mirror the immobilizing parts of
/// [`Breakable`]; [`Battery`](RepairPart::Battery) is delivered to a *depleted*
/// bot and recharges it instead of repairing a part. Kept as its own small enum
/// so dispatch / inventory code never depends on the full breakable struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairPart {
    MovementEngine,
    ControlPlane,
    SensorySystem,
    /// A fresh battery for a discharged bot (recharge on delivery, no part fix).
    Battery,
}

impl RepairPart {
    /// Human-readable label for logs / inspector.
    pub fn label(self) -> &'static str {
        match self {
            RepairPart::MovementEngine => "movement engine",
            RepairPart::ControlPlane => "control plane",
            RepairPart::SensorySystem => "sensory system",
            RepairPart::Battery => "battery",
        }
    }

    /// The matching [`BreakableSystem`] for logging, or `None` for a
    /// [`Battery`](RepairPart::Battery) (which is not a breakable sub-component).
    pub fn breakable_system(self) -> Option<BreakableSystem> {
        match self {
            RepairPart::MovementEngine => Some(BreakableSystem::MovementEngine),
            RepairPart::ControlPlane => Some(BreakableSystem::ControlPlane),
            RepairPart::SensorySystem => Some(BreakableSystem::SensorySystem),
            RepairPart::Battery => None,
        }
    }

    /// Color of the carried-part marker for this part.
    fn marker_color(self) -> Color {
        match self {
            RepairPart::MovementEngine => Color::srgb(1.0, 0.55, 0.10),
            RepairPart::ControlPlane => Color::srgb(0.20, 0.75, 1.0),
            RepairPart::SensorySystem => Color::srgb(0.85, 0.30, 1.0),
            RepairPart::Battery => Color::srgb(0.30, 1.0, 0.45),
        }
    }

    /// The most critical broken part of `b` (movement engine first), or `None`
    /// when no part is broken.
    pub fn most_critical_broken(b: &Breakable) -> Option<RepairPart> {
        if b.movement_engine.broken {
            Some(RepairPart::MovementEngine)
        } else if b.control_plane.broken {
            Some(RepairPart::ControlPlane)
        } else if b.sensory_system.broken {
            Some(RepairPart::SensorySystem)
        } else {
            None
        }
    }
}

/// Seconds a request stays **unclaimable** after a fixer gives it up, so the same
/// (or another) loitering fixer can't instantly re-claim a task it just failed —
/// the source of the "camp the depot, flicker pickup/drop" loop. After it elapses
/// the request is retryable again (a transiently-blocked target gets another go; a
/// permanently-unreachable one is retried only ~once per cooldown, not every tick).
pub const FIXER_TASK_COOLDOWN_S: f32 = 6.0;

/// One open repair request: a stranded bot wants `part` delivered to `location`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RepairRequest {
    /// The stranded bot needing repair.
    pub broken_bot: Entity,
    /// Which part it needs delivered.
    pub part: RepairPart,
    /// World tile the stranded bot sits on (it cannot move, so this is stable).
    pub location: IVec2,
    /// The fixer that claimed this request, if any.
    pub claimed_by: Option<Entity>,
    /// Seconds remaining before this request may be claimed again. Set when a
    /// fixer gives the task up ([`release_with_cooldown`](DispatchQueue::release_with_cooldown));
    /// ticked down by [`tick_cooldowns`](DispatchQueue::tick_cooldowns). While
    /// `> 0` the request is invisible to `claim_nearest` / `has_open_within`.
    pub cooldown: f32,
}

/// Global board of open [`RepairRequest`]s. Interior-mutable so the sequential
/// brain tick can claim / release / complete through a shared `&`.
#[derive(Resource, Default)]
pub struct DispatchQueue {
    inner: Mutex<Vec<RepairRequest>>,
}

impl DispatchQueue {
    /// Posts a fresh request for `broken_bot`, or refreshes the `part` / `location`
    /// of an existing one (preserving its `claimed_by`).
    pub fn post(&self, broken_bot: Entity, part: RepairPart, location: IVec2) {
        let mut q = self.inner.lock().expect("dispatch queue poisoned");
        if let Some(req) = q.iter_mut().find(|r| r.broken_bot == broken_bot) {
            req.part = part;
            req.location = location;
        } else {
            q.push(RepairRequest {
                broken_bot,
                part,
                location,
                claimed_by: None,
                cooldown: 0.0,
            });
        }
    }

    /// The request currently claimed by `fixer`, if any.
    pub fn claim_of(&self, fixer: Entity) -> Option<RepairRequest> {
        let q = self.inner.lock().expect("dispatch queue poisoned");
        q.iter().find(|r| r.claimed_by == Some(fixer)).copied()
    }

    /// Claims the nearest **unclaimed, off-cooldown** request to `from` (Manhattan),
    /// marking it claimed by `fixer`, and returns it. `None` when nothing is open.
    pub fn claim_nearest(&self, fixer: Entity, from: IVec2) -> Option<RepairRequest> {
        let mut q = self.inner.lock().expect("dispatch queue poisoned");
        let idx = q
            .iter()
            .enumerate()
            .filter(|(_, r)| r.claimed_by.is_none() && r.cooldown <= 0.0)
            .min_by_key(|(_, r)| {
                (r.location.x - from.x).abs() + (r.location.y - from.y).abs()
            })
            .map(|(i, _)| i)?;
        q[idx].claimed_by = Some(fixer);
        Some(q[idx])
    }

    /// Claims a uniformly random **unclaimed, off-cooldown** request, marking it
    /// claimed by `fixer`. `None` when nothing is open.
    pub fn claim_random(&self, fixer: Entity, rng: &mut StdRng) -> Option<RepairRequest> {
        let mut q = self.inner.lock().expect("dispatch queue poisoned");
        let open: Vec<usize> = q
            .iter()
            .enumerate()
            .filter(|(_, r)| r.claimed_by.is_none() && r.cooldown <= 0.0)
            .map(|(i, _)| i)
            .collect();
        if open.is_empty() {
            return None;
        }
        let idx = *rng::pick(rng, &open);
        q[idx].claimed_by = Some(fixer);
        Some(q[idx])
    }

    /// Every **unclaimed, off-cooldown** request currently on the board.
    pub fn open_requests(&self) -> Vec<RepairRequest> {
        let q = self.inner.lock().expect("dispatch queue poisoned");
        q.iter()
            .filter(|r| r.claimed_by.is_none() && r.cooldown <= 0.0)
            .copied()
            .collect()
    }

    /// Claims the open request for `broken_bot`, if any.
    pub fn claim_bot(&self, fixer: Entity, broken_bot: Entity) -> Option<RepairRequest> {
        let mut q = self.inner.lock().expect("dispatch queue poisoned");
        let idx = q.iter().position(|r| {
            r.broken_bot == broken_bot && r.claimed_by.is_none() && r.cooldown <= 0.0
        })?;
        q[idx].claimed_by = Some(fixer);
        Some(q[idx])
    }

    /// `true` when at least one unclaimed, off-cooldown request lies within `radius`
    /// (Manhattan) of `from`. Used by a loitering fixer to decide whether to bother
    /// claiming.
    pub fn has_open_within(&self, from: IVec2, radius: i32) -> bool {
        let q = self.inner.lock().expect("dispatch queue poisoned");
        q.iter().any(|r| {
            r.claimed_by.is_none()
                && r.cooldown <= 0.0
                && (r.location.x - from.x).abs() + (r.location.y - from.y).abs() <= radius
        })
    }

    /// Releases any request claimed by `fixer` (back to unclaimed).
    pub fn release(&self, fixer: Entity) {
        let mut q = self.inner.lock().expect("dispatch queue poisoned");
        for req in q.iter_mut() {
            if req.claimed_by == Some(fixer) {
                req.claimed_by = None;
            }
        }
    }

    /// Releases `fixer`'s claim **and** bars the request from being re-claimed for
    /// `cooldown` seconds. Used when a fixer *gives a task up* (unreachable target,
    /// or too many collision/stall resets) so it — or any loitering fixer — cannot
    /// instantly re-claim it and churn pickup/drop at the depot. The plain
    /// [`release`](Self::release) (offline gate) stays cooldown-free so another
    /// fixer can cover an incapacitated one immediately.
    pub fn release_with_cooldown(&self, fixer: Entity, cooldown: f32) {
        let mut q = self.inner.lock().expect("dispatch queue poisoned");
        for req in q.iter_mut() {
            if req.claimed_by == Some(fixer) {
                req.claimed_by = None;
                req.cooldown = req.cooldown.max(cooldown);
            }
        }
    }

    /// Decrements every request's claim cooldown by `dt` (floored at 0). Run once
    /// per frame from [`maintain_dispatch_queue`].
    pub fn tick_cooldowns(&self, dt: f32) {
        let mut q = self.inner.lock().expect("dispatch queue poisoned");
        for req in q.iter_mut() {
            if req.cooldown > 0.0 {
                req.cooldown = (req.cooldown - dt).max(0.0);
            }
        }
    }

    /// Removes the request for `broken_bot` (it has been repaired or is gone).
    pub fn complete(&self, broken_bot: Entity) {
        let mut q = self.inner.lock().expect("dispatch queue poisoned");
        q.retain(|r| r.broken_bot != broken_bot);
    }

    /// Drops every request whose broken bot is no longer in `broken` and clears
    /// claims held by fixers no longer in `alive`. Run once per frame.
    pub fn maintain(&self, broken: &std::collections::HashSet<Entity>, alive: &std::collections::HashSet<Entity>) {
        let mut q = self.inner.lock().expect("dispatch queue poisoned");
        q.retain(|r| broken.contains(&r.broken_bot));
        for req in q.iter_mut() {
            if let Some(claimer) = req.claimed_by {
                if !alive.contains(&claimer) {
                    req.claimed_by = None;
                }
            }
        }
    }

    /// Number of open requests (test / inspector helper).
    pub fn len(&self) -> usize {
        self.inner.lock().expect("dispatch queue poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What a bot is carrying. Only fixer bots ever fill this, but every BlackBot
/// gets the component (and a hidden marker child) so "carrying is visible over
/// the bot" is a uniform, general mechanism.
#[derive(Component, Default)]
pub struct BotInventory {
    pub carried: Option<RepairPart>,
}

/// The floating cube child rendered above a bot when it carries a part. Holds its
/// own material handle so [`sync_inventory_markers`] can recolor it per part.
#[derive(Component)]
pub struct InventoryMarker {
    pub material: Handle<StandardMaterial>,
}

/// Spawns the (initially hidden) carried-part marker child for a bot, returning
/// its entity so the caller can parent it to the bot root.
pub fn spawn_inventory_marker(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    center: Vec2,
) -> Entity {
    let mesh = meshes.add(Cuboid::from_length(INVENTORY_MARKER_SIZE));
    let material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        emissive: LinearRgba::rgb(0.6, 0.6, 0.6),
        metallic: 0.0,
        perceptual_roughness: 0.5,
        ..default()
    });
    commands
        .spawn((
            Name::new("Bot inventory marker"),
            InventoryMarker { material: material.clone() },
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::from_xyz(center.x, INVENTORY_MARKER_Y, center.y),
            Visibility::Hidden,
        ))
        .id()
}

/// Positions each carried-part marker above its bot and toggles its visibility /
/// color from the bot's [`BotInventory`]. Runs in `Update`; the marker is excluded
/// from the generic transform sync so it keeps its above-the-bot offset.
fn sync_inventory_markers(
    bots: Query<(&ActorObject, &BotInventory, &Children)>,
    mut markers: Query<(&InventoryMarker, &mut Transform, &mut Visibility)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (obj, inventory, children) in &bots {
        let center = obj.inner.state().center;
        for child in children.iter() {
            let Ok((marker, mut transform, mut visibility)) = markers.get_mut(child) else {
                continue;
            };
            transform.translation = Vec3::new(center.x, INVENTORY_MARKER_Y, center.y);
            match inventory.carried {
                Some(part) => {
                    *visibility = Visibility::Inherited;
                    if let Some(mat) = materials.get_mut(&marker.material) {
                        let color = part.marker_color();
                        if mat.base_color != color {
                            mat.base_color = color;
                            mat.emissive = LinearRgba::from(color) * 1.5;
                        }
                    }
                }
                None => *visibility = Visibility::Hidden,
            }
        }
    }
}

/// Refreshes the [`DispatchQueue`] from the world each frame: stranded bots
/// (re)post a request for what they need — a [`Battery`](RepairPart::Battery)
/// when discharged, otherwise their most-critical broken part. A discharged bot
/// can't move regardless of repairs, so charge comes first; once recharged it
/// re-posts for any remaining break. Requests for bots no longer stranded are
/// dropped and claims by vanished fixers released.
///
/// Runs `.before` the brain tick so loitering fixers see an up-to-date board.
fn maintain_dispatch_queue(
    time: Res<Time>,
    dispatch: Res<DispatchQueue>,
    bots: Query<(Entity, &ActorObject, Option<&Breakable>, Option<&Charge>), With<BlackBotVisual>>,
) {
    use std::collections::HashSet;
    // Age out give-up cooldowns so failed tasks become retryable again.
    dispatch.tick_cooldowns(time.delta_secs());
    let mut stranded_bots: HashSet<Entity> = HashSet::new();
    let mut alive: HashSet<Entity> = HashSet::new();
    for (entity, obj, breakable, charge) in &bots {
        alive.insert(entity);
        let depleted = charge.is_some_and(Charge::is_depleted);
        // What does this bot need? A battery if discharged; else its most
        // critical immobilizing break (only those warrant a rescue).
        let part = if depleted {
            Some(RepairPart::Battery)
        } else {
            breakable.and_then(|b| {
                (b.movement_engine.broken || b.control_plane.broken)
                    .then(|| RepairPart::most_critical_broken(b))
                    .flatten()
            })
        };
        let Some(part) = part else { continue };
        let tile = actor_main_tile(obj.inner.state().center);
        dispatch.post(entity, part, tile);
        stranded_bots.insert(entity);
    }
    dispatch.maintain(&stranded_bots, &alive);
}

/// Registers the [`DispatchQueue`] resource and its maintenance / marker systems.
pub struct DispatchPlugin;

impl Plugin for DispatchPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DispatchQueue>()
            .add_systems(
                Update,
                sync_inventory_markers.run_if(in_state(GameState::InGame)),
            )
            .add_systems(
                bevy::app::FixedUpdate,
                maintain_dispatch_queue
                    .before(crate::actor::black_bot::black_bot_brain)
                    .run_if(in_state(GameState::InGame))
                    .run_if(not(crate::actor::is_paused)),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(id: u64) -> Entity {
        Entity::from_bits(id)
    }

    #[test]
    fn post_upserts_by_bot_preserving_claim() {
        let q = DispatchQueue::default();
        let b = bot(1);
        q.post(b, RepairPart::MovementEngine, IVec2::new(0, 0));
        assert_eq!(q.len(), 1);
        // Claim it, then refresh: the claim must survive the upsert.
        let claimed = q.claim_nearest(bot(99), IVec2::ZERO).unwrap();
        assert_eq!(claimed.broken_bot, b);
        q.post(b, RepairPart::ControlPlane, IVec2::new(5, 5));
        assert_eq!(q.len(), 1, "same bot upserts, not duplicates");
        let still = q.claim_of(bot(99)).unwrap();
        assert_eq!(still.claimed_by, Some(bot(99)), "claim preserved across refresh");
        assert_eq!(still.part, RepairPart::ControlPlane, "part refreshed");
        assert_eq!(still.location, IVec2::new(5, 5), "location refreshed");
    }

    #[test]
    fn claim_random_picks_from_open_pool() {
        let q = DispatchQueue::default();
        q.post(bot(1), RepairPart::MovementEngine, IVec2::new(10, 0));
        q.post(bot(2), RepairPart::MovementEngine, IVec2::new(2, 0));
        let mut rng = crate::rng::seeded(99);
        let claimed = q.claim_random(bot(50), &mut rng).unwrap();
        assert!(
            claimed.broken_bot == bot(1) || claimed.broken_bot == bot(2),
            "claims one of the open requests"
        );
        assert!(q.claim_random(bot(51), &mut rng).is_some(), "second open request remains");
        assert!(q.claim_random(bot(52), &mut rng).is_none(), "all claimed");
    }

    #[test]
    fn claim_nearest_picks_closest_unclaimed() {
        let q = DispatchQueue::default();
        q.post(bot(1), RepairPart::MovementEngine, IVec2::new(10, 0));
        q.post(bot(2), RepairPart::MovementEngine, IVec2::new(2, 0));
        let claimed = q.claim_nearest(bot(50), IVec2::ZERO).unwrap();
        assert_eq!(claimed.broken_bot, bot(2), "nearest is claimed first");
        // Already-claimed one is skipped; next claim gets the farther bot.
        let next = q.claim_nearest(bot(51), IVec2::ZERO).unwrap();
        assert_eq!(next.broken_bot, bot(1));
        assert!(q.claim_nearest(bot(52), IVec2::ZERO).is_none(), "all claimed");
    }

    #[test]
    fn release_returns_request_to_pool() {
        let q = DispatchQueue::default();
        q.post(bot(1), RepairPart::MovementEngine, IVec2::new(3, 3));
        q.claim_nearest(bot(50), IVec2::ZERO).unwrap();
        assert!(q.claim_nearest(bot(51), IVec2::ZERO).is_none());
        q.release(bot(50));
        assert!(q.claim_nearest(bot(51), IVec2::ZERO).is_some(), "released back to pool");
    }

    #[test]
    fn release_with_cooldown_bars_reclaim_until_ticked() {
        let q = DispatchQueue::default();
        q.post(bot(1), RepairPart::MovementEngine, IVec2::new(3, 3));
        q.claim_nearest(bot(50), IVec2::ZERO).unwrap();
        // Give up the task with a cooldown: it returns to the pool but stays
        // unclaimable until the cooldown elapses.
        q.release_with_cooldown(bot(50), 1.0);
        assert!(q.claim_nearest(bot(51), IVec2::ZERO).is_none(), "still cooling down");
        assert!(!q.has_open_within(IVec2::ZERO, 100), "cooled request is not 'open'");
        // Not enough time yet.
        q.tick_cooldowns(0.4);
        assert!(q.claim_nearest(bot(51), IVec2::ZERO).is_none(), "cooldown not elapsed");
        // Elapse it.
        q.tick_cooldowns(0.7);
        assert!(q.has_open_within(IVec2::ZERO, 100), "claimable again after cooldown");
        assert!(q.claim_nearest(bot(51), IVec2::ZERO).is_some(), "re-claimable after cooldown");
    }

    #[test]
    fn post_refresh_preserves_active_cooldown() {
        // A stranded bot re-posts every frame; refreshing its request must not
        // wipe an active give-up cooldown.
        let q = DispatchQueue::default();
        q.post(bot(1), RepairPart::MovementEngine, IVec2::new(3, 3));
        q.claim_nearest(bot(50), IVec2::ZERO).unwrap();
        q.release_with_cooldown(bot(50), 2.0);
        q.post(bot(1), RepairPart::MovementEngine, IVec2::new(4, 4)); // re-post (moved? no, refresh)
        assert!(q.claim_nearest(bot(51), IVec2::ZERO).is_none(), "cooldown survives re-post");
    }

    #[test]
    fn has_open_within_respects_radius_and_claims() {
        let q = DispatchQueue::default();
        q.post(bot(1), RepairPart::MovementEngine, IVec2::new(8, 0));
        assert!(q.has_open_within(IVec2::ZERO, 10));
        assert!(!q.has_open_within(IVec2::ZERO, 5), "outside radius");
        q.claim_nearest(bot(50), IVec2::ZERO).unwrap();
        assert!(!q.has_open_within(IVec2::ZERO, 10), "claimed no longer counts as open");
    }

    #[test]
    fn complete_and_maintain_remove_stale() {
        use std::collections::HashSet;
        let q = DispatchQueue::default();
        q.post(bot(1), RepairPart::MovementEngine, IVec2::ZERO);
        q.post(bot(2), RepairPart::ControlPlane, IVec2::ZERO);
        q.complete(bot(1));
        assert_eq!(q.len(), 1);

        // bot(2) is no longer broken -> maintain drops it.
        let broken: HashSet<Entity> = HashSet::new();
        let alive: HashSet<Entity> = [bot(2)].into_iter().collect();
        q.maintain(&broken, &alive);
        assert!(q.is_empty());
    }

    #[test]
    fn maintain_releases_claims_of_dead_fixers() {
        use std::collections::HashSet;
        let q = DispatchQueue::default();
        q.post(bot(1), RepairPart::MovementEngine, IVec2::ZERO);
        q.claim_nearest(bot(50), IVec2::ZERO).unwrap();
        let broken: HashSet<Entity> = [bot(1)].into_iter().collect();
        let alive: HashSet<Entity> = [bot(1)].into_iter().collect(); // fixer 50 gone
        q.maintain(&broken, &alive);
        assert!(q.claim_nearest(bot(51), IVec2::ZERO).is_some(), "dead fixer's claim freed");
    }
}

//! Lock-free sub-section timers for the actor movement pipeline and the chunk
//! streaming systems, shown beneath the FPS counter.
//!
//! Rows come in two flavors (see [`TimedSystem`] for the full list):
//!
//! - **Wall-clock scopes** (`propose`, `prop_par`, `arb_*`, `chunk_*`) — RAII
//!   [`TimingScope`]s around a system or section. Note `prop_par` wraps a
//!   `par_iter_mut`: while its scope waits for batches, the thread can execute
//!   an unrelated queued task, so a `prop_par` spike with a flat `prop_body`
//!   means *stolen* time (look at the `chunk_*` rows), not actor work.
//! - **Parallel aggregates** (`prop_body`, `prop_think`, `prop_slide`,
//!   `prop_adv`) — CPU time summed across all worker threads via per-frame
//!   `AtomicU64` accumulators; they show budget, not latency.
//!
//! Storage is pairs of atomics; `timings.scope()` is a lock-free RAII record
//! (one relaxed store + one `fetch_max` on drop), `timings.record()` stores an
//! already-measured aggregate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bevy::picking::prelude::Pickable;
use bevy::prelude::*;

use crate::menu::main_menu::GameState;
use crate::scene::camera::{spawn_camera, StrategyCameraRig};

#[derive(Clone, Copy)]
pub enum TimedSystem {
    Propose,
    /// Wall-clock of the `par_iter_mut` call alone. When this spikes while
    /// `ProposeBody` stays flat, the time went to task-pool dispatch — the
    /// waiting thread can pick up and run an unrelated queued task (e.g. a
    /// chunk-mesh system), which bills that task's duration here.
    ProposePar,
    /// Aggregate CPU of the whole per-actor closure (superset of
    /// think/slide/advance — any gap is per-actor work outside those three).
    ProposeBody,
    ProposeThink,
    ProposeSlide,
    ProposeAdvance,
    ArbConflict,
    ArbApply,
    ArbSqueeze,
    /// `update_visible_hypermap_chunks` — chunk visibility / load queueing.
    ChunkVisibility,
    /// `render_chunks_30fps` — chunk mesh build + spawn/despawn.
    ChunkRender,
    /// `refresh_chunk_upper_layers_on_floor_change` — floor-switch remesh.
    ChunkFloors,
    /// `black_bot_brain` — sequential planning tick over all bots.
    Brain,
    /// `pathfind_dispatch` — spawning queued searches onto the async pool.
    PfDispatch,
    /// `pathfind_collect` — draining finished route outcomes.
    PfCollect,
}

impl TimedSystem {
    pub const COUNT: usize = 15;
    const LABELS: [&'static str; Self::COUNT] = [
        "propose",
        "prop_par",
        "prop_body",
        "prop_think",
        "prop_slide",
        "prop_adv",
        "arb_conflict",
        "arb_apply",
        "arb_squeeze",
        "chunk_vis",
        "chunk_render",
        "chunk_floors",
        "brain",
        "pf_dispatch",
        "pf_collect",
    ];
}

/// Live gauge counters shown beneath the timers — set each frame by the
/// systems that own the values (relaxed atomic stores; lock-free).
#[derive(Resource, Default)]
pub struct PerfCounts {
    /// Pathfind requests waiting in the queue (not yet dispatched).
    pub pf_pending: AtomicU64,
    /// Pathfind searches currently running on the async pool.
    pub pf_in_flight: AtomicU64,
    /// Bots coasting on `PendingPath` (waiting for an async route).
    pub coasting_bots: AtomicU64,
    /// Bots ticked by the brain this frame.
    pub total_bots: AtomicU64,
}

#[derive(Resource)]
pub struct SystemTimings {
    last_ns: [AtomicU64; TimedSystem::COUNT],
    peak_ns: [AtomicU64; TimedSystem::COUNT],
}

impl Default for SystemTimings {
    fn default() -> Self {
        Self {
            last_ns: std::array::from_fn(|_| AtomicU64::new(0)),
            peak_ns: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl SystemTimings {
    pub fn record(&self, sys: TimedSystem, nanos: u64) {
        let i = sys as usize;
        self.last_ns[i].store(nanos, Ordering::Relaxed);
        self.peak_ns[i].fetch_max(nanos, Ordering::Relaxed);
    }

    pub fn scope(&self, sys: TimedSystem) -> TimingScope<'_> {
        TimingScope { timings: self, sys, start: Instant::now() }
    }
}

#[must_use = "bind to a local so it lives for the timed section"]
pub struct TimingScope<'a> {
    timings: &'a SystemTimings,
    sys: TimedSystem,
    start: Instant,
}

impl Drop for TimingScope<'_> {
    fn drop(&mut self) {
        self.timings.record(self.sys, self.start.elapsed().as_nanos() as u64);
    }
}

#[derive(Component)]
struct PerfTimingsText;

const REFRESH_S: f32 = 0.25;
const PEAK_HOLD_S: f32 = 1.0;

pub struct PerfTimingsPlugin;

impl Plugin for PerfTimingsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SystemTimings>()
            .init_resource::<PerfCounts>()
            .add_systems(
                OnEnter(GameState::InGame),
                spawn_perf_timings.after(spawn_camera),
            )
            .add_systems(
                Update,
                update_perf_timings.run_if(in_state(GameState::InGame)),
            );
    }
}

fn spawn_perf_timings(mut commands: Commands, camera: Query<Entity, With<StrategyCameraRig>>) {
    let Ok(cam) = camera.single() else { return };
    commands.spawn((
        Name::new("Perf timings"),
        PerfTimingsText,
        UiTargetCamera(cam),
        Pickable::IGNORE,
        Text::new("..."),
        TextFont::from_font_size(11.0),
        TextColor(Color::srgba(0.85, 0.90, 0.95, 0.65)),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(30.0),
            right: Val::Px(12.0),
            ..default()
        },
        ZIndex(1500),
    ));
}

fn update_perf_timings(
    time: Res<Time>,
    timings: Res<SystemTimings>,
    counts: Res<PerfCounts>,
    mut refresh_acc: Local<f32>,
    mut held_peak_ms: Local<[f64; TimedSystem::COUNT]>,
    mut held_age_s: Local<[f32; TimedSystem::COUNT]>,
    mut query: Query<&mut Text, With<PerfTimingsText>>,
) {
    *refresh_acc += time.delta_secs();
    if *refresh_acc < REFRESH_S {
        return;
    }
    let dt = *refresh_acc;
    *refresh_acc = 0.0;

    let Ok(mut text) = query.single_mut() else { return };

    let mut out = String::with_capacity(TimedSystem::COUNT * 28);
    for i in 0..TimedSystem::COUNT {
        let last_ms = timings.last_ns[i].load(Ordering::Relaxed) as f64 / 1.0e6;
        let candidate_ms = timings.peak_ns[i].swap(0, Ordering::Relaxed) as f64 / 1.0e6;

        if candidate_ms > held_peak_ms[i] {
            held_peak_ms[i] = candidate_ms;
            held_age_s[i] = 0.0;
        } else {
            held_age_s[i] += dt;
            if held_age_s[i] >= PEAK_HOLD_S {
                held_peak_ms[i] = 0.0;
                held_age_s[i] = 0.0;
            }
        }

        out.push_str(&format!(
            "{:<12} {:>5.2} ^{:>5.2}\n",
            TimedSystem::LABELS[i], last_ms, held_peak_ms[i],
        ));
    }
    out.push_str(&format!(
        "pf q={} fly={} coast={}/{}\n",
        counts.pf_pending.load(Ordering::Relaxed),
        counts.pf_in_flight.load(Ordering::Relaxed),
        counts.coasting_bots.load(Ordering::Relaxed),
        counts.total_bots.load(Ordering::Relaxed),
    ));
    **text = out;
}

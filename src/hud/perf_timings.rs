//! Lock-free sub-section timers for the actor movement pipeline, shown beneath
//! the FPS counter.
//!
//! Four timers cover each stage of the pipeline:
//!
//! - `Propose`    -- parallel `par_iter_mut`: `think_low_level` +
//!                   `prepare_movement` + `propose_move` (static-only slide,
//!                   shadow fill) and off-screen `advance_unchecked`.
//! - `ArbConflict`-- owner-grid conflict resolution: stamp proposals, cascade
//!                   back-off (depth ≤ 4), fill the squeeze pool.
//! - `ArbApply`   -- apply outcomes to each actor (`center`, error, shadow
//!                   swap) and stamp accepted footprints into the dynamic map.
//! - `ArbSqueeze` -- sort + teleport the squeeze pool and off-screen re-entrants.
//!
//! Storage is pairs of atomics; instrumented code calls `timings.scope()` for a
//! lock-free RAII record (one relaxed store + one `fetch_max` on drop).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bevy::picking::prelude::Pickable;
use bevy::prelude::*;

use crate::menu::main_menu::GameState;
use crate::scene::camera::{spawn_camera, StrategyCameraRig};

#[derive(Clone, Copy)]
pub enum TimedSystem {
    Propose,
    ArbConflict,
    ArbApply,
    ArbSqueeze,
}

impl TimedSystem {
    pub const COUNT: usize = 4;
    const LABELS: [&'static str; Self::COUNT] =
        ["propose", "arb_conflict", "arb_apply", "arb_squeeze"];
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
    fn record(&self, sys: TimedSystem, nanos: u64) {
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
    **text = out;
}

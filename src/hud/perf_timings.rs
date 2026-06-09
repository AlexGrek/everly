//! Lock-free per-system frame timers shown beneath the FPS counter.
//!
//! Debug instrumentation for hunting frame hitches: each candidate system wraps
//! its body in [`SystemTimings::scope`], and [`PerfTimingsPlugin`] renders the
//! per-system millisecond cost (plus a short-window peak) under the FPS HUD.
//! When the in-sync bot slowdown strikes, the row whose `peak` jumps is the
//! offending system.
//!
//! Storage is a fixed array of atomics indexed by [`TimedSystem`], reached
//! through a shared `Res<SystemTimings>`, so instrumented systems keep running
//! in parallel with no scheduling conflict (OPTIMIZATION rule 1: lock-free hot
//! path). A timed system pays one relaxed store + one `fetch_max` per frame.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bevy::picking::prelude::Pickable;
use bevy::prelude::*;

use crate::menu::main_menu::GameState;
use crate::scene::camera::{spawn_camera, StrategyCameraRig};

/// Systems instrumented by the on-screen frame-time profiler. Each variant's
/// `usize` value indexes the atomic arrays in [`SystemTimings`]; keep
/// [`TimedSystem::LABELS`] aligned with the variant order, and
/// [`TimedSystem::COUNT`] equal to the variant count.
#[derive(Clone, Copy)]
pub enum TimedSystem {
    FlushOccupancy,
    ProcessActors,
    BlackBotBrain,
    PathfindCollect,
    PathfindDispatch,
    RenderChunks,
    Diffusion,
    DirtInteraction,
    BotOccupancyHeat,
    FlushDirt,
    Charge,
}

impl TimedSystem {
    pub const COUNT: usize = 11;

    /// Display labels, ordered to match the enum variants above.
    const LABELS: [&'static str; Self::COUNT] = [
        "flush_occupancy",
        "process_actors",
        "black_bot_brain",
        "pathfind_collect",
        "pathfind_dispatch",
        "render_chunks",
        "diffusion_tick",
        "dirt_interaction",
        "bot_occ_heat",
        "flush_dirt",
        "charge",
    ];
}

/// Lock-free per-system frame durations, reached through a shared
/// `Res<SystemTimings>`.
#[derive(Resource)]
pub struct SystemTimings {
    /// Most recent frame duration (nanoseconds) per system.
    last_ns: [AtomicU64; TimedSystem::COUNT],
    /// Max duration (nanoseconds) seen since the last display refresh — a short
    /// rolling window so a one-frame spike lingers long enough to read.
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
    /// Records one system's frame duration: a relaxed store of the latest value
    /// plus a `fetch_max` into the rolling-window peak. Both lock-free.
    fn record(&self, sys: TimedSystem, nanos: u64) {
        let i = sys as usize;
        self.last_ns[i].store(nanos, Ordering::Relaxed);
        self.peak_ns[i].fetch_max(nanos, Ordering::Relaxed);
    }

    /// RAII timer: starts now, records the elapsed time into `sys` on drop.
    /// Wrap a system body with `let _t = timings.scope(TimedSystem::X);`.
    pub fn scope(&self, sys: TimedSystem) -> TimingScope<'_> {
        TimingScope {
            timings: self,
            sys,
            start: Instant::now(),
        }
    }
}

/// Drop guard returned by [`SystemTimings::scope`].
#[must_use = "bind to a local (e.g. `let _t = ...`) so it lives for the system body"]
pub struct TimingScope<'a> {
    timings: &'a SystemTimings,
    sys: TimedSystem,
    start: Instant,
}

impl Drop for TimingScope<'_> {
    fn drop(&mut self) {
        self.timings
            .record(self.sys, self.start.elapsed().as_nanos() as u64);
    }
}

#[derive(Component)]
struct PerfTimingsText;

/// Seconds between display refreshes. The peak is reported as the max over this
/// window, so a one-frame hitch stays legible for up to this long.
const REFRESH_S: f32 = 0.25;

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
    let Ok(cam) = camera.single() else {
        return;
    };

    commands.spawn((
        Name::new("Perf timings"),
        PerfTimingsText,
        UiTargetCamera(cam),
        Pickable::IGNORE,
        Text::new("profiling…"),
        TextFont::from_font_size(11.0),
        TextColor(Color::srgba(0.85, 0.90, 0.95, 0.65)),
        // Right-aligned column directly under the "NN fps" line (top: 10px).
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
    mut query: Query<&mut Text, With<PerfTimingsText>>,
) {
    *refresh_acc += time.delta_secs();
    if *refresh_acc < REFRESH_S {
        return;
    }
    *refresh_acc = 0.0;

    let Ok(mut text) = query.single_mut() else {
        return;
    };

    let mut out = String::with_capacity(TimedSystem::COUNT * 28);
    for i in 0..TimedSystem::COUNT {
        let last_ms = timings.last_ns[i].load(Ordering::Relaxed) as f64 / 1.0e6;
        // Read-and-reset the window peak so each line shows the worst frame
        // since the previous refresh, then starts a fresh window.
        let peak_ms = timings.peak_ns[i].swap(0, Ordering::Relaxed) as f64 / 1.0e6;
        out.push_str(&format!(
            "{:<16} {:>5.2} ↑{:>5.2}\n",
            TimedSystem::LABELS[i],
            last_ms,
            peak_ms,
        ));
    }
    **text = out;
}

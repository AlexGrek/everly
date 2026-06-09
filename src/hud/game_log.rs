//! In-game event log: a top-left overlay that surfaces gameplay events
//! (bot reroutes, charging) as short-lived colored lines.
//!
//! ## Hypertile-local queues (the architecture)
//! Logs are **grouped by hypertile** (one [`ChunkCoord`] queue each), not held
//! in a single global list. A queue is created lazily the first time an event
//! fires in a chunk that has none. The store uses interior mutability so events
//! can be pushed from anywhere — including parallel actor systems — through a
//! shared [`Res<GameLog>`]:
//!
//! - The chunk table is an [`RwLock<HashMap<ChunkCoord, Mutex<ChunkLog>>>`]. The
//!   warm path (chunk already exists) takes only a **read** lock to find the
//!   per-chunk `Mutex`, then locks that `Mutex` for the single push. The
//!   **write** lock is taken only on the cold path that first inserts a new
//!   hypertile's queue. Every lock is released the instant the small operation
//!   under it finishes (rule 6 of `OPTIMIZATION.md`).
//!
//! ## Storage vs. rendering (the optimization)
//! Events are pushed as plain Rust structs ([`LogEntry`]) holding *copied*
//! values — no string formatting happens at push time. A line is turned into a
//! [`String`] (via [`LogEntry::render`]) only when it is actually displayed, and
//! that string is cached on the [`StoredLog`] so it is never re-rendered. We
//! only ever render the queue for **the hypertile the camera is currently on**;
//! every other chunk's events stay as structs and age out unrendered. While the
//! panel is disabled (the default), nothing is rendered at all.
//!
//! ## Lifetime
//! Every line lives [`LOG_LIFETIME_SECS`] seconds and then disappears.
//! [`age_logs`] runs only while unpaused, so a paused game freezes the timers
//! and nothing expires until play resumes.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use bevy::picking::prelude::Pickable;
use bevy::prelude::*;

use crate::actor::is_paused;
use crate::map::hypermap::{world_to_chunk_local, ChunkCoord};
use crate::menu::main_menu::GameState;
use crate::scene::camera::{StrategyCamera, StrategyCameraRig};

/// How long (seconds) a single log line stays on screen before it disappears.
const LOG_LIFETIME_SECS: f32 = 4.0;

/// Hard cap on entries kept per hypertile so a busy chunk (or the panel left
/// off) can never grow a queue without bound. Oldest entries are dropped first.
const MAX_LOGS_PER_CHUNK: usize = 64;

/// Font size of a log line.
const LOG_FONT_SIZE: f32 = 15.0;

/// Severity of a logged event; fixes the line color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Red — something went wrong.
    Err,
    /// Yellow — a warning.
    Warn,
    /// White — neutral information.
    Info,
    /// Green — an operation completed successfully.
    Success,
    /// Light blue — an unexpected but non-fatal event.
    Unexpected,
}

impl LogLevel {
    /// Display color for this level.
    pub fn color(self) -> Color {
        match self {
            LogLevel::Err => Color::srgb(0.95, 0.25, 0.25),
            LogLevel::Warn => Color::srgb(0.97, 0.85, 0.30),
            LogLevel::Info => Color::srgb(0.95, 0.96, 0.98),
            LogLevel::Success => Color::srgb(0.35, 0.90, 0.45),
            LogLevel::Unexpected => Color::srgb(0.55, 0.80, 1.0),
        }
    }
}

/// A single logged event, stored in non-rendered form. All referenced data is
/// owned (copied at push time) so rendering later never borrows world state.
#[derive(Debug, Clone)]
pub enum LogEntry {
    /// A bot rerouted after a head-on collision with another bot.
    BotReroute { name: String },
    /// A bot docked at a charger and started charging.
    ChargingStarted { name: String },
    /// A bot finished charging (reached full) and undocked.
    ChargingDone { name: String },
    /// A free-form message with an explicit level.
    Message { level: LogLevel, text: String },
}

impl LogEntry {
    /// Severity of this entry.
    pub fn level(&self) -> LogLevel {
        match self {
            LogEntry::BotReroute { .. } => LogLevel::Unexpected,
            LogEntry::ChargingStarted { .. } => LogLevel::Info,
            LogEntry::ChargingDone { .. } => LogLevel::Success,
            LogEntry::Message { level, .. } => *level,
        }
    }

    /// Renders this entry to a display line. Called lazily, at most once per
    /// entry (the result is cached on the [`StoredLog`]).
    pub fn render(&self) -> String {
        match self {
            LogEntry::BotReroute { name } => format!("{name} rerouting after collision"),
            LogEntry::ChargingStarted { name } => format!("{name} started charging"),
            LogEntry::ChargingDone { name } => format!("{name} finished charging"),
            LogEntry::Message { text, .. } => text.clone(),
        }
    }
}

/// One stored event plus its on-screen age and lazily-rendered string cache.
#[derive(Debug)]
struct StoredLog {
    entry: LogEntry,
    /// Seconds this line has been alive; advanced only while unpaused.
    age: f32,
    /// Cached render of `entry`, populated the first time it is displayed.
    rendered: Option<String>,
}

/// One hypertile's event queue. `dirty` is set whenever the displayed set
/// changes (push / expiry) so the renderer rebuilds the overlay only on change.
#[derive(Debug, Default)]
struct ChunkLog {
    entries: VecDeque<StoredLog>,
    dirty: bool,
}

impl ChunkLog {
    fn push(&mut self, entry: LogEntry) {
        if self.entries.len() >= MAX_LOGS_PER_CHUNK {
            self.entries.pop_front();
        }
        self.entries.push_back(StoredLog { entry, age: 0.0, rendered: None });
        self.dirty = true;
    }
}

/// The event-log store and display toggle.
///
/// Disabled by default: events are still recorded (as structs, grouped per
/// hypertile), they are simply never rendered or shown until the panel is
/// enabled from the bottom HUD. Accessed through a shared [`Res<GameLog>`] — all
/// mutation goes through interior locks/atomics, so any system (parallel or not)
/// can log without exclusive access.
#[derive(Resource, Default)]
pub struct GameLog {
    /// When `false`, events are stored but never rendered or displayed.
    enabled: AtomicBool,
    /// Per-hypertile queues. Read-locked on the warm push path; write-locked
    /// only to create a new chunk's queue.
    chunks: RwLock<HashMap<ChunkCoord, Mutex<ChunkLog>>>,
}

impl GameLog {
    /// `true` when the overlay is currently shown.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Flips the overlay on/off.
    pub fn toggle(&self) {
        self.enabled.fetch_xor(true, Ordering::Relaxed);
    }

    /// Records `entry` in the queue for the hypertile containing world tile
    /// `(world_x, world_y)`. Stores the struct only — no string is rendered
    /// here, even when the panel is enabled.
    pub fn push_world(&self, world_x: i32, world_y: i32, entry: LogEntry) {
        let (coord, _) = world_to_chunk_local(world_x, world_y);
        self.push(coord, entry);
    }

    /// Records `entry` in the given hypertile's queue, holding each lock only
    /// for the push itself.
    pub fn push(&self, coord: ChunkCoord, entry: LogEntry) {
        // Warm path: the chunk's queue already exists — a read lock is enough to
        // reach its `Mutex`, which we hold only for the push.
        {
            let map = self.chunks.read().expect("game log map poisoned");
            if let Some(chunk) = map.get(&coord) {
                chunk.lock().expect("chunk log poisoned").push(entry);
                return;
            }
        }
        // Cold path: first event in this hypertile. Take the write lock just to
        // insert the queue, then release map access and push under the chunk
        // lock. `or_insert_with` covers a racing inserter.
        let mut map = self.chunks.write().expect("game log map poisoned");
        map.entry(coord)
            .or_insert_with(|| Mutex::new(ChunkLog::default()))
            .lock()
            .expect("chunk log poisoned")
            .push(entry);
    }

    /// Advances every queue's ages by `dt` and drops lines past
    /// [`LOG_LIFETIME_SECS`]. Each chunk's `Mutex` is locked one at a time and
    /// released immediately.
    fn age_all(&self, dt: f32) {
        let map = self.chunks.read().expect("game log map poisoned");
        for chunk in map.values() {
            let mut log = chunk.lock().expect("chunk log poisoned");
            if log.entries.is_empty() {
                continue;
            }
            for stored in &mut log.entries {
                stored.age += dt;
            }
            let before = log.entries.len();
            log.entries.retain(|s| s.age < LOG_LIFETIME_SECS);
            if log.entries.len() != before {
                log.dirty = true;
            }
        }
    }

    /// Returns the display lines (newest first) for `coord`, rendering each
    /// entry lazily and caching the string. Returns `None` when nothing changed
    /// since the last render of this chunk (and `force` is false), so the caller
    /// can skip rebuilding the UI. `force` is set when the camera just moved
    /// onto this hypertile (or the panel was just enabled), where a rebuild is
    /// always needed even if the queue itself is unchanged.
    fn render_lines(&self, coord: ChunkCoord, force: bool) -> Option<Vec<(Color, String)>> {
        let map = self.chunks.read().expect("game log map poisoned");
        let Some(chunk) = map.get(&coord) else {
            // No queue here: only the camera-moved-here case needs to clear
            // whatever the previous chunk left on screen.
            return force.then(Vec::new);
        };
        let mut log = chunk.lock().expect("chunk log poisoned");
        if !force && !log.dirty {
            return None;
        }
        log.dirty = false;
        let lines = log
            .entries
            .iter_mut()
            .rev()
            .map(|stored| {
                let color = stored.entry.level().color();
                let text = stored.rendered.get_or_insert_with(|| stored.entry.render()).clone();
                (color, text)
            })
            .collect();
        Some(lines)
    }
}

/// Marker for the root container of the log overlay.
#[derive(Component)]
struct GameLogRoot;

pub struct GameLogPlugin;

impl Plugin for GameLogPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GameLog>()
            .add_systems(
                OnEnter(GameState::InGame),
                spawn_log_root.after(crate::scene::camera::spawn_camera),
            )
            .add_systems(
                Update,
                (age_logs.run_if(not(is_paused)), render_logs)
                    .chain()
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

fn spawn_log_root(mut commands: Commands, camera: Query<Entity, With<StrategyCameraRig>>) {
    let Ok(cam) = camera.single() else {
        return;
    };

    commands.spawn((
        Name::new("Game log"),
        GameLogRoot,
        UiTargetCamera(cam),
        Pickable::IGNORE,
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(10.0),
            left: Val::Px(12.0),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(2.0),
            ..default()
        },
        ZIndex(1500),
    ));
}

/// Advances line ages and expires old lines. Gated on `not(is_paused)` so timers
/// freeze while the game is paused — paused logs never disappear.
fn age_logs(time: Res<Time>, log: Res<GameLog>) {
    let dt = time.delta_secs();
    if dt > 0.0 {
        log.age_all(dt);
    }
}

/// Rebuilds the overlay from the queue of the hypertile the camera is on, only
/// when that queue changed or the camera moved to a new hypertile. Renders
/// nothing while disabled (beyond clearing leftover lines once).
fn render_logs(
    log: Res<GameLog>,
    cameras: Query<&StrategyCamera>,
    root: Query<Entity, With<GameLogRoot>>,
    children: Query<&Children, With<GameLogRoot>>,
    mut last_chunk: Local<Option<ChunkCoord>>,
    mut commands: Commands,
) {
    let Ok(root) = root.single() else {
        return;
    };

    if !log.is_enabled() {
        if children.get(root).is_ok_and(|c| !c.is_empty()) {
            commands.entity(root).despawn_related::<Children>();
        }
        *last_chunk = None;
        return;
    }

    let Ok(camera) = cameras.single() else {
        return;
    };
    let (coord, _) =
        world_to_chunk_local(camera.focus.x.floor() as i32, camera.focus.z.floor() as i32);

    // Force a rebuild when the camera moved onto a different hypertile (or the
    // panel was just enabled, when `last_chunk` is `None`).
    let force = *last_chunk != Some(coord);
    *last_chunk = Some(coord);

    if let Some(lines) = log.render_lines(coord, force) {
        commands.entity(root).despawn_related::<Children>();
        for (color, text) in lines {
            let line = commands
                .spawn((
                    Text::new(text),
                    TextFont::from_font_size(LOG_FONT_SIZE),
                    TextColor(color),
                    Pickable::IGNORE,
                ))
                .id();
            commands.entity(root).add_child(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_matches_level_and_text() {
        let e = LogEntry::BotReroute { name: "Zippy".to_string() };
        assert_eq!(e.level(), LogLevel::Unexpected);
        assert_eq!(e.render(), "Zippy rerouting after collision");

        let e = LogEntry::ChargingStarted { name: "Bolt".to_string() };
        assert_eq!(e.level(), LogLevel::Info);
        assert_eq!(e.render(), "Bolt started charging");

        let e = LogEntry::ChargingDone { name: "Bolt".to_string() };
        assert_eq!(e.level(), LogLevel::Success);
        assert_eq!(e.render(), "Bolt finished charging");
    }

    #[test]
    fn push_groups_by_hypertile_and_stores_unrendered() {
        let log = GameLog::default();
        // Two events in the same chunk, one far away in another chunk.
        log.push_world(5, 5, LogEntry::Message { level: LogLevel::Info, text: "a".into() });
        log.push_world(6, 7, LogEntry::Message { level: LogLevel::Info, text: "b".into() });
        log.push_world(1000, 1000, LogEntry::Message { level: LogLevel::Err, text: "c".into() });

        let map = log.chunks.read().unwrap();
        assert_eq!(map.len(), 2, "events split across two hypertiles");
        let (here, _) = world_to_chunk_local(5, 5);
        let chunk = map.get(&here).unwrap().lock().unwrap();
        assert_eq!(chunk.entries.len(), 2);
        // Stored in non-rendered form — no string produced at push time.
        assert!(chunk.entries.iter().all(|s| s.rendered.is_none()));
    }

    #[test]
    fn render_lines_only_for_requested_chunk_newest_first() {
        let log = GameLog::default();
        let (here, _) = world_to_chunk_local(5, 5);
        log.push(here, LogEntry::Message { level: LogLevel::Info, text: "first".into() });
        log.push(here, LogEntry::Message { level: LogLevel::Info, text: "second".into() });

        let lines = log.render_lines(here, true).unwrap();
        assert_eq!(lines.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["second", "first"]);

        // Unchanged since last render and not forced → no rebuild requested.
        assert!(log.render_lines(here, false).is_none());

        // A chunk with no queue only rebuilds (to clear) when forced.
        let (elsewhere, _) = world_to_chunk_local(1000, 1000);
        assert_eq!(log.render_lines(elsewhere, true), Some(Vec::new()));
        assert!(log.render_lines(elsewhere, false).is_none());
    }

    #[test]
    fn push_caps_chunk_queue_dropping_oldest() {
        let log = GameLog::default();
        let (here, _) = world_to_chunk_local(0, 0);
        for i in 0..(MAX_LOGS_PER_CHUNK + 5) {
            log.push(here, LogEntry::Message { level: LogLevel::Info, text: format!("{i}") });
        }
        let map = log.chunks.read().unwrap();
        let chunk = map.get(&here).unwrap().lock().unwrap();
        assert_eq!(chunk.entries.len(), MAX_LOGS_PER_CHUNK);
        assert_eq!(chunk.entries.front().unwrap().entry.render(), "5");
    }

    #[test]
    fn age_expires_lines_past_lifetime() {
        let log = GameLog::default();
        let (here, _) = world_to_chunk_local(0, 0);
        log.push(here, LogEntry::Message { level: LogLevel::Info, text: "x".into() });
        log.age_all(LOG_LIFETIME_SECS + 0.1);
        let map = log.chunks.read().unwrap();
        assert!(map.get(&here).unwrap().lock().unwrap().entries.is_empty());
    }
}

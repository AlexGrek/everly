//! In-game event log: a top-left overlay that surfaces gameplay events
//! (bot reroutes, charging) as short-lived colored lines.
//!
//! ## Storage vs. rendering (the optimization)
//! Events are pushed as plain Rust structs ([`LogEntry`]) holding *copied*
//! values — no string formatting happens at push time. A line is only turned
//! into a [`String`] (via [`LogEntry::render`]) the first time it is actually
//! displayed, and that string is cached on the [`StoredLog`] so it is never
//! re-rendered. While the panel is disabled (the default), nothing is rendered
//! at all: [`render_logs`] returns immediately. Pushing therefore stays
//! allocation-free on the hot path regardless of whether the panel is shown.
//!
//! ## Lifetime
//! Every line lives [`LOG_LIFETIME_SECS`] seconds and then disappears.
//! [`age_logs`] runs only while unpaused, so a paused game freezes the timers
//! and nothing expires until play resumes.

use std::collections::VecDeque;

use bevy::picking::prelude::Pickable;
use bevy::prelude::*;

use crate::actor::is_paused;
use crate::menu::main_menu::GameState;
use crate::scene::camera::StrategyCameraRig;

/// How long (seconds) a single log line stays on screen before it disappears.
const LOG_LIFETIME_SECS: f32 = 4.0;

/// Hard cap on stored entries so a long session (or the panel left off) can
/// never grow the buffer without bound. Oldest entries are dropped first.
const MAX_STORED_LOGS: usize = 256;

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

/// The event-log store and display toggle.
///
/// Disabled by default: events are still recorded (as structs), they are simply
/// never rendered or shown until the panel is enabled from the bottom HUD.
#[derive(Resource)]
pub struct GameLog {
    /// When `false`, events are stored but never rendered or displayed.
    pub enabled: bool,
    entries: VecDeque<StoredLog>,
    /// Set when the displayed set of entries changes (push / expire / toggle),
    /// so [`render_logs`] only rebuilds the UI when something actually changed.
    dirty: bool,
}

impl Default for GameLog {
    fn default() -> Self {
        Self {
            enabled: false,
            entries: VecDeque::new(),
            dirty: false,
        }
    }
}

impl GameLog {
    /// Records an event. Stores the struct only — no string is rendered here,
    /// even when the panel is enabled. Drops the oldest entry if the buffer is
    /// full.
    pub fn push(&mut self, entry: LogEntry) {
        if self.entries.len() >= MAX_STORED_LOGS {
            self.entries.pop_front();
        }
        self.entries.push_back(StoredLog {
            entry,
            age: 0.0,
            rendered: None,
        });
        self.dirty = true;
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
                (
                    age_logs.run_if(not(is_paused)),
                    render_logs,
                )
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

/// Advances every line's age and removes any that have outlived
/// [`LOG_LIFETIME_SECS`]. Gated on `not(is_paused)` so timers freeze while the
/// game is paused — paused logs never disappear.
fn age_logs(time: Res<Time>, mut log: ResMut<GameLog>) {
    let dt = time.delta_secs();
    if dt <= 0.0 || log.entries.is_empty() {
        return;
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

/// Rebuilds the overlay's child lines when the entry set changed. Does nothing
/// (and renders nothing) while the panel is disabled, beyond clearing any lines
/// left over from when it was last enabled.
fn render_logs(
    mut log: ResMut<GameLog>,
    root: Query<Entity, With<GameLogRoot>>,
    children: Query<&Children, With<GameLogRoot>>,
    mut commands: Commands,
) {
    let Ok(root) = root.single() else {
        return;
    };

    if !log.enabled {
        // Clear leftover lines once, then stay idle while disabled.
        if children.get(root).is_ok_and(|c| !c.is_empty()) {
            commands.entity(root).despawn_related::<Children>();
        }
        return;
    }

    if !log.dirty {
        return;
    }
    log.dirty = false;

    commands.entity(root).despawn_related::<Children>();

    // Newest first: render top-down from the most recent entry.
    for stored in log.entries.iter_mut().rev() {
        let color = stored.entry.level().color();
        let text = stored
            .rendered
            .get_or_insert_with(|| stored.entry.render())
            .clone();
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
    fn push_stores_without_rendering_and_marks_dirty() {
        let mut log = GameLog::default();
        assert!(!log.dirty);
        log.push(LogEntry::Message { level: LogLevel::Err, text: "boom".to_string() });
        assert_eq!(log.entries.len(), 1);
        // Stored in non-rendered form — no string was produced at push time.
        assert!(log.entries[0].rendered.is_none());
        assert!(log.dirty);
    }

    #[test]
    fn push_caps_buffer_dropping_oldest() {
        let mut log = GameLog::default();
        for i in 0..(MAX_STORED_LOGS + 10) {
            log.push(LogEntry::Message { level: LogLevel::Info, text: format!("{i}") });
        }
        assert_eq!(log.entries.len(), MAX_STORED_LOGS);
        // Oldest ("0".."9") dropped; front is now entry index 10.
        assert_eq!(log.entries.front().unwrap().entry.render(), "10");
    }
}

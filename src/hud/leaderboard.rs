//! Bot leaderboard (modal card). Lists every bot in the world with its name,
//! specialization, charge level, and system health, ranked by charge (fullest
//! first). Opened via the "Bots" button in the bottom HUD or the `L` key; close
//! with X / Escape / clicking the scrim. Clicking a row selects that bot (the
//! right-docked inspector opens) and closes the leaderboard.
//!
//! Styled to match the overlays modal (`crate::hud::overlays`). Rows are rebuilt
//! lazily — twice a second while open, plus immediately on open — so charge and
//! health stay current without per-frame churn.

use bevy::picking::prelude::*;
use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::actor::actor_pick::ActorInspectable;
use crate::actor::black_bot::{BotSpecialization, Breakable};
use crate::actor::charge::Charge;
use crate::actor::inspect::display_actor_name;
use crate::hud::actor_inspector::SelectedActor;
use crate::menu::main_menu::GameState;
use crate::scene::camera::StrategyCameraRig;

const SCRIM: Color = Color::srgba(0.02, 0.03, 0.06, 0.68);
const CARD_BG: Color = Color::srgba(0.09, 0.11, 0.15, 0.96);
const CARD_BORDER: Color = Color::srgba(0.55, 0.62, 0.72, 0.35);
const TEXT_BRIGHT: Color = Color::srgba(0.97, 0.98, 1.0, 0.96);
const TEXT_MUTED: Color = Color::srgba(0.72, 0.76, 0.82, 0.88);
const ROW_BG: Color = Color::srgba(0.13, 0.15, 0.2, 0.7);
const ROW_BORDER: Color = Color::srgba(0.4, 0.45, 0.52, 0.25);

const HEALTH_OK: Color = Color::srgb(0.45, 0.85, 0.45);
const HEALTH_DAMAGED: Color = Color::srgb(0.97, 0.72, 0.30);
const HEALTH_OFFLINE: Color = Color::srgb(0.95, 0.40, 0.40);

const CHARGE_HIGH: Color = Color::srgb(0.45, 0.85, 0.45);
const CHARGE_MID: Color = Color::srgb(0.97, 0.85, 0.30);
const CHARGE_LOW: Color = Color::srgb(0.95, 0.40, 0.40);

const ANIM_DURATION_S: f32 = 0.18;
/// Row refresh cadence — charge / health update twice a second while open.
const REFRESH_INTERVAL_S: f32 = 0.5;

#[derive(Resource, Default)]
pub struct LeaderboardPanel {
    pub open: bool,
}

/// Marker for the bottom-HUD button that toggles the leaderboard. Defined here so
/// the toggle handler lives next to the panel; spawned by the bottom HUD bar.
#[derive(Component)]
pub struct LeaderboardToggleButton;

#[derive(Component)]
struct LeaderboardUiRoot;

#[derive(Component)]
struct LeaderboardOverlay;

#[derive(Component)]
struct LeaderboardScrim;

#[derive(Component)]
struct LeaderboardCard;

#[derive(Component)]
struct LeaderboardCloseButton;

#[derive(Component)]
struct LeaderboardListHost;

#[derive(Component)]
struct LeaderboardEmptyNote;

/// Host for the aggregate stats block (counts + percentages), rebuilt alongside
/// the rows each refresh.
#[derive(Component)]
struct LeaderboardStatsHost;

/// One rebuilt child of [`LeaderboardStatsHost`]; despawned on each refresh.
#[derive(Component)]
struct LeaderboardStatLine;

/// One leaderboard row; carries the bot entity it represents so a click can
/// select it.
#[derive(Component, Clone, Copy)]
struct LeaderboardRow(Entity);

pub struct LeaderboardPlugin;

impl Plugin for LeaderboardPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LeaderboardPanel>()
            .add_systems(
                OnEnter(GameState::InGame),
                spawn_leaderboard_ui.after(crate::scene::camera::spawn_camera),
            )
            .add_systems(
                Update,
                (
                    leaderboard_toggle_button,
                    leaderboard_key_toggle,
                    sync_leaderboard_panel,
                    animate_leaderboard,
                    leaderboard_close_input,
                    rebuild_leaderboard_rows,
                    leaderboard_row_click,
                )
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

fn spawn_leaderboard_ui(mut commands: Commands, camera: Query<Entity, With<StrategyCameraRig>>) {
    let Ok(cam) = camera.single() else {
        return;
    };

    commands
        .spawn((
            Name::new("Leaderboard UI"),
            LeaderboardUiRoot,
            UiTargetCamera(cam),
            Pickable::IGNORE,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
            ZIndex(1650),
        ))
        .with_children(|root| {
            root.spawn((
                Name::new("Leaderboard overlay"),
                LeaderboardOverlay,
                Pickable::IGNORE,
                Visibility::Hidden,
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    ..default()
                },
            ))
            .with_children(|overlay| {
                overlay
                    .spawn((
                        Name::new("Leaderboard scrim"),
                        LeaderboardScrim,
                        Pickable::default(),
                        Button,
                        Node {
                            position_type: PositionType::Absolute,
                            width: Val::Percent(100.0),
                            height: Val::Percent(100.0),
                            ..default()
                        },
                        BackgroundColor(SCRIM),
                        ZIndex(0),
                    ))
                    .observe(close_leaderboard_on_scrim_click);

                overlay
                    .spawn((
                        Name::new("Leaderboard card"),
                        LeaderboardCard,
                        Pickable::default(),
                        Node {
                            width: Val::Px(420.0),
                            max_height: Val::Percent(74.0),
                            flex_direction: FlexDirection::Column,
                            padding: UiRect::all(Val::Px(12.0)),
                            row_gap: Val::Px(8.0),
                            border: UiRect::all(Val::Px(1.0)),
                            ..default()
                        },
                        BackgroundColor(CARD_BG),
                        BorderColor::all(CARD_BORDER),
                        ZIndex(1),
                    ))
                    .with_children(|card| {
                        // Header
                        card.spawn(Node {
                            width: Val::Percent(100.0),
                            flex_direction: FlexDirection::Row,
                            justify_content: JustifyContent::SpaceBetween,
                            align_items: AlignItems::Center,
                            ..default()
                        })
                        .with_children(|header| {
                            header.spawn((
                                Text::new("BOT LEADERBOARD"),
                                TextFont::from_font_size(12.0),
                                TextColor(TEXT_BRIGHT),
                            ));

                            header
                                .spawn((
                                    Name::new("Leaderboard close"),
                                    LeaderboardCloseButton,
                                    Pickable::default(),
                                    Button,
                                    Node {
                                        width: Val::Px(22.0),
                                        height: Val::Px(22.0),
                                        justify_content: JustifyContent::Center,
                                        align_items: AlignItems::Center,
                                        ..default()
                                    },
                                    BackgroundColor(Color::srgba(0.16, 0.18, 0.22, 0.6)),
                                ))
                                .with_children(|btn| {
                                    btn.spawn((
                                        Text::new("X"),
                                        TextFont::from_font_size(12.0),
                                        TextColor(TEXT_MUTED),
                                    ));
                                });
                        });

                        // Aggregate stats block (rebuilt each refresh).
                        card.spawn((
                            LeaderboardStatsHost,
                            Node {
                                width: Val::Percent(100.0),
                                flex_direction: FlexDirection::Column,
                                row_gap: Val::Px(2.0),
                                padding: UiRect::all(Val::Px(7.0)),
                                ..default()
                            },
                            BackgroundColor(Color::srgba(0.11, 0.13, 0.18, 0.55)),
                        ));

                        // Column headers
                        card.spawn(Node {
                            width: Val::Percent(100.0),
                            flex_direction: FlexDirection::Row,
                            align_items: AlignItems::Center,
                            column_gap: Val::Px(6.0),
                            padding: UiRect::horizontal(Val::Px(8.0)),
                            ..default()
                        })
                        .with_children(|cols| {
                            spawn_col_header(cols, "Name", true);
                            spawn_col_header(cols, "Role", false);
                            spawn_col_header(cols, "Charge", false);
                            spawn_col_header(cols, "Health", false);
                        });

                        // Divider
                        card.spawn((
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(1.0),
                                ..default()
                            },
                            BackgroundColor(ROW_BORDER),
                        ));

                        // Scrollable list host (rows rebuilt each refresh).
                        card.spawn((
                            LeaderboardListHost,
                            Node {
                                width: Val::Percent(100.0),
                                flex_direction: FlexDirection::Column,
                                flex_shrink: 1.0,
                                min_height: Val::Px(0.0),
                                row_gap: Val::Px(4.0),
                                overflow: Overflow::scroll_y(),
                                ..default()
                            },
                            ScrollPosition(Vec2::ZERO),
                        ));
                    });
            });
        });
}

fn spawn_col_header(parent: &mut ChildSpawnerCommands, label: &str, grow: bool) {
    parent
        .spawn(Node {
            width: if grow { Val::Auto } else { Val::Px(COL_WIDTH) },
            flex_grow: if grow { 1.0 } else { 0.0 },
            ..default()
        })
        .with_children(|h| {
            h.spawn((
                Text::new(label.to_uppercase()),
                TextFont::from_font_size(9.0),
                TextColor(TEXT_MUTED),
            ));
        });
}

/// Fixed width of the Role / Charge / Health columns.
const COL_WIDTH: f32 = 76.0;

fn leaderboard_toggle_button(
    interactions: Query<&Interaction, (With<LeaderboardToggleButton>, Changed<Interaction>)>,
    mut panel: ResMut<LeaderboardPanel>,
) {
    for interaction in &interactions {
        if *interaction == Interaction::Pressed {
            panel.open = !panel.open;
        }
    }
}

fn leaderboard_key_toggle(keys: Res<ButtonInput<KeyCode>>, mut panel: ResMut<LeaderboardPanel>) {
    if keys.just_pressed(KeyCode::KeyL) {
        panel.open = !panel.open;
    }
}

fn sync_leaderboard_panel(
    panel: Res<LeaderboardPanel>,
    mut overlays: Query<&mut Visibility, With<LeaderboardOverlay>>,
) {
    if !panel.is_changed() {
        return;
    }
    let v = if panel.open {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    for mut vis in &mut overlays {
        *vis = v;
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn animate_leaderboard(
    panel: Res<LeaderboardPanel>,
    time: Res<Time>,
    mut progress: Local<f32>,
    mut was_open: Local<bool>,
    mut card: Query<&mut Transform, With<LeaderboardCard>>,
    mut scrim: Query<&mut BackgroundColor, With<LeaderboardScrim>>,
) {
    if panel.open && !*was_open {
        *progress = 0.0;
    }
    *was_open = panel.open;

    if !panel.open {
        return;
    }

    *progress = (*progress + time.delta_secs() / ANIM_DURATION_S).min(1.0);
    let t = ease_out_cubic(*progress);

    if let Ok(mut tf) = card.single_mut() {
        let s = 0.94 + 0.06 * t;
        tf.scale = Vec3::new(s, s, 1.0);
        tf.translation.y = 8.0 * (1.0 - t);
    }
    if let Ok(mut bg) = scrim.single_mut() {
        bg.0 = SCRIM.with_alpha(SCRIM.alpha() * t);
    }
}

fn leaderboard_close_input(
    interactions: Query<&Interaction, (With<LeaderboardCloseButton>, Changed<Interaction>)>,
    keys: Res<ButtonInput<KeyCode>>,
    mut panel: ResMut<LeaderboardPanel>,
) {
    for interaction in &interactions {
        if *interaction == Interaction::Pressed {
            panel.open = false;
        }
    }
    if keys.just_pressed(KeyCode::Escape) && panel.open {
        panel.open = false;
    }
}

fn close_leaderboard_on_scrim_click(_click: On<Pointer<Click>>, mut panel: ResMut<LeaderboardPanel>) {
    panel.open = false;
}

/// `(label, color)` describing a bot's overall system health. Charge depletion is
/// reported separately in the Charge column, so this reflects only the breakable
/// sub-systems: movement engine, control plane, sensory system.
fn health_status(breakable: Option<&Breakable>) -> (String, Color) {
    let Some(b) = breakable else {
        return ("—".to_string(), TEXT_MUTED);
    };
    let broken = [
        b.movement_engine.broken,
        b.control_plane.broken,
        b.sensory_system.broken,
    ]
    .into_iter()
    .filter(|x| *x)
    .count();
    match broken {
        0 => ("OK".to_string(), HEALTH_OK),
        // A broken movement engine or control plane immobilizes the bot.
        _ if b.movement_engine.broken || b.control_plane.broken => {
            (format!("OFFLINE ({broken})"), HEALTH_OFFLINE)
        }
        _ => (format!("{broken} hit"), HEALTH_DAMAGED),
    }
}

fn charge_color(level: f32) -> Color {
    if level > 0.5 {
        CHARGE_HIGH
    } else if level > 0.2 {
        CHARGE_MID
    } else {
        CHARGE_LOW
    }
}

/// One bot's queried columns, as yielded by the leaderboard's bot query.
type BotRow<'a> = (
    Entity,
    &'a Name,
    Option<&'a Charge>,
    Option<&'a Breakable>,
    Option<&'a BotSpecialization>,
);

fn pct(n: usize, total: usize) -> i32 {
    if total == 0 {
        0
    } else {
        (n as f32 / total as f32 * 100.0).round() as i32
    }
}

/// Recomputes the aggregate counts and repopulates the stats block. Bots are
/// bucketed mutually-exclusively by priority: **discharged** (charge depleted)
/// over **broken** (movement engine or control plane down, which immobilizes the
/// bot) over **alive** (operational), so the three buckets partition the total.
/// Alive bots are further split by specialization.
fn rebuild_stats(commands: &mut Commands, host: Entity, ranked: &[BotRow]) {
    let total = ranked.len();
    let mut discharged = 0usize;
    let mut broken = 0usize;
    let mut alive = 0usize;
    // [DoNothing, Patrol, Fixer]
    let mut alive_spec = [0usize; 4];
    for (_, _, charge, breakable, spec) in ranked {
        let depleted = charge.map_or(false, |c| c.is_depleted());
        let immobilized = breakable
            .map_or(false, |b| b.movement_engine.broken || b.control_plane.broken);
        if depleted {
            discharged += 1;
        } else if immobilized {
            broken += 1;
        } else {
            alive += 1;
            match spec.copied().unwrap_or_default() {
                BotSpecialization::DoNothing => alive_spec[0] += 1,
                BotSpecialization::Patrol => alive_spec[1] += 1,
                BotSpecialization::Fixer => alive_spec[2] += 1,
                BotSpecialization::Cleaner => alive_spec[3] += 1,
            }
        }
    }

    commands.entity(host).with_children(|parent| {
        let mut line = |text: String, size: f32, color: Color| {
            parent.spawn((
                LeaderboardStatLine,
                Text::new(text),
                TextFont::from_font_size(size),
                TextColor(color),
            ));
        };
        line(format!("Total bots: {total}"), 12.0, TEXT_BRIGHT);
        line(
            format!("Alive: {alive} ({}%)", pct(alive, total)),
            11.0,
            HEALTH_OK,
        );
        line(
            format!(
                "  DO_NOTHING {}   PATROL {}   FIXER {}   CLEANER {}",
                alive_spec[0], alive_spec[1], alive_spec[2], alive_spec[3]
            ),
            10.0,
            TEXT_MUTED,
        );
        line(
            format!("Broken: {broken} ({}%)", pct(broken, total)),
            11.0,
            HEALTH_DAMAGED,
        );
        line(
            format!("Discharged: {discharged} ({}%)", pct(discharged, total)),
            11.0,
            HEALTH_OFFLINE,
        );
    });
}

struct LeaderboardBuildState {
    refresh: Timer,
    was_open: bool,
}

impl Default for LeaderboardBuildState {
    fn default() -> Self {
        Self {
            refresh: Timer::from_seconds(REFRESH_INTERVAL_S, TimerMode::Repeating),
            was_open: false,
        }
    }
}

fn rebuild_leaderboard_rows(
    mut commands: Commands,
    time: Res<Time>,
    panel: Res<LeaderboardPanel>,
    list_host: Query<Entity, With<LeaderboardListHost>>,
    stats_host: Query<Entity, With<LeaderboardStatsHost>>,
    existing_rows: Query<Entity, With<LeaderboardRow>>,
    existing_note: Query<Entity, With<LeaderboardEmptyNote>>,
    existing_stats: Query<Entity, With<LeaderboardStatLine>>,
    bots: Query<
        (
            Entity,
            &Name,
            Option<&Charge>,
            Option<&Breakable>,
            Option<&BotSpecialization>,
        ),
        With<ActorInspectable>,
    >,
    mut state: Local<LeaderboardBuildState>,
) {
    let just_opened = panel.open && !state.was_open;
    state.was_open = panel.open;

    if !panel.open {
        state.refresh.reset();
        return;
    }

    let ticked = state.refresh.tick(time.delta()).just_finished();
    if !just_opened && !ticked {
        return;
    }

    let Ok(host) = list_host.single() else {
        return;
    };
    for row in &existing_rows {
        commands.entity(row).despawn();
    }
    for note in &existing_note {
        commands.entity(note).despawn();
    }
    for line in &existing_stats {
        commands.entity(line).despawn();
    }

    // Collect and rank by charge, fullest first (a depleted bot sinks to the
    // bottom). Bots without a `Charge` are treated as full so they sort high.
    let mut ranked: Vec<_> = bots.iter().collect();
    ranked.sort_by(|a, b| {
        let ca = a.2.map(|c| c.level).unwrap_or(1.0);
        let cb = b.2.map(|c| c.level).unwrap_or(1.0);
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
    });

    if let Ok(stats) = stats_host.single() {
        rebuild_stats(&mut commands, stats, &ranked);
    }

    if ranked.is_empty() {
        commands.entity(host).with_children(|parent| {
            parent
                .spawn((
                    LeaderboardEmptyNote,
                    Node {
                        padding: UiRect::vertical(Val::Px(8.0)),
                        ..default()
                    },
                ))
                .with_children(|p| {
                    p.spawn((
                        Text::new("No bots in the world."),
                        TextFont::from_font_size(11.0),
                        TextColor(TEXT_MUTED),
                    ));
                });
        });
        return;
    }

    for (entity, name, charge, breakable, spec) in ranked {
        let display_name = display_actor_name(name.as_str());
        let spec = spec.copied().unwrap_or_default();
        let charge_level = charge.map(|c| c.level);
        let (health_text, health_color) = health_status(breakable);

        commands.entity(host).with_children(|parent| {
            parent
                .spawn((
                    LeaderboardRow(entity),
                    Pickable::default(),
                    Button,
                    Node {
                        width: Val::Percent(100.0),
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        column_gap: Val::Px(6.0),
                        padding: UiRect::axes(Val::Px(8.0), Val::Px(5.0)),
                        ..default()
                    },
                    BackgroundColor(ROW_BG),
                ))
                .with_children(|row| {
                    // Name (grows)
                    row.spawn(Node {
                        flex_grow: 1.0,
                        flex_shrink: 1.0,
                        min_width: Val::Px(0.0),
                        ..default()
                    })
                    .with_children(|c| {
                        c.spawn((
                            Text::new(display_name.clone()),
                            TextFont::from_font_size(13.0),
                            TextColor(TEXT_BRIGHT),
                        ));
                    });

                    // Role badge
                    row.spawn(Node {
                        width: Val::Px(COL_WIDTH),
                        flex_shrink: 0.0,
                        ..default()
                    })
                    .with_children(|c| {
                        c.spawn((
                            Text::new(spec.label().to_string()),
                            TextFont::from_font_size(10.0),
                            TextColor(spec_text_color(spec)),
                        ));
                    });

                    // Charge
                    row.spawn(Node {
                        width: Val::Px(COL_WIDTH),
                        flex_shrink: 0.0,
                        ..default()
                    })
                    .with_children(|c| {
                        let (text, color) = match charge_level {
                            Some(level) => {
                                (format!("{}%", (level * 100.0).round() as i32), charge_color(level))
                            }
                            None => ("—".to_string(), TEXT_MUTED),
                        };
                        c.spawn((
                            Text::new(text),
                            TextFont::from_font_size(12.0),
                            TextColor(color),
                        ));
                    });

                    // Health
                    row.spawn(Node {
                        width: Val::Px(COL_WIDTH),
                        flex_shrink: 0.0,
                        ..default()
                    })
                    .with_children(|c| {
                        c.spawn((
                            Text::new(health_text.clone()),
                            TextFont::from_font_size(12.0),
                            TextColor(health_color),
                        ));
                    });
                });
        });
    }
}

/// Legible variant of each specialization's ring color for the dark row
/// background. The black `DO_NOTHING` ring would be invisible, so it falls back
/// to the muted text color; `PATROL` / `FIXER` use brightened ring tints.
fn spec_text_color(spec: BotSpecialization) -> Color {
    match spec {
        BotSpecialization::DoNothing => TEXT_MUTED,
        BotSpecialization::Patrol => Color::srgb(0.45, 0.70, 1.0),
        BotSpecialization::Fixer => Color::srgb(1.0, 0.42, 0.42),
        BotSpecialization::Cleaner => Color::srgb(0.35, 0.85, 0.85),
    }
}

/// Clicking a row selects that bot (opening the inspector) and closes the
/// leaderboard so the right-docked panel is unobstructed.
fn leaderboard_row_click(
    interactions: Query<(&Interaction, &LeaderboardRow), Changed<Interaction>>,
    actors: Query<(), With<ActorInspectable>>,
    mut selection: ResMut<SelectedActor>,
    mut panel: ResMut<LeaderboardPanel>,
) {
    for (interaction, row) in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if actors.get(row.0).is_err() {
            continue;
        }
        selection.entity = Some(row.0);
        panel.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_rounds_and_guards_zero_total() {
        assert_eq!(pct(0, 0), 0);
        assert_eq!(pct(1, 3), 33);
        assert_eq!(pct(2, 3), 67);
        assert_eq!(pct(3, 3), 100);
    }

    #[test]
    fn health_none_breakable_is_dash() {
        let (text, _) = health_status(None);
        assert_eq!(text, "—");
    }

    #[test]
    fn health_intact_is_ok() {
        let b = Breakable::new();
        let (text, color) = health_status(Some(&b));
        assert_eq!(text, "OK");
        assert_eq!(color, HEALTH_OK);
    }

    #[test]
    fn health_sensory_only_is_damaged_not_offline() {
        let mut b = Breakable::new();
        b.sensory_system.broken = true;
        let (text, color) = health_status(Some(&b));
        assert_eq!(text, "1 hit");
        assert_eq!(color, HEALTH_DAMAGED);
    }

    #[test]
    fn health_movement_or_control_break_is_offline() {
        let mut b = Breakable::new();
        b.movement_engine.broken = true;
        b.sensory_system.broken = true;
        let (text, color) = health_status(Some(&b));
        assert_eq!(text, "OFFLINE (2)");
        assert_eq!(color, HEALTH_OFFLINE);
    }

    #[test]
    fn charge_color_thresholds() {
        assert_eq!(charge_color(1.0), CHARGE_HIGH);
        assert_eq!(charge_color(0.51), CHARGE_HIGH);
        assert_eq!(charge_color(0.50), CHARGE_MID);
        assert_eq!(charge_color(0.21), CHARGE_MID);
        assert_eq!(charge_color(0.20), CHARGE_LOW);
        assert_eq!(charge_color(0.0), CHARGE_LOW);
    }
}

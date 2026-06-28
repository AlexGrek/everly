//! Overlays control panel (modal card). Collects all "visibility trigger" toggles
//! (Ambient fill, Sun, Occupancy F4, Temperature/Heat F5, Paths F6, Water, Log) so the
//! bottom HUD bar stays uncluttered. Opened via the "Overlays" button; keys still
//! work when closed. Styled similarly to the actor inspector modal.

use bevy::picking::prelude::*;
use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::hud::game_log::GameLog;
use crate::map::chunk_overlay::{OccupancyOverlayEnabled, PathOverlayEnabled};
use crate::map::hypermap_world::WaterRenderingEnabled;
use crate::map::temperature_overlay::TemperatureOverlayEnabled;
use crate::menu::main_menu::GameState;
use crate::scene::camera::{AmbientFillEnabled, StrategyCameraRig};
use crate::scene::sun::SunEnabled;

const SCRIM: Color = Color::srgba(0.02, 0.03, 0.06, 0.68);
const CARD_BG: Color = Color::srgba(0.09, 0.11, 0.15, 0.96);
const CARD_BORDER: Color = Color::srgba(0.55, 0.62, 0.72, 0.35);
const TEXT_BRIGHT: Color = Color::srgba(0.97, 0.98, 1.0, 0.96);
const TEXT_MUTED: Color = Color::srgba(0.72, 0.76, 0.82, 0.88);

const BTN_BG: Color = Color::srgba(0.18, 0.2, 0.24, 0.55);
const BTN_BORDER: Color = Color::srgba(0.9, 0.92, 0.96, 0.35);
const TEXT_MAIN: Color = Color::srgba(0.95, 0.96, 0.98, 0.92);

const ANIM_DURATION_S: f32 = 0.18;

#[derive(Resource, Default)]
pub struct OverlaysPanel {
    pub open: bool,
}

#[derive(Component)]
struct OverlaysUiRoot;

#[derive(Component)]
struct OverlaysOverlay;

#[derive(Component)]
struct OverlaysScrim;

#[derive(Component)]
struct OverlaysCard;

#[derive(Component)]
struct OverlaysCloseButton;

#[derive(Component)]
struct OverlaysAmbientButton;
#[derive(Component)]
struct OverlaysAmbientLabel;

#[derive(Component)]
struct OverlaysSunButton;
#[derive(Component)]
struct OverlaysSunLabel;

#[derive(Component)]
struct OverlaysOccupancyButton;
#[derive(Component)]
struct OverlaysOccupancyLabel;

#[derive(Component)]
struct OverlaysHeatmapButton;
#[derive(Component)]
struct OverlaysHeatmapLabel;

#[derive(Component)]
struct OverlaysPathButton;
#[derive(Component)]
struct OverlaysPathLabel;

#[derive(Component)]
struct OverlaysWaterButton;
#[derive(Component)]
struct OverlaysWaterLabel;

#[derive(Component)]
struct OverlaysLogButton;
#[derive(Component)]
struct OverlaysLogLabel;

pub struct OverlaysPlugin;

impl Plugin for OverlaysPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OverlaysPanel>()
            .add_systems(
                OnEnter(GameState::InGame),
                spawn_overlays_ui.after(crate::scene::camera::spawn_camera),
            )
            .add_systems(
                Update,
                (
                    sync_overlays_panel,
                    animate_overlays,
                    overlays_close_input,
                )
                    .run_if(in_state(GameState::InGame)),
            )
            .add_systems(
                Update,
                (
                    ambient_fill_toggle_button,
                    sync_ambient_toggle_label,
                    sun_toggle_button,
                    sync_sun_toggle_label,
                    occupancy_toggle_button,
                    sync_occupancy_toggle_label,
                    heatmap_toggle_button,
                    sync_heatmap_toggle_label,
                    path_toggle_button,
                    sync_path_toggle_label,
                    water_toggle_button,
                    sync_water_toggle_label,
                    log_toggle_button,
                    sync_log_toggle_label,
                )
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

fn spawn_overlays_ui(mut commands: Commands, camera: Query<Entity, With<StrategyCameraRig>>) {
    let Ok(cam) = camera.single() else {
        return;
    };

    commands
        .spawn((
            Name::new("Overlays UI"),
            OverlaysUiRoot,
            UiTargetCamera(cam),
            Pickable::IGNORE,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
            ZIndex(1600),
        ))
        .with_children(|root| {
            root.spawn((
                Name::new("Overlays overlay"),
                OverlaysOverlay,
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
                        Name::new("Overlays scrim"),
                        OverlaysScrim,
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
                    .observe(close_overlays_on_scrim_click);

                overlay
                    .spawn((
                        Name::new("Overlays card"),
                        OverlaysCard,
                        Pickable::default(),
                        Node {
                            width: Val::Px(380.0),
                            flex_direction: FlexDirection::Column,
                            padding: UiRect::all(Val::Px(16.0)),
                            row_gap: Val::Px(10.0),
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
                                Text::new("Overlays"),
                                TextFont::from_font_size(18.0),
                                TextColor(TEXT_BRIGHT),
                            ));

                            header
                                .spawn((
                                    Name::new("Overlays close"),
                                    OverlaysCloseButton,
                                    Pickable::default(),
                                    Button,
                                    Node {
                                        width: Val::Px(28.0),
                                        height: Val::Px(28.0),
                                        justify_content: JustifyContent::Center,
                                        align_items: AlignItems::Center,
                                        border: UiRect::all(Val::Px(1.0)),
                                        ..default()
                                    },
                                    BorderColor::all(CARD_BORDER),
                                    BackgroundColor(Color::srgba(0.14, 0.16, 0.2, 0.8)),
                                ))
                                .with_children(|btn| {
                                    btn.spawn((
                                        Text::new("X"),
                                        TextFont::from_font_size(18.0),
                                        TextColor(TEXT_MUTED),
                                    ));
                                });
                        });

                        // Divider
                        card.spawn((
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(1.0),
                                ..default()
                            },
                            BackgroundColor(CARD_BORDER),
                        ));

                        // Controls (the moved visibility toggles)
                        card.spawn(Node {
                            width: Val::Percent(100.0),
                            flex_direction: FlexDirection::Column,
                            row_gap: Val::Px(6.0),
                            ..default()
                        })
                        .with_children(|ctrls| {
                            spawn_vis_button(ctrls, OverlaysAmbientButton, OverlaysAmbientLabel, "Ambient: On");
                            spawn_vis_button(ctrls, OverlaysSunButton, OverlaysSunLabel, "Sun: On");
                            spawn_vis_button(ctrls, OverlaysOccupancyButton, OverlaysOccupancyLabel, "Occ: Off");
                            spawn_vis_button(ctrls, OverlaysHeatmapButton, OverlaysHeatmapLabel, "Heat: Off");
                            spawn_vis_button(ctrls, OverlaysPathButton, OverlaysPathLabel, "Path: Off");
                            spawn_vis_button(ctrls, OverlaysWaterButton, OverlaysWaterLabel, "Water: On");
                            spawn_vis_button(ctrls, OverlaysLogButton, OverlaysLogLabel, "Log: On");
                        });
                    });
            });
        });
}

fn spawn_vis_button(
    parent: &mut ChildSpawnerCommands,
    btn_marker: impl Component,
    label_marker: impl Component,
    initial_text: &str,
) {
    parent
        .spawn((
            btn_marker,
            Button,
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(30.0),
                padding: UiRect::horizontal(Val::Px(10.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(BTN_BORDER),
            BackgroundColor(BTN_BG),
        ))
        .with_children(|p| {
            p.spawn((
                label_marker,
                Text::new(initial_text),
                TextFont::from_font_size(14.0),
                TextColor(TEXT_MAIN),
            ));
        });
}

fn sync_overlays_panel(
    panel: Res<OverlaysPanel>,
    mut overlays: Query<&mut Visibility, With<OverlaysOverlay>>,
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

fn animate_overlays(
    panel: Res<OverlaysPanel>,
    time: Res<Time>,
    mut progress: Local<f32>,
    mut was_open: Local<bool>,
    mut card: Query<&mut Transform, With<OverlaysCard>>,
    mut scrim: Query<&mut BackgroundColor, With<OverlaysScrim>>,
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

fn overlays_close_input(
    interactions: Query<&Interaction, (With<OverlaysCloseButton>, Changed<Interaction>)>,
    keys: Res<ButtonInput<KeyCode>>,
    mut panel: ResMut<OverlaysPanel>,
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

fn close_overlays_on_scrim_click(_click: On<Pointer<Click>>, mut panel: ResMut<OverlaysPanel>) {
    panel.open = false;
}

// --- Moved visibility toggle handlers (button clicks still work when panel is closed via keys) ---

fn ambient_fill_toggle_button(
    interactions: Query<&Interaction, (With<OverlaysAmbientButton>, Changed<Interaction>)>,
    mut fill: ResMut<AmbientFillEnabled>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        fill.0 = !fill.0;
    }
}

fn sync_ambient_toggle_label(
    fill: Res<AmbientFillEnabled>,
    mut texts: Query<&mut Text, With<OverlaysAmbientLabel>>,
) {
    if !fill.is_changed() {
        return;
    }
    let label = if fill.0 { "Ambient: On" } else { "Ambient: Off" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

fn sun_toggle_button(
    interactions: Query<&Interaction, (With<OverlaysSunButton>, Changed<Interaction>)>,
    mut enabled: ResMut<SunEnabled>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        enabled.0 = !enabled.0;
    }
}

fn sync_sun_toggle_label(
    enabled: Res<SunEnabled>,
    mut texts: Query<&mut Text, With<OverlaysSunLabel>>,
) {
    if !enabled.is_changed() {
        return;
    }
    let label = if enabled.0 { "Sun: On" } else { "Sun: Off" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

fn occupancy_toggle_button(
    interactions: Query<&Interaction, (With<OverlaysOccupancyButton>, Changed<Interaction>)>,
    mut enabled: ResMut<OccupancyOverlayEnabled>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        enabled.0 = !enabled.0;
    }
}

fn sync_occupancy_toggle_label(
    enabled: Res<OccupancyOverlayEnabled>,
    mut texts: Query<&mut Text, With<OverlaysOccupancyLabel>>,
) {
    if !enabled.is_changed() {
        return;
    }
    let label = if enabled.0 { "Occ: On" } else { "Occ: Off" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

fn heatmap_toggle_button(
    interactions: Query<&Interaction, (With<OverlaysHeatmapButton>, Changed<Interaction>)>,
    mut enabled: ResMut<TemperatureOverlayEnabled>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        enabled.0 = !enabled.0;
    }
}

fn sync_heatmap_toggle_label(
    enabled: Res<TemperatureOverlayEnabled>,
    mut texts: Query<&mut Text, With<OverlaysHeatmapLabel>>,
) {
    if !enabled.is_changed() {
        return;
    }
    let label = if enabled.0 { "Heat: On" } else { "Heat: Off" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

fn path_toggle_button(
    interactions: Query<&Interaction, (With<OverlaysPathButton>, Changed<Interaction>)>,
    mut enabled: ResMut<PathOverlayEnabled>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        enabled.0 = !enabled.0;
    }
}

fn sync_path_toggle_label(
    enabled: Res<PathOverlayEnabled>,
    mut texts: Query<&mut Text, With<OverlaysPathLabel>>,
) {
    if !enabled.is_changed() {
        return;
    }
    let label = if enabled.0 { "Path: On" } else { "Path: Off" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

fn water_toggle_button(
    interactions: Query<&Interaction, (With<OverlaysWaterButton>, Changed<Interaction>)>,
    mut enabled: ResMut<WaterRenderingEnabled>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        enabled.0 = !enabled.0;
    }
}

fn sync_water_toggle_label(
    enabled: Res<WaterRenderingEnabled>,
    mut texts: Query<&mut Text, With<OverlaysWaterLabel>>,
) {
    if !enabled.is_changed() {
        return;
    }
    let label = if enabled.0 { "Water: On" } else { "Water: Off" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

fn log_toggle_button(
    interactions: Query<&Interaction, (With<OverlaysLogButton>, Changed<Interaction>)>,
    log: Res<GameLog>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        log.toggle();
    }
}

fn sync_log_toggle_label(
    log: Res<GameLog>,
    mut last: Local<Option<bool>>,
    mut texts: Query<&mut Text, With<OverlaysLogLabel>>,
) {
    let enabled = log.is_enabled();
    if *last == Some(enabled) {
        return;
    }
    *last = Some(enabled);
    let label = if enabled { "Log: On" } else { "Log: Off" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

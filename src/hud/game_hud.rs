//! Bottom-screen HUD: semi-transparent controls wired to gameplay.

use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::scene::camera::{
    spawn_camera, AmbientFillEnabled, StrategyCamera, StrategyCameraRig,
    StrategyCameraViewMode, STRATEGY_CAMERA_DEFAULT_PITCH, STRATEGY_CAMERA_MAP_PITCH,
};
use crate::edit::actor_spawn::{ActorSpawnToggleButton, ActorSpawnToggleLabel};
use crate::edit::map_edit::{MapEditToggleButton, MapEditToggleLabel};
use crate::map::chunk_overlay::OccupancyOverlayEnabled;
use crate::map::temperature_overlay::TemperatureOverlayEnabled;
use crate::map::floor_level::{ActiveFloorLevel, HYPERMAP_FLOOR_MAX};
use crate::menu::main_menu::GameState;

const BAR_BG: Color = Color::srgba(0.06, 0.07, 0.1, 0.62);
const BTN_BG: Color = Color::srgba(0.18, 0.2, 0.24, 0.55);
const BTN_BORDER: Color = Color::srgba(0.9, 0.92, 0.96, 0.35);
const TEXT_MAIN: Color = Color::srgba(0.95, 0.96, 0.98, 0.92);

#[derive(Component)]
struct MapViewToggleButton;

#[derive(Component)]
struct FloorLevelDownButton;

#[derive(Component)]
struct FloorLevelUpButton;

#[derive(Component)]
struct FloorHudLevelText;

#[derive(Component)]
struct AmbientToggleButton;

#[derive(Component)]
struct AmbientToggleLabel;

#[derive(Component)]
struct OccupancyToggleButton;

#[derive(Component)]
struct OccupancyToggleLabel;

#[derive(Component)]
struct HeatmapToggleButton;

#[derive(Component)]
struct HeatmapToggleLabel;

pub struct GameHudPlugin;

impl Plugin for GameHudPlugin {
    fn build(&self, app: &mut App) {
        // The HUD attaches itself to the strategy camera (`UiTargetCamera`),
        // so it must spawn after `spawn_camera`'s entity is on the world.
        // Without `.after`, both systems run in parallel inside `OnEnter` and
        // the HUD's `Query<&StrategyCameraRig>::single()` returns `Err`.
        app.add_systems(
            OnEnter(GameState::InGame),
            spawn_bottom_hud.after(spawn_camera),
        )
        .add_systems(
            Update,
            (
                map_button_toggle_views,
                map_key_toggle_views,
                ambient_fill_toggle_button,
                sync_ambient_toggle_label,
                occupancy_toggle_button,
                sync_occupancy_toggle_label,
                heatmap_toggle_button,
                sync_heatmap_toggle_label,
                floor_level_buttons,
                update_floor_level_readout,
            )
                .run_if(in_state(GameState::InGame)),
        );
    }
}

pub(crate) fn spawn_bottom_hud(mut commands: Commands, camera: Query<Entity, With<StrategyCameraRig>>) {
    let Ok(cam) = camera.single() else {
        return;
    };

    commands
        .spawn((
            Name::new("Bottom HUD"),
            UiTargetCamera(cam),
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Px(52.0),
                bottom: Val::Px(0.0),
                left: Val::Px(0.0),
                padding: UiRect::axes(Val::Px(14.0), Val::Px(8.0)),
                column_gap: Val::Px(10.0),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::FlexStart,
                ..default()
            },
            BackgroundColor(BAR_BG),
            ZIndex(1000),
        ))
        .with_children(|parent| {
            parent
                .spawn((
                    Name::new("HUD Map view toggle"),
                    MapViewToggleButton,
                    Button,
                    Node {
                        min_width: Val::Px(88.0),
                        height: Val::Px(36.0),
                        padding: UiRect::horizontal(Val::Px(14.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        ..default()
                    },
                    BorderColor::all(BTN_BORDER),
                    BackgroundColor(BTN_BG),
                ))
                .with_children(|p| {
                    p.spawn((
                        Text::new("Map"),
                        TextFont::from_font_size(17.0),
                        TextColor(TEXT_MAIN),
                    ));
                });

            parent
                .spawn((
                    Name::new("HUD Map edit toggle"),
                    MapEditToggleButton,
                    Button,
                    Node {
                        min_width: Val::Px(72.0),
                        height: Val::Px(36.0),
                        padding: UiRect::horizontal(Val::Px(12.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        ..default()
                    },
                    BorderColor::all(BTN_BORDER),
                    BackgroundColor(BTN_BG),
                ))
                .with_children(|p| {
                    p.spawn((
                        MapEditToggleLabel,
                        Text::new("Edit"),
                        TextFont::from_font_size(17.0),
                        TextColor(TEXT_MAIN),
                    ));
                });

            parent
                .spawn((
                    Name::new("HUD actor spawn toggle"),
                    ActorSpawnToggleButton,
                    Button,
                    Node {
                        min_width: Val::Px(86.0),
                        height: Val::Px(36.0),
                        padding: UiRect::horizontal(Val::Px(12.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        ..default()
                    },
                    BorderColor::all(BTN_BORDER),
                    BackgroundColor(BTN_BG),
                ))
                .with_children(|p| {
                    p.spawn((
                        ActorSpawnToggleLabel,
                        Text::new("Actors"),
                        TextFont::from_font_size(17.0),
                        TextColor(TEXT_MAIN),
                    ));
                });

            parent
                .spawn((
                    Name::new("HUD ambient fill toggle"),
                    AmbientToggleButton,
                    Button,
                    Node {
                        min_width: Val::Px(118.0),
                        height: Val::Px(36.0),
                        padding: UiRect::horizontal(Val::Px(12.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        ..default()
                    },
                    BorderColor::all(BTN_BORDER),
                    BackgroundColor(BTN_BG),
                ))
                .with_children(|p| {
                    p.spawn((
                        AmbientToggleLabel,
                        Text::new("Ambient: On"),
                        TextFont::from_font_size(17.0),
                        TextColor(TEXT_MAIN),
                    ));
                });

            parent
                .spawn((
                    Name::new("HUD occupancy overlay toggle"),
                    OccupancyToggleButton,
                    Button,
                    Node {
                        min_width: Val::Px(88.0),
                        height: Val::Px(36.0),
                        padding: UiRect::horizontal(Val::Px(12.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        ..default()
                    },
                    BorderColor::all(BTN_BORDER),
                    BackgroundColor(BTN_BG),
                ))
                .with_children(|p| {
                    p.spawn((
                        OccupancyToggleLabel,
                        Text::new("Occ: Off"),
                        TextFont::from_font_size(17.0),
                        TextColor(TEXT_MAIN),
                    ));
                });

            parent
                .spawn((
                    Name::new("HUD heatmap toggle"),
                    HeatmapToggleButton,
                    Button,
                    Node {
                        min_width: Val::Px(108.0),
                        height: Val::Px(36.0),
                        padding: UiRect::horizontal(Val::Px(12.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        ..default()
                    },
                    BorderColor::all(BTN_BORDER),
                    BackgroundColor(BTN_BG),
                ))
                .with_children(|p| {
                    p.spawn((
                        HeatmapToggleLabel,
                        Text::new("Heat: Off"),
                        TextFont::from_font_size(17.0),
                        TextColor(TEXT_MAIN),
                    ));
                });

            parent.spawn(Node {
                flex_grow: 1.0,
                min_width: Val::Px(8.0),
                ..default()
            });

            parent
                .spawn((
                    Name::new("HUD floor controls"),
                    Node {
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        column_gap: Val::Px(8.0),
                        ..default()
                    },
                ))
                .with_children(|row| {
                    row.spawn((
                        Name::new("HUD floor down"),
                        FloorLevelDownButton,
                        Button,
                        Node {
                            width: Val::Px(40.0),
                            height: Val::Px(36.0),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            border: UiRect::all(Val::Px(1.0)),
                            border_radius: BorderRadius::all(Val::Px(6.0)),
                            ..default()
                        },
                        BorderColor::all(BTN_BORDER),
                        BackgroundColor(BTN_BG),
                    ))
                    .with_children(|p| {
                        p.spawn((
                            Text::new("-"),
                            TextFont::from_font_size(22.0),
                            TextColor(TEXT_MAIN),
                        ));
                    });

                    row.spawn((
                        Name::new("HUD floor readout"),
                        FloorHudLevelText,
                        Text::new("Floor 0"),
                        TextFont::from_font_size(16.0),
                        TextColor(TEXT_MAIN),
                    ));

                    row.spawn((
                        Name::new("HUD floor up"),
                        FloorLevelUpButton,
                        Button,
                        Node {
                            width: Val::Px(40.0),
                            height: Val::Px(36.0),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            border: UiRect::all(Val::Px(1.0)),
                            border_radius: BorderRadius::all(Val::Px(6.0)),
                            ..default()
                        },
                        BorderColor::all(BTN_BORDER),
                        BackgroundColor(BTN_BG),
                    ))
                    .with_children(|p| {
                        p.spawn((
                            Text::new("+"),
                            TextFont::from_font_size(20.0),
                            TextColor(TEXT_MAIN),
                        ));
                    });
                });
        });
}

fn ambient_fill_toggle_button(
    interactions: Query<&Interaction, (With<AmbientToggleButton>, Changed<Interaction>)>,
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
    mut texts: Query<&mut Text, With<AmbientToggleLabel>>,
) {
    if !fill.is_changed() {
        return;
    }
    let label = if fill.0 { "Ambient: On" } else { "Ambient: Off" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

fn toggle_strategy_camera_view(cam: &mut StrategyCamera) {
    match cam.view_mode {
        StrategyCameraViewMode::Strategy => {
            cam.view_mode = StrategyCameraViewMode::Map;
            cam.pitch = STRATEGY_CAMERA_MAP_PITCH;
        }
        StrategyCameraViewMode::Map => {
            cam.view_mode = StrategyCameraViewMode::Strategy;
            cam.pitch = STRATEGY_CAMERA_DEFAULT_PITCH;
        }
    }
}

fn map_button_toggle_views(
    interactions: Query<&Interaction, (With<MapViewToggleButton>, Changed<Interaction>)>,
    mut cameras: Query<&mut StrategyCamera>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        for mut cam in &mut cameras {
            toggle_strategy_camera_view(&mut cam);
        }
    }
}

fn map_key_toggle_views(
    keys: Res<ButtonInput<KeyCode>>,
    mut cameras: Query<&mut StrategyCamera>,
) {
    if !keys.just_pressed(KeyCode::KeyM) {
        return;
    }
    for mut cam in &mut cameras {
        toggle_strategy_camera_view(&mut cam);
    }
}

fn floor_level_buttons(
    down: Query<&Interaction, (With<FloorLevelDownButton>, Changed<Interaction>)>,
    up: Query<&Interaction, (With<FloorLevelUpButton>, Changed<Interaction>)>,
    mut floor: ResMut<ActiveFloorLevel>,
) {
    for interaction in &down {
        if *interaction == Interaction::Pressed {
            floor.0 = (floor.0 - 1).max(0);
        }
    }
    for interaction in &up {
        if *interaction == Interaction::Pressed {
            floor.0 = (floor.0 + 1).min(HYPERMAP_FLOOR_MAX);
        }
    }
}

fn update_floor_level_readout(
    floor: Res<ActiveFloorLevel>,
    mut texts: Query<&mut Text, With<FloorHudLevelText>>,
) {
    if !floor.is_changed() {
        return;
    }
    for mut text in &mut texts {
        **text = format!("Floor {}", floor.0);
    }
}

fn occupancy_toggle_button(
    interactions: Query<&Interaction, (With<OccupancyToggleButton>, Changed<Interaction>)>,
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
    mut texts: Query<&mut Text, With<OccupancyToggleLabel>>,
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
    interactions: Query<&Interaction, (With<HeatmapToggleButton>, Changed<Interaction>)>,
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
    mut texts: Query<&mut Text, With<HeatmapToggleLabel>>,
) {
    if !enabled.is_changed() {
        return;
    }
    let label = if enabled.0 { "Heat: On" } else { "Heat: Off" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

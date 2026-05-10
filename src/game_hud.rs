//! Bottom-screen HUD: semi-transparent controls wired to gameplay.

use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::camera::{
    StrategyCamera, StrategyCameraRig, StrategyCameraViewMode, STRATEGY_CAMERA_DEFAULT_PITCH,
    STRATEGY_CAMERA_MAP_PITCH,
};
use crate::floor_level::{ActiveFloorLevel, HYPERMAP_FLOOR_MAX};
use crate::map_edit::{MapEditToggleButton, MapEditToggleLabel};

const BAR_BG: Color = Color::srgba(0.06, 0.07, 0.1, 0.62);
const BTN_BG: Color = Color::srgba(0.18, 0.2, 0.24, 0.55);
const BTN_BORDER: Color = Color::srgba(0.9, 0.92, 0.96, 0.35);
const TEXT_MAIN: Color = Color::srgba(0.95, 0.96, 0.98, 0.92);
const TEXT_DIM: Color = Color::srgba(0.75, 0.78, 0.82, 0.55);

#[derive(Component)]
struct MapViewToggleButton;

#[derive(Component)]
struct FloorLevelDownButton;

#[derive(Component)]
struct FloorLevelUpButton;

#[derive(Component)]
struct FloorHudLevelText;

pub struct GameHudPlugin;

impl Plugin for GameHudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_bottom_hud).add_systems(
            Update,
            (
                map_button_toggle_views,
                floor_level_buttons,
                update_floor_level_readout,
            ),
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
                    Name::new("HUD placeholder button"),
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
                        Text::new("…"),
                        TextFont::from_font_size(17.0),
                        TextColor(TEXT_DIM),
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
                            Text::new("−"),
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

fn map_button_toggle_views(
    interactions: Query<&Interaction, (With<MapViewToggleButton>, Changed<Interaction>)>,
    mut cameras: Query<&mut StrategyCamera>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        for mut cam in &mut cameras {
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
    }
}

fn floor_level_buttons(
    down: Query<&Interaction, (With<FloorLevelDownButton>, Changed<Interaction>)>,
    up: Query<&Interaction, (With<FloorLevelUpButton>, Changed<Interaction>)>,
    mut floor: ResMut<ActiveFloorLevel>,
) {
    for interaction in &down {
        if *interaction == Interaction::Pressed {
            floor.0 = floor.0.saturating_sub(1);
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

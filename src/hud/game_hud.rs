//! Bottom-screen HUD: semi-transparent controls wired to gameplay.

use bevy::picking::prelude::Pickable;
use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::actor::Paused;
use crate::scene::camera::{
    spawn_camera, StrategyCamera, StrategyCameraRig,
    StrategyCameraViewMode, STRATEGY_CAMERA_DEFAULT_PITCH, STRATEGY_CAMERA_MAP_PITCH,
};
use crate::edit::actor_spawn::{ActorSpawnToggleButton, ActorSpawnToggleLabel};
use crate::edit::map_edit::{MapEditToggleButton, MapEditToggleLabel};
use crate::map::dirt::DirtMap;
use crate::map::hypermap_world::{HypermapChunkRemeshQueue, HypermapRuntime};
use crate::map::temperature::TemperatureMap;
use crate::map::floor_level::{ActiveFloorLevel, HYPERMAP_FLOOR_MAX};
use crate::hud::panel_anim::PanelAnim;
use crate::menu::main_menu::GameState;

const BAR_BG: Color = Color::srgba(0.06, 0.07, 0.1, 0.62);
const BTN_BG: Color = Color::srgba(0.18, 0.2, 0.24, 0.55);
const BTN_HOVER: Color = Color::srgba(0.26, 0.29, 0.35, 0.72);
const BTN_PRESSED: Color = Color::srgba(0.10, 0.12, 0.16, 0.88);
const BTN_BORDER: Color = Color::srgba(0.9, 0.92, 0.96, 0.35);
const BTN_BORDER_HOVER: Color = Color::srgba(0.9, 0.92, 0.96, 0.65);
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
struct OverlaysToggleButton;

#[derive(Component)]
struct OverlaysToggleLabel;

#[derive(Component)]
struct RedrawAllButton;

#[derive(Component)]
struct PauseButton;

#[derive(Component)]
struct PauseButtonLabel;

#[derive(Component)]
struct PausedBanner;

#[derive(Component)]
struct FpsCounterText;

pub struct GameHudPlugin;

impl Plugin for GameHudPlugin {
    fn build(&self, app: &mut App) {
        // The HUD attaches itself to the strategy camera (`UiTargetCamera`),
        // so it must spawn after `spawn_camera`'s entity is on the world.
        // Without `.after`, both systems run in parallel inside `OnEnter` and
        // the HUD's `Query<&StrategyCameraRig>::single()` returns `Err`.
        app.add_systems(
            OnEnter(GameState::InGame),
            (
                spawn_bottom_hud.after(spawn_camera),
                spawn_paused_banner.after(spawn_camera),
                spawn_fps_counter.after(spawn_camera),
            ),
        )
        .add_systems(
            Update,
            (
                map_button_toggle_views,
                map_key_toggle_views,
                overlays_toggle_button,
                sync_overlays_toggle_label,
                redraw_all_button,
                floor_level_buttons,
                update_floor_level_readout,
                update_button_visuals,
                pause_button_click,
                sync_pause_ui,
                update_fps_counter,
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
            PanelAnim { progress: 0.0, target: 1.0, open_bottom: 0.0, panel_height: 52.0 },
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Px(52.0),
                bottom: Val::Px(-52.0),
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
                    Name::new("HUD overlays panel toggle"),
                    OverlaysToggleButton,
                    Button,
                    Node {
                        min_width: Val::Px(100.0),
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
                        OverlaysToggleLabel,
                        Text::new("Overlays"),
                        TextFont::from_font_size(17.0),
                        TextColor(TEXT_MAIN),
                    ));
                });

            parent
                .spawn((
                    Name::new("HUD redraw all"),
                    RedrawAllButton,
                    Button,
                    Node {
                        min_width: Val::Px(96.0),
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
                        Text::new("Redraw"),
                        TextFont::from_font_size(17.0),
                        TextColor(TEXT_MAIN),
                    ));
                });

            parent
                .spawn((
                    Name::new("HUD pause"),
                    PauseButton,
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
                        PauseButtonLabel,
                        Text::new("Pause"),
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

/// Forces every visible chunk's field overlay textures (dirt + heat) to repaint
/// and re-bakes the chunk meshes — a manual recovery for stale or out-of-sync
/// GPU textures. Marking a chunk dirty makes the overlay update systems repaint
/// it from current field data on the next frame.
fn redraw_all_button(
    interactions: Query<&Interaction, (With<RedrawAllButton>, Changed<Interaction>)>,
    runtime: Res<HypermapRuntime>,
    dirt: Res<DirtMap>,
    temperature: Res<TemperatureMap>,
    mut remesh: ResMut<HypermapChunkRemeshQueue>,
) {
    if !interactions.iter().any(|i| *i == Interaction::Pressed) {
        return;
    }
    for coord in runtime.desired_chunk_coords() {
        dirt.mark_dirty(coord);
        temperature.mark_dirty(coord);
        remesh.0.insert(coord);
    }
}

fn update_button_visuals(
    mut buttons: Query<
        (&Interaction, &mut BackgroundColor, &mut BorderColor),
        (Changed<Interaction>, With<Button>),
    >,
) {
    for (interaction, mut bg, mut border) in &mut buttons {
        match interaction {
            Interaction::Pressed => {
                *bg = BackgroundColor(BTN_PRESSED);
                *border = BorderColor::all(BTN_BORDER_HOVER);
            }
            Interaction::Hovered => {
                *bg = BackgroundColor(BTN_HOVER);
                *border = BorderColor::all(BTN_BORDER_HOVER);
            }
            Interaction::None => {
                *bg = BackgroundColor(BTN_BG);
                *border = BorderColor::all(BTN_BORDER);
            }
        }
    }
}

fn spawn_paused_banner(mut commands: Commands, camera: Query<Entity, With<StrategyCameraRig>>) {
    let Ok(cam) = camera.single() else {
        return;
    };

    const BANNER_BG: Color = Color::srgba(0.10, 0.08, 0.03, 0.90);
    const BANNER_BORDER: Color = Color::srgba(0.95, 0.75, 0.20, 0.60);
    const BANNER_TEXT: Color = Color::srgb(0.98, 0.85, 0.30);

    commands
        .spawn((
            Name::new("Paused banner"),
            PausedBanner,
            UiTargetCamera(cam),
            Pickable::IGNORE,
            Visibility::Hidden,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                top: Val::Px(18.0),
                justify_content: JustifyContent::Center,
                ..default()
            },
            ZIndex(2000),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(28.0), Val::Px(7.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(10.0)),
                    ..default()
                },
                BackgroundColor(BANNER_BG),
                BorderColor::all(BANNER_BORDER),
            ))
            .with_children(|pill| {
                pill.spawn((
                    Text::new("|| PAUSED"),
                    TextFont::from_font_size(16.0),
                    TextColor(BANNER_TEXT),
                ));
            });
        });
}

fn pause_button_click(
    interactions: Query<&Interaction, (With<PauseButton>, Changed<Interaction>)>,
    mut paused: ResMut<Paused>,
) {
    for interaction in &interactions {
        if *interaction == Interaction::Pressed {
            paused.0 = !paused.0;
        }
    }
}

fn sync_pause_ui(
    paused: Res<Paused>,
    mut banner: Query<&mut Visibility, With<PausedBanner>>,
    mut labels: Query<&mut Text, With<PauseButtonLabel>>,
) {
    if !paused.is_changed() {
        return;
    }
    let is_paused = paused.0;
    for mut vis in &mut banner {
        *vis = if is_paused { Visibility::Inherited } else { Visibility::Hidden };
    }
    let label = if is_paused { "Resume" } else { "Pause" };
    for mut text in &mut labels {
        **text = label.to_string();
    }
}

fn spawn_fps_counter(mut commands: Commands, camera: Query<Entity, With<StrategyCameraRig>>) {
    let Ok(cam) = camera.single() else {
        return;
    };

    commands.spawn((
        Name::new("FPS counter"),
        FpsCounterText,
        UiTargetCamera(cam),
        Pickable::IGNORE,
        Text::new("-- fps"),
        TextFont::from_font_size(14.0),
        TextColor(Color::srgba(0.85, 0.90, 0.95, 0.70)),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(10.0),
            right: Val::Px(12.0),
            ..default()
        },
        ZIndex(1500),
    ));
}

fn overlays_toggle_button(
    interactions: Query<&Interaction, (With<OverlaysToggleButton>, Changed<Interaction>)>,
    mut panel: ResMut<crate::hud::overlays::OverlaysPanel>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        panel.open = !panel.open;
    }
}

fn sync_overlays_toggle_label(
    panel: Res<crate::hud::overlays::OverlaysPanel>,
    mut texts: Query<&mut Text, With<OverlaysToggleLabel>>,
) {
    if !panel.is_changed() {
        return;
    }
    let label = if panel.open { "Overlays *" } else { "Overlays" };
    for mut text in &mut texts {
        **text = label.to_string();
    }
}

fn update_fps_counter(
    time: Res<Time>,
    mut last_fps: Local<u32>,
    mut query: Query<&mut Text, With<FpsCounterText>>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let fps = (1.0 / dt).round() as u32;
    // Only allocate and write when the displayed integer actually changes.
    if fps == *last_fps {
        return;
    }
    *last_fps = fps;
    for mut text in &mut query {
        **text = format!("{fps} fps");
    }
}

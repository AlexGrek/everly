//! Idiomatic main menu screen.
//!
//! Lists every level discovered on disk under `levels/level_*/` and lets the
//! player pick one. Choosing a level writes [`crate::map::level::LevelName`]
//! and transitions [`GameState`] to [`GameState::Loading`], where the strategy
//! camera and hypermap renderer spin up behind a loading overlay; once the
//! initial visible chunks are meshed, the app enters [`GameState::InGame`] and
//! the HUD plus editor systems wake up.
//!
//! The menu owns its own UI camera (a `Camera2d`), independent from the
//! gameplay strategy camera, so the two views never share entities.

use std::fs;
use std::path::PathBuf;

use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::map::level::{create_new_level_with_road_origin, pick_new_level_name, LevelName};

use super::loading_screen::LoadingScreenPlugin;

/// App-wide screen flag. `MainMenu` is the default so the player always lands
/// on the menu first; gameplay plugins gate their setup on `OnEnter(InGame)`.
#[derive(States, Default, Debug, Clone, Eq, PartialEq, Hash)]
pub enum GameState {
    #[default]
    MainMenu,
    /// World is bootstrapping and initial chunk meshes are baking/spawning.
    Loading,
    InGame,
}

/// True while the 3D world session is active (initial load or gameplay).
pub fn in_world_session(state: Res<State<GameState>>) -> bool {
    matches!(*state.get(), GameState::Loading | GameState::InGame)
}

const MENU_BG: Color = Color::srgb(0.04, 0.05, 0.07);
const PANEL_BG: Color = Color::srgba(0.09, 0.11, 0.15, 0.92);
const PANEL_BORDER: Color = Color::srgba(0.85, 0.88, 0.94, 0.18);
const TITLE_COLOR: Color = Color::srgb(0.95, 0.96, 0.99);
const SUBTITLE_COLOR: Color = Color::srgba(0.78, 0.82, 0.88, 0.78);
const BTN_BG: Color = Color::srgba(0.18, 0.21, 0.27, 0.95);
const BTN_BG_HOVER: Color = Color::srgba(0.27, 0.32, 0.4, 1.0);
const BTN_BG_PRESSED: Color = Color::srgba(0.36, 0.42, 0.5, 1.0);
const BTN_BORDER: Color = Color::srgba(0.9, 0.92, 0.96, 0.35);
const BTN_TEXT: Color = Color::srgb(0.94, 0.96, 0.98);
const NEW_BTN_BG: Color = Color::srgba(0.16, 0.36, 0.24, 0.95);
const NEW_BTN_BG_HOVER: Color = Color::srgba(0.22, 0.48, 0.32, 1.0);
const NEW_BTN_BG_PRESSED: Color = Color::srgba(0.3, 0.58, 0.4, 1.0);

/// Marker for everything spawned to render the main menu (root UI node + camera).
#[derive(Component)]
struct MainMenuEntity;

/// Marker on the dedicated 2D UI camera for the menu.
#[derive(Component)]
struct MainMenuCamera;

/// Marker carrying the level folder name (without the `level_` prefix) the
/// button will load when pressed.
#[derive(Component, Debug, Clone)]
struct LoadLevelButton(pub String);

/// "+ New level" button — auto-picks a fresh `new_NNN` name, writes a
/// road-only `0_0` chunk, sets the active level, and enters gameplay.
#[derive(Component, Debug, Clone)]
struct NewLevelButton;

/// Cached scan of `levels/level_*/`. Refreshed each time the menu is entered
/// so newly created levels show up without restarting the game.
#[derive(Resource, Default, Debug)]
pub struct AvailableLevels {
    pub names: Vec<String>,
}

pub struct MainMenuPlugin;

impl Plugin for MainMenuPlugin {
    fn build(&self, app: &mut App) {
        app.init_state::<GameState>()
            .init_resource::<AvailableLevels>()
            .add_plugins(LoadingScreenPlugin)
            .add_systems(
                OnEnter(GameState::MainMenu),
                (scan_available_levels, spawn_main_menu).chain(),
            )
            .add_systems(OnExit(GameState::MainMenu), despawn_main_menu)
            .add_systems(
                Update,
                (
                    main_menu_button_visuals,
                    main_menu_new_level_button_visuals,
                    main_menu_load_buttons,
                    main_menu_new_level_button,
                )
                    .run_if(in_state(GameState::MainMenu)),
            );
    }
}

/// Walks `levels/` for `level_*` subdirectories. Falls back to a single
/// `default` entry when the folder is missing or empty so the player can
/// always start a fresh sandbox.
fn scan_available_levels(mut available: ResMut<AvailableLevels>) {
    let mut names: Vec<String> = Vec::new();
    let levels_dir = PathBuf::from("levels");
    if let Ok(entries) = fs::read_dir(&levels_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(level_name) = file_name.strip_prefix("level_") {
                if !level_name.is_empty() {
                    names.push(level_name.to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    if names.is_empty() {
        names.push("default".to_string());
    }
    available.names = names;
}

fn spawn_main_menu(mut commands: Commands, available: Res<AvailableLevels>) {
    let cam = commands
        .spawn((
            Name::new("Main menu camera"),
            MainMenuEntity,
            MainMenuCamera,
            Camera2d,
        ))
        .id();

    commands
        .spawn((
            Name::new("Main menu root"),
            MainMenuEntity,
            UiTargetCamera(cam),
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: Val::Px(28.0),
                ..default()
            },
            BackgroundColor(MENU_BG),
        ))
        .with_children(|root| {
            root.spawn((
                Name::new("Main menu title"),
                Text::new("Everly"),
                TextFont::from_font_size(72.0),
                TextColor(TITLE_COLOR),
            ));
            root.spawn((
                Name::new("Main menu subtitle"),
                Text::new("Select a level"),
                TextFont::from_font_size(20.0),
                TextColor(SUBTITLE_COLOR),
            ));

            root.spawn((
                Name::new("Main menu level list panel"),
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Stretch,
                    row_gap: Val::Px(10.0),
                    padding: UiRect::axes(Val::Px(28.0), Val::Px(22.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    min_width: Val::Px(320.0),
                    max_height: Val::Percent(60.0),
                    overflow: Overflow::clip_y(),
                    ..default()
                },
                BorderColor::all(PANEL_BORDER),
                BackgroundColor(PANEL_BG),
            ))
            .with_children(|panel| {
                for name in &available.names {
                    spawn_level_button(panel, name);
                }
                spawn_new_level_button(panel);
            });
        });
}

fn spawn_level_button(parent: &mut ChildSpawnerCommands, level_name: &str) {
    parent
        .spawn((
            Name::new(format!("Main menu load `{level_name}`")),
            LoadLevelButton(level_name.to_string()),
            Button,
            Node {
                width: Val::Percent(100.0),
                min_width: Val::Px(260.0),
                height: Val::Px(44.0),
                padding: UiRect::horizontal(Val::Px(18.0)),
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
                Text::new(level_name.to_string()),
                TextFont::from_font_size(20.0),
                TextColor(BTN_TEXT),
            ));
        });
}

fn spawn_new_level_button(parent: &mut ChildSpawnerCommands) {
    parent
        .spawn((
            Name::new("Main menu new level"),
            NewLevelButton,
            Button,
            Node {
                width: Val::Percent(100.0),
                min_width: Val::Px(260.0),
                height: Val::Px(44.0),
                margin: UiRect::top(Val::Px(8.0)),
                padding: UiRect::horizontal(Val::Px(18.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(BTN_BORDER),
            BackgroundColor(NEW_BTN_BG),
        ))
        .with_children(|p| {
            p.spawn((
                Text::new("+ New level"),
                TextFont::from_font_size(20.0),
                TextColor(BTN_TEXT),
            ));
        });
}

fn despawn_main_menu(mut commands: Commands, q: Query<Entity, With<MainMenuEntity>>) {
    for e in &q {
        commands.entity(e).despawn();
    }
}

fn main_menu_button_visuals(
    mut buttons: Query<
        (&Interaction, &mut BackgroundColor),
        (With<LoadLevelButton>, Changed<Interaction>),
    >,
) {
    for (interaction, mut bg) in &mut buttons {
        bg.0 = match *interaction {
            Interaction::Pressed => BTN_BG_PRESSED,
            Interaction::Hovered => BTN_BG_HOVER,
            Interaction::None => BTN_BG,
        };
    }
}

fn main_menu_load_buttons(
    interactions: Query<
        (&Interaction, &LoadLevelButton),
        (Changed<Interaction>, With<Button>),
    >,
    mut next: ResMut<NextState<GameState>>,
    mut level: ResMut<LevelName>,
) {
    for (interaction, btn) in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        info!("loading level `{}`", btn.0);
        level.0 = btn.0.clone();
        next.set(GameState::Loading);
        return;
    }
}

fn main_menu_new_level_button_visuals(
    mut buttons: Query<
        (&Interaction, &mut BackgroundColor),
        (With<NewLevelButton>, Changed<Interaction>),
    >,
) {
    for (interaction, mut bg) in &mut buttons {
        bg.0 = match *interaction {
            Interaction::Pressed => NEW_BTN_BG_PRESSED,
            Interaction::Hovered => NEW_BTN_BG_HOVER,
            Interaction::None => NEW_BTN_BG,
        };
    }
}

fn main_menu_new_level_button(
    interactions: Query<&Interaction, (Changed<Interaction>, With<NewLevelButton>, With<Button>)>,
    available: Res<AvailableLevels>,
    mut next: ResMut<NextState<GameState>>,
    mut level: ResMut<LevelName>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let name = pick_new_level_name(&available.names);
        match create_new_level_with_road_origin(&name) {
            Ok(()) => {
                info!("created new level `{name}` (road-only origin chunk)");
                level.0 = name;
                next.set(GameState::Loading);
            }
            Err(e) => warn!("failed to create new level `{name}`: {e}"),
        }
        return;
    }
}

//! Full-screen overlay shown while the hypermap renderer bakes and spawns the
//! initial visible chunks after the player picks a level on the main menu.

use bevy::prelude::*;
use bevy::camera::ClearColorConfig;

use crate::map::hypermap_world::HypermapRuntime;
use crate::map::level::LevelName;
use crate::menu::main_menu::GameState;

const LOADING_BG: Color = Color::srgb(0.04, 0.05, 0.07);
const TITLE_COLOR: Color = Color::srgb(0.95, 0.96, 0.99);
const STATUS_COLOR: Color = Color::srgba(0.78, 0.82, 0.88, 0.78);
const PROGRESS_COLOR: Color = Color::srgba(0.62, 0.68, 0.76, 0.72);

/// Renders above the strategy camera while [`GameState::Loading`] is active.
const LOADING_CAMERA_ORDER: isize = 100;

#[derive(Component)]
struct LoadingScreenEntity;

#[derive(Component)]
struct LoadingScreenCamera;

#[derive(Component)]
struct LoadingStatusText;

#[derive(Component)]
struct LoadingProgressText;

pub struct LoadingScreenPlugin;

impl Plugin for LoadingScreenPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(OnEnter(GameState::Loading), spawn_loading_screen)
            .add_systems(OnExit(GameState::Loading), despawn_loading_screen)
            .add_systems(
                Update,
                (update_loading_progress, finish_loading_when_world_ready)
                    .chain()
                    .run_if(in_state(GameState::Loading)),
            );
    }
}

fn spawn_loading_screen(mut commands: Commands, level: Res<LevelName>) {
    let cam = commands
        .spawn((
            Name::new("Loading screen camera"),
            LoadingScreenEntity,
            LoadingScreenCamera,
            Camera2d,
            Camera {
                order: LOADING_CAMERA_ORDER,
                clear_color: ClearColorConfig::None,
                ..default()
            },
        ))
        .id();

    commands
        .spawn((
            Name::new("Loading screen root"),
            LoadingScreenEntity,
            UiTargetCamera(cam),
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: Val::Px(18.0),
                ..default()
            },
            BackgroundColor(LOADING_BG),
        ))
        .with_children(|root| {
            root.spawn((
                Name::new("Loading screen title"),
                Text::new("Everly"),
                TextFont::from_font_size(56.0),
                TextColor(TITLE_COLOR),
            ));
            root.spawn((
                Name::new("Loading screen status"),
                LoadingStatusText,
                Text::new(format!("Loading `{}`…", level.0)),
                TextFont::from_font_size(22.0),
                TextColor(STATUS_COLOR),
            ));
            root.spawn((
                Name::new("Loading screen progress"),
                LoadingProgressText,
                Text::new("Preparing world…"),
                TextFont::from_font_size(18.0),
                TextColor(PROGRESS_COLOR),
            ));
        });
}

fn despawn_loading_screen(mut commands: Commands, q: Query<Entity, With<LoadingScreenEntity>>) {
    for entity in &q {
        commands.entity(entity).despawn();
    }
}

fn update_loading_progress(
    runtime: Res<HypermapRuntime>,
    mut progress: Query<&mut Text, With<LoadingProgressText>>,
) {
    let Ok(mut text) = progress.single_mut() else {
        return;
    };
    let (loaded, total) = runtime.visible_chunks_spawned();
    if total == 0 {
        **text = "Preparing world…".into();
        return;
    }
    **text = format!("Rendering terrain… {loaded}/{total}");
}

fn finish_loading_when_world_ready(
    runtime: Res<HypermapRuntime>,
    mut next: ResMut<NextState<GameState>>,
) {
    if runtime.initial_visible_world_ready() {
        next.set(GameState::InGame);
    }
}

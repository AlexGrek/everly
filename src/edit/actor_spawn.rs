//! In-game actor spawner: pick an actor type, preview the target cell, spawn on click.
//!
//! Independent from the tile [`MapEditPlugin`](crate::edit::map_edit): its own HUD
//! toggle (`Actors`) and palette strip. The two brushes are mutually exclusive —
//! picking an actor clears any active tile brush and vice versa — so a single click
//! never both paints a tile and spawns an actor.

use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::actor::black_bot::{self, BlackBotRng};
use crate::actor::glitch_bot::{self, GlitchBotRng};
use crate::edit::map_edit::{
    ray_intersect_horizontal_plane, void_preview_plane, MapEditPaletteRoot, MapEditPreviewMaterial,
    MapEditState,
};
use crate::hud::panel_anim::PanelAnim;
use crate::map::floor_level::{ActiveFloorLevel, HYPERMAP_FLOOR_HEIGHT};
use crate::menu::main_menu::GameState;
use crate::scene::camera::StrategyCameraRig;

/// Pixels from the bottom of the window where spawn clicks are suppressed (covers the
/// 52 px HUD bar + the 40 px palette row this panel shares with the map-edit palette).
const ACTOR_DEAD_ZONE_PX: f32 = 120.0;

const PALETTE_BG: Color = Color::srgba(0.05, 0.06, 0.09, 0.78);
const BTN_BG: Color = Color::srgba(0.16, 0.18, 0.22, 0.75);
const BTN_BORDER: Color = Color::srgba(0.85, 0.88, 0.92, 0.4);
const TEXT_MAIN: Color = Color::srgba(0.94, 0.95, 0.97, 0.92);
const KILL_BTN_BG: Color = Color::srgba(0.22, 0.10, 0.10, 0.85);
const KILL_BTN_BORDER: Color = Color::srgba(0.75, 0.28, 0.28, 0.6);
const KILL_TEXT: Color = Color::srgb(0.95, 0.55, 0.55);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActorKind {
    GlitchBot,
    BlackBot,
}

/// Active brush in the actor palette: either spawn a kind on click, or kill the
/// clicked bot. `None` means no brush is armed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActorTool {
    Spawn(ActorKind),
    Kill,
}

/// HUD toggle (next to Edit) — wired in [`crate::hud::game_hud`].
#[derive(Component)]
pub struct ActorSpawnToggleButton;

#[derive(Component)]
pub(crate) struct ActorSpawnToggleLabel;

#[derive(Component)]
pub(crate) struct ActorSpawnPaletteRoot;

#[derive(Component, Clone, Copy)]
struct ActorSpawnPickButton(ActorTool);

#[derive(Component)]
struct ActorSpawnPreviewRoot;

#[derive(Resource, Default)]
pub struct ActorSpawnState {
    /// Palette + interactions enabled (Actors was pressed).
    pub panel_open: bool,
    /// Active brush; `None` = clicks do nothing.
    pub tool: Option<ActorTool>,
}

#[derive(Resource, Default)]
struct ActorSpawnHoverCell(Option<(i32, i32)>);

pub struct ActorSpawnPlugin;

impl Plugin for ActorSpawnPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ActorSpawnState>()
            .init_resource::<ActorSpawnHoverCell>()
            .add_systems(
                Update,
                (
                    sync_actor_spawn_toggle_label,
                    actor_spawn_toggle_panel,
                    actor_spawn_pick_buttons,
                    (
                        actor_spawn_hover_under_cursor,
                        actor_spawn_pointer_click,
                        actor_spawn_update_preview,
                    )
                        .chain(),
                    actor_spawn_right_click_cancel,
                )
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

pub(crate) fn spawn_actor_spawn_palette(
    mut commands: Commands,
    camera: Query<Entity, With<StrategyCameraRig>>,
) {
    let Ok(cam) = camera.single() else {
        return;
    };

    commands
        .spawn((
            Name::new("Actor spawn palette"),
            ActorSpawnPaletteRoot,
            PanelAnim::closed(52.0, 40.0),
            UiTargetCamera(cam),
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Px(40.0),
                bottom: Val::Px(12.0),
                left: Val::Px(0.0),
                padding: UiRect::horizontal(Val::Px(12.0)),
                column_gap: Val::Px(8.0),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::FlexStart,
                ..default()
            },
            BackgroundColor(PALETTE_BG),
            Visibility::Hidden,
            ZIndex(999),
        ))
        .with_children(|row| {
            for (label, tool) in [
                ("Bot", ActorTool::Spawn(ActorKind::GlitchBot)),
                ("Black", ActorTool::Spawn(ActorKind::BlackBot)),
                ("Kill", ActorTool::Kill),
            ] {
                let (bg, border, text) = match tool {
                    ActorTool::Kill => (KILL_BTN_BG, KILL_BTN_BORDER, KILL_TEXT),
                    ActorTool::Spawn(_) => (BTN_BG, BTN_BORDER, TEXT_MAIN),
                };
                row.spawn((
                    Name::new(format!("Actor spawn pick {label}")),
                    ActorSpawnPickButton(tool),
                    Button,
                    Node {
                        min_width: Val::Px(72.0),
                        height: Val::Px(32.0),
                        padding: UiRect::horizontal(Val::Px(12.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(5.0)),
                        ..default()
                    },
                    BorderColor::all(border),
                    BackgroundColor(bg),
                ))
                .with_children(|p| {
                    p.spawn((
                        Text::new(label),
                        TextFont::from_font_size(15.0),
                        TextColor(text),
                    ));
                });
            }
        });
}

fn sync_actor_spawn_toggle_label(
    state: Res<ActorSpawnState>,
    mut q: Query<&mut Text, With<ActorSpawnToggleLabel>>,
) {
    if !state.is_changed() {
        return;
    }
    let label = if state.panel_open { "Actors *" } else { "Actors" };
    for mut t in &mut q {
        **t = label.to_string();
    }
}

fn actor_spawn_toggle_panel(
    interactions: Query<&Interaction, (With<ActorSpawnToggleButton>, Changed<Interaction>)>,
    mut state: ResMut<ActorSpawnState>,
    mut palette: Query<&mut PanelAnim, (With<ActorSpawnPaletteRoot>, Without<MapEditPaletteRoot>)>,
    mut map_edit: ResMut<MapEditState>,
    mut map_palette: Query<&mut PanelAnim, (With<MapEditPaletteRoot>, Without<ActorSpawnPaletteRoot>)>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        state.panel_open = !state.panel_open;
        if !state.panel_open {
            state.tool = None;
        } else {
            map_edit.panel_open = false;
            map_edit.placement_tile = None;
            for mut anim in &mut map_palette {
                anim.target = 0.0;
            }
        }
        let target = if state.panel_open { 1.0 } else { 0.0 };
        for mut anim in &mut palette {
            anim.target = target;
        }
    }
}

fn actor_spawn_pick_buttons(
    interactions: Query<
        (&Interaction, &ActorSpawnPickButton),
        (Changed<Interaction>, With<Button>),
    >,
    mut state: ResMut<ActorSpawnState>,
    mut map_edit: ResMut<MapEditState>,
) {
    if !state.panel_open {
        return;
    }
    for (interaction, btn) in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        state.tool = Some(btn.0);
        // Mutually exclusive with the tile brush (see module docs).
        map_edit.placement_tile = None;
    }
}

fn actor_spawn_hover_under_cursor(
    state: Res<ActorSpawnState>,
    mut hover: ResMut<ActorSpawnHoverCell>,
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform), With<StrategyCameraRig>>,
    floor: Res<ActiveFloorLevel>,
) {
    if !matches!(state.tool, Some(ActorTool::Spawn(_))) {
        if hover.0.is_some() {
            hover.0 = None;
        }
        return;
    }
    let Ok(window) = windows.single() else {
        hover.0 = None;
        return;
    };
    let Ok((cam, cam_gt)) = cameras.single() else {
        hover.0 = None;
        return;
    };
    hover.0 = actor_spawn_plane_cell(window, cam, cam_gt, floor.0);
}

fn actor_spawn_pointer_click(
    mut commands: Commands,
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform), With<StrategyCameraRig>>,
    state: Res<ActorSpawnState>,
    floor: Res<ActiveFloorLevel>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut bot_rng: ResMut<GlitchBotRng>,
    mut black_rng: ResMut<BlackBotRng>,
) {
    let Some(ActorTool::Spawn(kind)) = state.tool else {
        return;
    };
    if !mouse.just_released(MouseButton::Left) {
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let Ok((cam, cam_gt)) = cameras.single() else {
        return;
    };
    let Some((cx, cz)) = actor_spawn_plane_cell(window, cam, cam_gt, floor.0) else {
        return;
    };

    let center = Vec2::new(cx as f32 + 0.5, cz as f32 + 0.5);
    match kind {
        ActorKind::GlitchBot => {
            glitch_bot::spawn_glitch_bot(
                &mut commands,
                &mut meshes,
                &mut materials,
                &mut bot_rng.0,
                center,
            );
        }
        ActorKind::BlackBot => {
            black_bot::spawn_black_bot(
                &mut commands,
                &mut meshes,
                &mut materials,
                &mut black_rng.0,
                center,
            );
        }
    }
}

fn actor_spawn_right_click_cancel(
    mouse: Res<ButtonInput<MouseButton>>,
    mut state: ResMut<ActorSpawnState>,
) {
    if mouse.just_pressed(MouseButton::Right) {
        state.tool = None;
    }
}

fn actor_spawn_update_preview(
    mut commands: Commands,
    state: Res<ActorSpawnState>,
    hover: Res<ActorSpawnHoverCell>,
    floor: Res<ActiveFloorLevel>,
    mut meshes: ResMut<Assets<Mesh>>,
    preview_mat: Option<Res<MapEditPreviewMaterial>>,
    mut preview_entity: Local<Option<Entity>>,
) {
    let Some(preview_mat) = preview_mat else {
        return;
    };

    let spawning = matches!(state.tool, Some(ActorTool::Spawn(_)));
    let Some((cx, cz)) = spawning.then_some(hover.0).flatten() else {
        if let Some(e) = *preview_entity {
            commands.entity(e).insert(Visibility::Hidden);
        }
        return;
    };

    let Some(mesh) = void_preview_plane() else {
        return;
    };
    let mesh_h = meshes.add(mesh);
    let transform = Transform::from_xyz(
        cx as f32 + 0.5,
        floor.0 as f32 * HYPERMAP_FLOOR_HEIGHT + 0.02,
        cz as f32 + 0.5,
    );

    if let Some(e) = *preview_entity {
        commands.entity(e).insert((
            Mesh3d(mesh_h),
            MeshMaterial3d(preview_mat.0.clone()),
            transform,
            Visibility::Inherited,
            NotShadowCaster,
            NotShadowReceiver,
        ));
    } else {
        let e = commands
            .spawn((
                Name::new("Actor spawn preview"),
                ActorSpawnPreviewRoot,
                Mesh3d(mesh_h),
                MeshMaterial3d(preview_mat.0.clone()),
                transform,
                Visibility::Inherited,
                NotShadowCaster,
                NotShadowReceiver,
            ))
            .id();
        *preview_entity = Some(e);
    }
}

fn actor_spawn_cursor_ok(window: &Window) -> bool {
    let Some(cursor) = window.cursor_position() else {
        return false;
    };
    cursor.y <= window.height() - ACTOR_DEAD_ZONE_PX
}

fn actor_spawn_plane_cell(
    window: &Window,
    cam: &Camera,
    cam_gt: &GlobalTransform,
    floor_idx: i32,
) -> Option<(i32, i32)> {
    if !actor_spawn_cursor_ok(window) {
        return None;
    }
    let cursor = window.cursor_position()?;
    let ray = cam.viewport_to_world(cam_gt, cursor).ok()?;
    let plane_y = floor_idx as f32 * HYPERMAP_FLOOR_HEIGHT;
    let hit = ray_intersect_horizontal_plane(ray, plane_y)?;
    Some((hit.x.floor() as i32, hit.z.floor() as i32))
}

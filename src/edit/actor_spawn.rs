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
use crate::actor::resurrect::{resurrect_all_button, ResurrectAllButton};
use crate::edit::map_edit::{
    ray_intersect_horizontal_plane, void_preview_plane, MapEditPaletteRoot, MapEditPreviewMaterial,
    MapEditState,
};
use crate::hud::panel_anim::PanelAnim;
use crate::map::floor_level::{ActiveFloorLevel, HYPERMAP_FLOOR_HEIGHT};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::hypermap::Hypermap;
use crate::map::passability::{
    DynamicPassabilityMap, SubtilePassability, FLAG_BLOCKED, FLAG_VOID, SUBTILE_COUNT,
};
use crate::menu::main_menu::GameState;
use crate::scene::camera::{StrategyCamera, StrategyCameraRig};

/// Pixels from the bottom of the window where spawn clicks are suppressed (covers the
/// 52 px HUD bar + the 40 px palette row this panel shares with the map-edit palette).
const ACTOR_DEAD_ZONE_PX: f32 = 120.0;

/// Spawnable bots use radius-2 subtiles (see `BLACK_RADIUS_SUBTILES`).
const ACTOR_SPAWN_RADIUS_SUBTILES: i32 = 2;

const PALETTE_BG: Color = Color::srgba(0.05, 0.06, 0.09, 0.78);
const BTN_BG: Color = Color::srgba(0.16, 0.18, 0.22, 0.75);
const BTN_BORDER: Color = Color::srgba(0.85, 0.88, 0.92, 0.4);
const TEXT_MAIN: Color = Color::srgba(0.94, 0.95, 0.97, 0.92);
const KILL_BTN_BG: Color = Color::srgba(0.22, 0.10, 0.10, 0.85);
const KILL_BTN_BORDER: Color = Color::srgba(0.75, 0.28, 0.28, 0.6);
const KILL_TEXT: Color = Color::srgb(0.95, 0.55, 0.55);
const RESURRECT_BTN_BG: Color = Color::srgba(0.10, 0.22, 0.14, 0.85);
const RESURRECT_BTN_BORDER: Color = Color::srgba(0.35, 0.78, 0.45, 0.6);
const RESURRECT_TEXT: Color = Color::srgb(0.55, 0.95, 0.65);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActorKind {
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

#[derive(Resource)]
struct ActorSpawnPreviewInvalidMaterial(Handle<StandardMaterial>);

pub struct ActorSpawnPlugin;

impl Plugin for ActorSpawnPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ActorSpawnState>()
            .init_resource::<ActorSpawnHoverCell>()
            .add_systems(OnEnter(GameState::InGame), setup_actor_spawn_preview_material)
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
                    resurrect_all_button,
                    bulk_spawn_bots,
                )
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

/// How many bots a bulk spawn drops per trigger (button or **Shift+B**).
const BULK_SPAWN_COUNT: usize = 100;
/// Cap on the outward cell-scan radius so a boxed-in camera can't loop forever.
const BULK_SPAWN_SCAN_RADIUS: i32 = 80;

const BULK_BTN_BG: Color = Color::srgba(0.10, 0.16, 0.24, 0.85);
const BULK_BTN_BORDER: Color = Color::srgba(0.35, 0.55, 0.85, 0.6);
const BULK_TEXT: Color = Color::srgb(0.6, 0.78, 0.98);

/// Marker on the "Spawn 100" palette button.
#[derive(Component)]
struct BulkSpawnButton;

/// Triggers a bulk spawn from either the **Shift+B** keybind or the
/// "Spawn 100" button. Drops [`BULK_SPAWN_COUNT`] BlackBots on passable cells
/// spiralling out from the camera focus — for testing the movement pipeline at
/// scale (hundreds of bots) without hundreds of clicks.
fn bulk_spawn_bots(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Query<&Interaction, (With<BulkSpawnButton>, Changed<Interaction>)>,
    cameras: Query<&StrategyCamera>,
    dynamic_passability: Res<DynamicPassabilityMap>,
    hypermap: Res<HypermapRuntime>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut black_rng: ResMut<BlackBotRng>,
) {
    let shift = keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]);
    let key_trigger = shift && keys.just_pressed(KeyCode::KeyB);
    let button_trigger = buttons.iter().any(|i| *i == Interaction::Pressed);
    if !(key_trigger || button_trigger) {
        return;
    }
    let Ok(camera) = cameras.single() else { return };
    let static_cache = hypermap.static_subtile_cache.as_ref();
    let origin_x = camera.focus.x.floor() as i32;
    let origin_z = camera.focus.z.floor() as i32;

    let mut spawned = 0usize;
    // Outward square rings from the focus tile; spawn on each passable cell.
    'scan: for ring in 0..=BULK_SPAWN_SCAN_RADIUS {
        for dz in -ring..=ring {
            for dx in -ring..=ring {
                // Only the perimeter of this ring (interior was covered already).
                if ring > 0 && dx.abs() != ring && dz.abs() != ring {
                    continue;
                }
                let (cx, cz) = (origin_x + dx, origin_z + dz);
                if !actor_spawn_cell_passable(
                    ActorKind::BlackBot,
                    cx,
                    cz,
                    &dynamic_passability,
                    static_cache,
                ) {
                    continue;
                }
                let center = Vec2::new(cx as f32 + 0.5, cz as f32 + 0.5);
                black_bot::spawn_black_bot(
                    &mut commands,
                    &mut meshes,
                    &mut materials,
                    &mut black_rng.0,
                    center,
                );
                spawned += 1;
                if spawned >= BULK_SPAWN_COUNT {
                    break 'scan;
                }
            }
        }
    }
    info!("bulk spawn: {spawned} BlackBots near ({origin_x}, {origin_z})");
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
                ("Bot", ActorTool::Spawn(ActorKind::BlackBot)),
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
            row.spawn((
                Name::new("Actor spawn resurrect all"),
                ResurrectAllButton,
                Button,
                Node {
                    min_width: Val::Px(108.0),
                    height: Val::Px(32.0),
                    padding: UiRect::horizontal(Val::Px(12.0)),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(5.0)),
                    ..default()
                },
                BorderColor::all(RESURRECT_BTN_BORDER),
                BackgroundColor(RESURRECT_BTN_BG),
            ))
            .with_children(|p| {
                p.spawn((
                    Text::new("Resurrect all"),
                    TextFont::from_font_size(15.0),
                    TextColor(RESURRECT_TEXT),
                ));
            });
            row.spawn((
                Name::new("Actor spawn bulk 100"),
                BulkSpawnButton,
                Button,
                Node {
                    min_width: Val::Px(96.0),
                    height: Val::Px(32.0),
                    padding: UiRect::horizontal(Val::Px(12.0)),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(5.0)),
                    ..default()
                },
                BorderColor::all(BULK_BTN_BORDER),
                BackgroundColor(BULK_BTN_BG),
            ))
            .with_children(|p| {
                p.spawn((
                    Text::new("Spawn 100"),
                    TextFont::from_font_size(15.0),
                    TextColor(BULK_TEXT),
                ));
            });
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
    dynamic_passability: Res<DynamicPassabilityMap>,
    hypermap: Res<HypermapRuntime>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
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
    if !actor_spawn_cell_passable(
        kind,
        cx,
        cz,
        &dynamic_passability,
        hypermap.static_subtile_cache.as_ref(),
    ) {
        return;
    }

    let center = Vec2::new(cx as f32 + 0.5, cz as f32 + 0.5);
    match kind {
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
    dynamic_passability: Res<DynamicPassabilityMap>,
    hypermap: Res<HypermapRuntime>,
    mut meshes: ResMut<Assets<Mesh>>,
    preview_mat: Option<Res<MapEditPreviewMaterial>>,
    invalid_preview_mat: Option<Res<ActorSpawnPreviewInvalidMaterial>>,
    mut preview_entity: Local<Option<Entity>>,
) {
    let (Some(preview_mat), Some(invalid_preview_mat)) = (preview_mat, invalid_preview_mat) else {
        return;
    };

    let kind = match state.tool {
        Some(ActorTool::Spawn(k)) => k,
        _ => {
            if let Some(e) = *preview_entity {
                commands.entity(e).insert(Visibility::Hidden);
            }
            return;
        }
    };
    let Some((cx, cz)) = hover.0 else {
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
    let passable = actor_spawn_cell_passable(
        kind,
        cx,
        cz,
        &dynamic_passability,
        hypermap.static_subtile_cache.as_ref(),
    );
    let mat = if passable {
        preview_mat.0.clone()
    } else {
        invalid_preview_mat.0.clone()
    };

    if let Some(e) = *preview_entity {
        commands.entity(e).insert((
            Mesh3d(mesh_h),
            MeshMaterial3d(mat),
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
                MeshMaterial3d(mat),
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

fn actor_spawn_blocked_flags(kind: ActorKind) -> u64 {
    match kind {
        ActorKind::BlackBot => FLAG_BLOCKED | FLAG_VOID,
    }
}

fn actor_spawn_center_subtile(cx: i32, cz: i32) -> IVec2 {
    let sc = SUBTILE_COUNT as f32;
    IVec2::new(
        ((cx as f32 + 0.5) * sc).floor() as i32,
        ((cz as f32 + 0.5) * sc).floor() as i32,
    )
}

fn actor_spawn_cell_passable(
    kind: ActorKind,
    cx: i32,
    cz: i32,
    dynamic: &DynamicPassabilityMap,
    static_cache: &Hypermap<SubtilePassability>,
) -> bool {
    dynamic
        .probe_footprint(
            actor_spawn_center_subtile(cx, cz),
            ACTOR_SPAWN_RADIUS_SUBTILES,
            None,
            actor_spawn_blocked_flags(kind),
            static_cache,
        )
        .is_ok()
}

fn setup_actor_spawn_preview_material(
    mut commands: Commands,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let h = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 0.28, 0.28, 0.42),
        emissive: LinearRgba::BLACK,
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        depth_bias: 1.0,
        ..Default::default()
    });
    commands.insert_resource(ActorSpawnPreviewInvalidMaterial(h));
}

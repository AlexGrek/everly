//! In-game tile edit mode: pick a tile type, preview strokes, paint on mouse up.

use std::collections::HashSet;

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::map::floor_level::{ActiveFloorLevel, HYPERMAP_FLOOR_HEIGHT, HYPERMAP_FLOOR_MAX};
use crate::map::hypermap::{world_to_chunk_local, ChunkCoord};
use crate::map::hypermap_world::{
    build_floor0_road_mesh, build_floor0_wall_mesh, build_upper_road_mesh, build_upper_wall_mesh,
    ensure_chunk_generated, queue_hypermap_chunk_remesh, write_world_cell,
    write_world_floor_style, write_world_wall_style,
    HypermapChunkRemeshQueue, HypermapRuntime,
};
use crate::map::level::{
    save_level_geometry_for_chunks, save_level_floor_style_for_chunks,
    save_level_wall_style_for_chunks, LevelName,
};
use crate::actor::glitch_bot::GlitchBotVisual;
use crate::actor::black_bot::BlackBotVisual;
use crate::actor::snapshot::{save_level_actors, LevelActorsFile};
use crate::scene::camera::{StrategyCamera, StrategyCameraRig};
use crate::scene::camera_snapshot::{save_level_camera, LevelCameraFile};
use crate::actor::ActorObject;
use crate::map::world_map::{
    CellType, TileStyle, WallCorner, WallMask, MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST,
};
use crate::menu::main_menu::GameState;
use crate::actor::black_bot::{self, BlackBotRng};
use crate::actor::glitch_bot::{self, GlitchBotRng};

/// Pixels from the bottom of the window where raycasting and clicks are suppressed
/// (covers the 52 px HUD bar + 40 px palette row + a small margin).
const HUD_DEAD_ZONE_PX: f32 = 120.0;

const PALETTE_BG: Color = Color::srgba(0.05, 0.06, 0.09, 0.78);
const BTN_BG: Color = Color::srgba(0.16, 0.18, 0.22, 0.75);
const BTN_BORDER: Color = Color::srgba(0.85, 0.88, 0.92, 0.4);
const TEXT_MAIN: Color = Color::srgba(0.94, 0.95, 0.97, 0.92);

/// HUD toggle (next to Map) — wired in [`crate::game_hud`].
#[derive(Component)]
pub struct MapEditToggleButton;

#[derive(Component)]
struct MapEditPaletteRoot;

#[derive(Component, Clone, Copy)]
struct MapEditTilePickButton(MapTileKind);

#[derive(Component)]
struct MapEditSaveButton;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapTileKind {
    Void,
    Road,
    Wall,
    /// Glass wall: same geometry as [`Wall`] but rendered with the glass material.
    WallGlass,
    /// Drag a rectangle; on mouse up, place walls only on the **border** with masks facing outward (closed loop).
    Room,
    Corner,
    /// Places a GlitchBot actor at the clicked tile (single-click, no drag).
    GlitchBot,
    /// Places a BlackBot actor at the clicked tile (single-click, no drag).
    BlackBot,
}

#[derive(Resource, Default)]
pub struct MapEditState {
    /// Palette + interactions enabled (Edit was pressed).
    pub panel_open: bool,
    /// Active placement brush; `None` = choosing another tile from palette.
    pub placement_tile: Option<MapTileKind>,
}

#[derive(Resource, Default)]
struct MapEditVariantIndex(pub u32);

#[derive(Resource, Default)]
struct MapEditStyleState {
    pub floor: u32,
    pub wall: u32,
}

#[derive(Resource, Default)]
struct MapEditHoverCell(pub Option<(i32, i32)>);

/// Active floor cell when the user pressed the left mouse button for a stroke (wall line / floor rect).
#[derive(Resource, Default)]
struct MapEditDragAnchor(pub Option<(i32, i32)>);

#[derive(Component)]
struct MapEditPreviewRoot;

#[derive(Component)]
struct MapEditStyleInfoLabel;

pub struct MapEditPlugin;

impl Plugin for MapEditPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MapEditState>()
            .init_resource::<MapEditVariantIndex>()
            .init_resource::<MapEditStyleState>()
            .init_resource::<MapEditHoverCell>()
            .init_resource::<MapEditDragAnchor>()
            .add_systems(OnEnter(GameState::InGame), setup_map_edit_preview_material)
            .add_systems(
                Update,
                (
                    sync_map_edit_toggle_button_label,
                    map_edit_toggle_panel,
                    map_edit_tile_pick_buttons,
                    map_edit_save_button,
                    map_edit_tab_cycle_style,
                    map_edit_sync_style_label,
                    (
                        map_edit_hover_under_cursor,
                        map_edit_pointer_stroke,
                        map_edit_update_preview,
                    )
                        .chain(),
                    map_edit_right_click_cancel_placement,
                    map_edit_scroll_variants,
                )
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

pub(crate) fn spawn_map_edit_palette(mut commands: Commands, camera: Query<Entity, With<StrategyCameraRig>>) {
    let Ok(cam) = camera.single() else {
        return;
    };

    commands
        .spawn((
            Name::new("Map edit palette"),
            MapEditPaletteRoot,
            UiTargetCamera(cam),
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Px(40.0),
                bottom: Val::Px(52.0),
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
            for (label, kind) in [
                ("Void", MapTileKind::Void),
                ("Road", MapTileKind::Road),
                ("Wall", MapTileKind::Wall),
                ("WallG", MapTileKind::WallGlass),
                ("Room", MapTileKind::Room),
                ("Corner", MapTileKind::Corner),
                ("Bot", MapTileKind::GlitchBot),
                ("Black", MapTileKind::BlackBot),
            ] {
                row.spawn((
                    Name::new(format!("Map edit pick {label}")),
                    MapEditTilePickButton(kind),
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
                    BorderColor::all(BTN_BORDER),
                    BackgroundColor(BTN_BG),
                ))
                .with_children(|p| {
                    p.spawn((
                        Text::new(label),
                        TextFont::from_font_size(15.0),
                        TextColor(TEXT_MAIN),
                    ));
                });
            }
            row.spawn((
                Name::new("Map edit save"),
                MapEditSaveButton,
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
                BorderColor::all(BTN_BORDER),
                BackgroundColor(BTN_BG),
            ))
            .with_children(|p| {
                p.spawn((
                    Text::new("Save"),
                    TextFont::from_font_size(15.0),
                    TextColor(TEXT_MAIN),
                ));
            });

            // Style indicator — updated by `map_edit_sync_style_label`.
            row.spawn((
                Name::new("Map edit style info"),
                MapEditStyleInfoLabel,
                Node {
                    margin: UiRect::left(Val::Px(16.0)),
                    ..default()
                },
                Text::new(""),
                TextFont::from_font_size(14.0),
                TextColor(Color::srgba(0.70, 0.85, 1.00, 0.80)),
            ));
        });
}

fn sync_map_edit_toggle_button_label(
    state: Res<MapEditState>,
    mut q: Query<&mut Text, With<MapEditToggleLabel>>,
) {
    if !state.is_changed() {
        return;
    }
    let label = if state.panel_open {
        "Edit *"
    } else {
        "Edit"
    };
    for mut t in &mut q {
        **t = label.to_string();
    }
}

#[derive(Component)]
pub(crate) struct MapEditToggleLabel;

fn map_edit_toggle_panel(
    interactions: Query<&Interaction, (With<MapEditToggleButton>, Changed<Interaction>)>,
    mut state: ResMut<MapEditState>,
    mut drag: ResMut<MapEditDragAnchor>,
    mut palette: Query<&mut Visibility, With<MapEditPaletteRoot>>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        state.panel_open = !state.panel_open;
        if !state.panel_open {
            state.placement_tile = None;
            drag.0 = None;
        }
        for mut vis in &mut palette {
            *vis = if state.panel_open {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            };
        }
    }
}

fn map_edit_save_button(
    interactions: Query<&Interaction, (With<MapEditSaveButton>, Changed<Interaction>)>,
    state: Res<MapEditState>,
    level: Res<LevelName>,
    runtime: Res<HypermapRuntime>,
    camera: Query<&StrategyCamera, With<StrategyCameraRig>>,
    glitch_bots: Query<(&ActorObject, &GlitchBotVisual, Option<&Name>)>,
    black_bots: Query<(&ActorObject, &BlackBotVisual, Option<&Name>)>,
) {
    if !state.panel_open {
        return;
    }
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let level_name = level.0.as_str();
        match save_level_geometry_for_chunks(level_name, &runtime.map, runtime.desired_chunk_coords()) {
            Ok(n) => info!(
                "saved {n} rendered chunk geometry file(s) under `levels/level_{level_name}/geometry/`",
            ),
            Err(e) => warn!("save level geometry failed: {e}"),
        }
        let actors_file = LevelActorsFile::collect(&glitch_bots, &black_bots);
        match save_level_actors(level_name, &actors_file) {
            Ok(()) => info!(
                "saved {} actor(s) to `levels/level_{level_name}/actors.json`",
                actors_file.actors.len()
            ),
            Err(e) => warn!("save level actors failed: {e}"),
        }
        if let Ok(cam) = camera.single() {
            let camera_file = LevelCameraFile::from_camera(cam);
            match save_level_camera(level_name, &camera_file) {
                Ok(()) => info!("saved strategy camera to `levels/level_{level_name}/camera.json`"),
                Err(e) => warn!("save level camera failed: {e}"),
            }
        }
        let coords = runtime.desired_chunk_coords();
        match save_level_floor_style_for_chunks(level.0.as_str(), &runtime.style_floor_map, coords.clone()) {
            Ok(n) => info!("saved {n} chunk floor style file(s) for level `{}`", level.0),
            Err(e) => warn!("save level floor style failed: {e}"),
        }
        match save_level_wall_style_for_chunks(level.0.as_str(), &runtime.style_wall_map, coords) {
            Ok(n) => info!("saved {n} chunk wall style file(s) for level `{}`", level.0),
            Err(e) => warn!("save level wall style failed: {e}"),
        }
    }
}

fn map_edit_tile_pick_buttons(
    interactions: Query<
        (&Interaction, &MapEditTilePickButton),
        (Changed<Interaction>, With<Button>),
    >,
    mut state: ResMut<MapEditState>,
    mut variant: ResMut<MapEditVariantIndex>,
    mut style: ResMut<MapEditStyleState>,
    mut drag_anchor: ResMut<MapEditDragAnchor>,
) {
    if !state.panel_open {
        return;
    }
    for (interaction, btn) in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        state.placement_tile = Some(btn.0);
        variant.0 = 0;
        style.floor = 0;
        style.wall = 0;
        drag_anchor.0 = None;
    }
}

fn map_edit_hover_under_cursor(
    state: Res<MapEditState>,
    mut hover: ResMut<MapEditHoverCell>,
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform), With<StrategyCameraRig>>,
    floor: Res<ActiveFloorLevel>,
) {
    if state.placement_tile.is_none() {
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

    hover.0 = map_edit_plane_cell(&window, &cam, &cam_gt, floor.0);
}

fn map_edit_cursor_ok_for_paint(window: &Window) -> bool {
    let Some(cursor) = window.cursor_position() else {
        return false;
    };
    let h = window.height();
    cursor.y <= h - HUD_DEAD_ZONE_PX
}

fn map_edit_plane_cell(
    window: &Window,
    cam: &Camera,
    cam_gt: &GlobalTransform,
    floor_idx: i32,
) -> Option<(i32, i32)> {
    if !map_edit_cursor_ok_for_paint(window) {
        return None;
    }
    let cursor = window.cursor_position()?;
    let ray = cam.viewport_to_world(cam_gt, cursor).ok()?;
    let plane_y = floor_idx as f32 * HYPERMAP_FLOOR_HEIGHT;
    let hit = ray_intersect_horizontal_plane(ray, plane_y)?;
    Some((hit.x.floor() as i32, hit.z.floor() as i32))
}

/// Wall stroke along one axis: larger `|Δ|` picks the axis; constant `z` from anchor when `|Δx| > |Δz|`, else constant `x`. Equal nonzero `|Δ|` → horizontal segment at anchor `z`.
fn wall_line_cells(start: (i32, i32), end: (i32, i32)) -> Vec<(i32, i32)> {
    let (sx, sz) = start;
    let (ex, ez) = end;
    let dx = ex - sx;
    let dz = ez - sz;
    let adx = dx.abs();
    let adz = dz.abs();
    if adx == 0 && adz == 0 {
        return vec![(sx, sz)];
    }
    if adx > adz {
        let x0 = sx.min(ex);
        let x1 = sx.max(ex);
        (x0..=x1).map(|x| (x, sz)).collect()
    } else if adz > adx {
        let z0 = sz.min(ez);
        let z1 = sz.max(ez);
        (z0..=z1).map(|z| (sx, z)).collect()
    } else {
        let x0 = sx.min(ex);
        let x1 = sx.max(ex);
        (x0..=x1).map(|x| (x, sz)).collect()
    }
}

fn rect_axis_bounds(a: (i32, i32), b: (i32, i32)) -> (i32, i32, i32, i32) {
    let (ax, az) = a;
    let (bx, bz) = b;
    let min_x = ax.min(bx);
    let max_x = ax.max(bx);
    let min_z = az.min(bz);
    let max_z = az.max(bz);
    (min_x, max_x, min_z, max_z)
}

/// Cells on the axis-aligned border of the rectangle from `start` to `end` (inclusive).
fn room_outline_cells(start: (i32, i32), end: (i32, i32)) -> Vec<(i32, i32)> {
    let (min_x, max_x, min_z, max_z) = rect_axis_bounds(start, end);
    if min_x == max_x && min_z == max_z {
        return vec![(min_x, min_z)];
    }
    if min_z == max_z {
        return (min_x..=max_x).map(|x| (x, min_z)).collect();
    }
    if min_x == max_x {
        return (min_z..=max_z).map(|z| (min_x, z)).collect();
    }
    let mut out = Vec::new();
    for x in min_x..=max_x {
        out.push((x, min_z));
        out.push((x, max_z));
    }
    for z in (min_z + 1)..=(max_z - 1) {
        out.push((min_x, z));
        out.push((max_x, z));
    }
    out
}

/// Wall mask for a cell on the rectangle border: each edge of the cell that lies on the outer boundary of the selection.
///
/// Must match [`for_each_wall_segment`](crate::map::world_map::for_each_wall_segment): **north** bit draws
/// toward **-world Z** (`oz = -inset`), **south** toward **+world Z** — so the +Z side of the loop uses
/// [`MASK_SOUTH`] and the -Z side uses [`MASK_NORTH`].
fn perimeter_wall_mask(cx: i32, cz: i32, min_x: i32, max_x: i32, min_z: i32, max_z: i32) -> WallMask {
    let mut bits = 0u8;
    if cz == max_z {
        bits |= MASK_SOUTH;
    }
    if cz == min_z {
        bits |= MASK_NORTH;
    }
    if cx == max_x {
        bits |= MASK_EAST;
    }
    if cx == min_x {
        bits |= MASK_WEST;
    }
    WallMask::from_bits(bits).expect("border cell has at least one outer edge")
}

fn floor_rect_cells(start: (i32, i32), end: (i32, i32)) -> Vec<(i32, i32)> {
    let (sx, sz) = start;
    let (ex, ez) = end;
    let x0 = sx.min(ex);
    let x1 = sx.max(ex);
    let z0 = sz.min(ez);
    let z1 = sz.max(ez);
    let mut out = Vec::new();
    for x in x0..=x1 {
        for z in z0..=z1 {
            out.push((x, z));
        }
    }
    out
}

fn stroke_world_cells(kind: MapTileKind, start: (i32, i32), end: (i32, i32)) -> Vec<(i32, i32)> {
    match kind {
        MapTileKind::Wall | MapTileKind::WallGlass => wall_line_cells(start, end),
        MapTileKind::Void | MapTileKind::Road => floor_rect_cells(start, end),
        MapTileKind::Room => room_outline_cells(start, end),
        MapTileKind::Corner => vec![end],
        MapTileKind::GlitchBot | MapTileKind::BlackBot => vec![end],
    }
}

fn preview_stroke_cells(kind: MapTileKind, anchor: Option<(i32, i32)>, hover: Option<(i32, i32)>) -> Vec<(i32, i32)> {
    match (anchor, hover) {
        (None, None) => Vec::new(),
        (None, Some(h)) => vec![h],
        (Some(s), None) => vec![s],
        (Some(s), Some(h)) => stroke_world_cells(kind, s, h),
    }
}

fn ray_intersect_horizontal_plane(ray: Ray3d, plane_y: f32) -> Option<Vec3> {
    let n = Vec3::Y;
    let dir = Vec3::from(*ray.direction);
    let denom = dir.dot(n);
    if denom.abs() < 1e-5 {
        return None;
    }
    let t = (Vec3::new(0.0, plane_y, 0.0) - ray.origin).dot(n) / denom;
    if t < 0.0 {
        return None;
    }
    Some(ray.origin + dir * t)
}

fn resolved_cell(kind: MapTileKind, variant: u32) -> CellType {
    match kind {
        MapTileKind::Void => CellType::Void,
        MapTileKind::Road => CellType::Road,
        MapTileKind::Wall | MapTileKind::WallGlass => {
            let bits = ((variant % 15) + 1) as u8;
            CellType::Wall(WallMask::from_bits(bits).expect("1..=15 valid"))
        }
        MapTileKind::Room => CellType::Road,
        MapTileKind::Corner => {
            let c = match variant % 4 {
                0 => WallCorner::Nw,
                1 => WallCorner::Ne,
                2 => WallCorner::Sw,
                _ => WallCorner::Se,
            };
            CellType::Corner(c)
        }
        MapTileKind::GlitchBot | MapTileKind::BlackBot => CellType::Road,
    }
}

fn map_edit_update_preview(
    mut commands: Commands,
    state: Res<MapEditState>,
    hover: Res<MapEditHoverCell>,
    drag: Res<MapEditDragAnchor>,
    variant: Res<MapEditVariantIndex>,
    floor: Res<ActiveFloorLevel>,
    mut meshes: ResMut<Assets<Mesh>>,
    preview_mat: Option<Res<MapEditPreviewMaterial>>,
    mut preview_entity: Local<Option<Entity>>,
) {
    let Some(preview_mat) = preview_mat else {
        return;
    };

    let show = state.placement_tile.is_some() && (hover.0.is_some() || drag.0.is_some());
    if !show {
        if let Some(e) = *preview_entity {
            commands.entity(e).insert(Visibility::Hidden);
        }
        return;
    }

    let kind = state.placement_tile.unwrap();
    let f = floor.0;
    let strokes = preview_stroke_cells(kind, drag.0, hover.0);
    if strokes.is_empty() {
        if let Some(e) = *preview_entity {
            commands.entity(e).insert(Visibility::Hidden);
        }
        return;
    }

    let min_x = strokes.iter().map(|(x, _)| *x).min().unwrap();
    let min_z = strokes.iter().map(|(_, z)| *z).min().unwrap();

    let room_bounds = (kind == MapTileKind::Room).then(|| match (drag.0, hover.0) {
        (Some(s), Some(h)) => rect_axis_bounds(s, h),
        (Some(s), None) => rect_axis_bounds(s, s),
        (None, Some(h)) => rect_axis_bounds(h, h),
        (None, None) => {
            let (x, z) = strokes[0];
            rect_axis_bounds((x, z), (x, z))
        }
    });

    let wall_or_corner_cell = matches!(kind, MapTileKind::Wall | MapTileKind::WallGlass | MapTileKind::Corner)
        .then(|| resolved_cell(kind, variant.0));
    let mesh_opt = if f == 0 {
        match kind {
            MapTileKind::Void => {
                let rel: Vec<_> = strokes
                    .iter()
                    .map(|&(x, z)| (x - min_x, z - min_z, CellType::Road))
                    .collect();
                build_floor0_road_mesh(&rel, min_x, min_z)
            }
            MapTileKind::Road => {
                let rel: Vec<_> = strokes
                    .iter()
                    .map(|&(x, z)| (x - min_x, z - min_z, CellType::Road))
                    .collect();
                build_floor0_road_mesh(&rel, min_x, min_z)
            }
            MapTileKind::Wall | MapTileKind::WallGlass => {
                let c = wall_or_corner_cell.expect("wall brush");
                let rel: Vec<_> = strokes
                    .iter()
                    .map(|&(x, z)| (x - min_x, z - min_z, c))
                    .collect();
                build_floor0_wall_mesh(&rel, min_x, min_z).or_else(void_preview_plane)
            }
            MapTileKind::Room => {
                let (bx0, bx1, bz0, bz1) = room_bounds.expect("room_bounds set for Room");
                let rel: Vec<_> = strokes
                    .iter()
                    .map(|&(x, z)| {
                        let m = perimeter_wall_mask(x, z, bx0, bx1, bz0, bz1);
                        (x - min_x, z - min_z, CellType::Wall(m))
                    })
                    .collect();
                build_floor0_wall_mesh(&rel, min_x, min_z).or_else(void_preview_plane)
            }
            MapTileKind::Corner => {
                let c = wall_or_corner_cell.expect("corner brush");
                let (ix, iz) = strokes[0];
                build_floor0_wall_mesh(&[(0, 0, c)], ix, iz).or_else(void_preview_plane)
            }
            MapTileKind::GlitchBot | MapTileKind::BlackBot => void_preview_plane(),
        }
    } else {
        match kind {
            MapTileKind::Void => {
                let rel: Vec<_> = strokes
                    .iter()
                    .map(|&(x, z)| (x - min_x, z - min_z, f, CellType::Road))
                    .collect();
                build_upper_road_mesh(&rel, min_x, min_z)
            }
            MapTileKind::Road => {
                let rel: Vec<_> = strokes
                    .iter()
                    .map(|&(x, z)| (x - min_x, z - min_z, f, CellType::Road))
                    .collect();
                build_upper_road_mesh(&rel, min_x, min_z)
            }
            MapTileKind::Wall | MapTileKind::WallGlass => {
                let c = wall_or_corner_cell.expect("wall brush");
                let rel: Vec<_> = strokes
                    .iter()
                    .map(|&(x, z)| (x - min_x, z - min_z, f, c))
                    .collect();
                build_upper_wall_mesh(&rel, min_x, min_z).or_else(void_preview_plane)
            }
            MapTileKind::Room => {
                let (bx0, bx1, bz0, bz1) = room_bounds.expect("room_bounds set for Room");
                let rel: Vec<_> = strokes
                    .iter()
                    .map(|&(x, z)| {
                        let m = perimeter_wall_mask(x, z, bx0, bx1, bz0, bz1);
                        (x - min_x, z - min_z, f, CellType::Wall(m))
                    })
                    .collect();
                build_upper_wall_mesh(&rel, min_x, min_z).or_else(void_preview_plane)
            }
            MapTileKind::Corner => {
                let c = wall_or_corner_cell.expect("corner brush");
                let (ix, iz) = strokes[0];
                build_upper_wall_mesh(&[(0, 0, f, c)], ix, iz).or_else(void_preview_plane)
            }
            MapTileKind::GlitchBot | MapTileKind::BlackBot => void_preview_plane(),
        }
    };

    let Some(mesh) = mesh_opt else {
        return;
    };
    let mesh_h = meshes.add(mesh);

    let lift_y = if matches!(kind, MapTileKind::Void | MapTileKind::Road) {
        f as f32 * HYPERMAP_FLOOR_HEIGHT + 0.02
    } else {
        0.0
    };
    let transform = Transform::from_xyz(0.0, lift_y, 0.0);

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
                Name::new("Map edit preview"),
                MapEditPreviewRoot,
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

fn void_preview_plane() -> Option<Mesh> {
    Some(Plane3d::default().mesh().size(0.96, 0.96).into())
}

#[derive(Resource, Clone)]
struct MapEditPreviewMaterial(Handle<StandardMaterial>);

fn map_edit_pointer_stroke(
    mut commands: Commands,
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform), With<StrategyCameraRig>>,
    state: Res<MapEditState>,
    mut drag: ResMut<MapEditDragAnchor>,
    variant: Res<MapEditVariantIndex>,
    style: Res<MapEditStyleState>,
    floor: Res<ActiveFloorLevel>,
    level: Res<LevelName>,
    runtime: Res<HypermapRuntime>,
    mut remesh: ResMut<HypermapChunkRemeshQueue>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut bot_rng: ResMut<GlitchBotRng>,
    mut black_rng: ResMut<BlackBotRng>,
) {
    let Some(kind) = state.placement_tile else {
        drag.0 = None;
        return;
    };

    let Ok(window) = windows.single() else {
        return;
    };
    let Ok((cam, cam_gt)) = cameras.single() else {
        return;
    };

    let fl = floor.0.min(HYPERMAP_FLOOR_MAX);

    if mouse.just_pressed(MouseButton::Left) {
        if map_edit_cursor_ok_for_paint(&window) {
            if let Some(start) = map_edit_plane_cell(&window, &cam, &cam_gt, floor.0) {
                drag.0 = Some(start);
            }
        }
    }

    if !mouse.just_released(MouseButton::Left) {
        return;
    }

    let Some(start) = drag.0.take() else {
        return;
    };
    if !map_edit_cursor_ok_for_paint(&window) {
        return;
    }
    let Some(end) = map_edit_plane_cell(&window, &cam, &cam_gt, floor.0) else {
        return;
    };

    if kind == MapTileKind::GlitchBot {
        let center = Vec2::new(end.0 as f32 + 0.5, end.1 as f32 + 0.5);
        glitch_bot::spawn_glitch_bot(
            &mut commands,
            &mut meshes,
            &mut materials,
            &mut bot_rng.0,
            center,
        );
        return;
    }

    if kind == MapTileKind::BlackBot {
        let center = Vec2::new(end.0 as f32 + 0.5, end.1 as f32 + 0.5);
        black_bot::spawn_black_bot(
            &mut commands,
            &mut meshes,
            &mut materials,
            &mut black_rng.0,
            center,
        );
        return;
    }

    let tiles = stroke_world_cells(kind, start, end);
    if tiles.is_empty() {
        return;
    }

    let mut chunk_coords = HashSet::<ChunkCoord>::new();
    for &(ix, iz) in &tiles {
        chunk_coords.insert(world_to_chunk_local(ix, iz).0);
    }
    for c in &chunk_coords {
        ensure_chunk_generated(
            &runtime.map,
            &runtime.static_passability_map,
            &runtime.static_subtile_cache,
            &runtime.style_floor_map,
            &runtime.style_wall_map,
            *c,
            level.0.as_str(),
        );
    }

    let floor_styles = floor_styles_for_kind(kind);
    let floor_paint = floor_styles[(style.floor as usize).min(floor_styles.len().saturating_sub(1))];
    let wall_styles = wall_styles_for_kind(kind);
    let wall_paint = wall_styles[(style.wall as usize).min(wall_styles.len().saturating_sub(1))];

    if kind == MapTileKind::Room {
        let (bx0, bx1, bz0, bz1) = rect_axis_bounds(start, end);
        for &(ix, iz) in &tiles {
            let mask = perimeter_wall_mask(ix, iz, bx0, bx1, bz0, bz1);
            write_world_cell(&runtime, ix, iz, fl, CellType::Wall(mask));
            write_world_floor_style(&runtime, ix, iz, fl, floor_paint);
            write_world_wall_style(&runtime, ix, iz, fl, wall_paint);
        }
    } else {
        let cell = resolved_cell(kind, variant.0);
        for &(ix, iz) in &tiles {
            write_world_cell(&runtime, ix, iz, fl, cell);
            write_world_floor_style(&runtime, ix, iz, fl, floor_paint);
            write_world_wall_style(&runtime, ix, iz, fl, wall_paint);
        }
    }

    let mut remeshed = HashSet::<ChunkCoord>::new();
    for &(ix, iz) in &tiles {
        let cc = world_to_chunk_local(ix, iz).0;
        if remeshed.insert(cc) {
            queue_hypermap_chunk_remesh(&mut remesh, ix, iz);
        }
    }
}

fn map_edit_right_click_cancel_placement(
    mouse: Res<ButtonInput<MouseButton>>,
    mut state: ResMut<MapEditState>,
    mut drag: ResMut<MapEditDragAnchor>,
) {
    if mouse.just_pressed(MouseButton::Right) {
        state.placement_tile = None;
        drag.0 = None;
    }
}

fn map_edit_scroll_variants(
    state: Res<MapEditState>,
    mut variant: ResMut<MapEditVariantIndex>,
    mut wheel: MessageReader<MouseWheel>,
) {
    if state.placement_tile.is_none() {
        return;
    }
    let kind = state.placement_tile.unwrap();
    let max_v = match kind {
        MapTileKind::Wall | MapTileKind::WallGlass => 15,
        MapTileKind::Corner => 4,
        MapTileKind::Void | MapTileKind::Road | MapTileKind::Room | MapTileKind::GlitchBot | MapTileKind::BlackBot => return,
    };

    let mut scroll = 0.0f32;
    for ev in wheel.read() {
        scroll += match ev.unit {
            MouseScrollUnit::Line => ev.y,
            MouseScrollUnit::Pixel => ev.y * 0.05,
        };
    }
    if scroll.abs() < 1e-4 {
        return;
    }
    let delta = if scroll > 0.0 { 1i32 } else { -1i32 };
    let v = variant.0 as i32 + delta;
    let wrapped = v.rem_euclid(max_v as i32) as u32;
    variant.0 = wrapped;
}

fn floor_styles_for_kind(kind: MapTileKind) -> &'static [TileStyle] {
    match kind {
        MapTileKind::Road | MapTileKind::Room | MapTileKind::Wall
        | MapTileKind::WallGlass | MapTileKind::Corner => &[
            TileStyle::DEFAULT,
            TileStyle([b'f', b'g']),
            TileStyle([b'f', b'p']),
            TileStyle([b'f', b'm']),
        ],
        _ => &[TileStyle::DEFAULT],
    }
}

fn wall_styles_for_kind(kind: MapTileKind) -> &'static [TileStyle] {
    match kind {
        MapTileKind::Wall | MapTileKind::Room | MapTileKind::Corner => {
            &[TileStyle([b'w', b'r']), TileStyle([b'w', b'g'])]
        }
        MapTileKind::WallGlass => &[TileStyle([b'w', b'g'])],
        _ => &[TileStyle::DEFAULT],
    }
}

fn floor_style_label(style: TileStyle) -> &'static str {
    match style.0 {
        [b'f', b'g'] => "Glass",
        [b'f', b'p'] => "Pavement",
        [b'f', b'm'] => "Marble",
        _ => "Default",
    }
}

fn wall_style_label(style: TileStyle) -> &'static str {
    match style.0 {
        [b'w', b'g'] => "Glass",
        _ => "Regular",
    }
}

fn map_edit_tab_cycle_style(
    state: Res<MapEditState>,
    keys: Res<ButtonInput<KeyCode>>,
    mut style: ResMut<MapEditStyleState>,
) {
    if !state.panel_open {
        return;
    }
    if !keys.just_pressed(KeyCode::Tab) {
        return;
    }
    let Some(kind) = state.placement_tile else {
        return;
    };
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    if shift {
        let styles = wall_styles_for_kind(kind);
        if styles.len() > 1 {
            style.wall = ((style.wall as usize + 1) % styles.len()) as u32;
        }
    } else {
        let styles = floor_styles_for_kind(kind);
        style.floor = ((style.floor as usize + 1) % styles.len()) as u32;
    }
}

fn map_edit_sync_style_label(
    state: Res<MapEditState>,
    style: Res<MapEditStyleState>,
    mut labels: Query<&mut Text, With<MapEditStyleInfoLabel>>,
) {
    let text = match state.placement_tile {
        Some(kind) => {
            let floor_styles = floor_styles_for_kind(kind);
            let fi = (style.floor as usize).min(floor_styles.len().saturating_sub(1));
            let floor_style = floor_styles[fi];

            let wall_styles = wall_styles_for_kind(kind);
            let wi = (style.wall as usize).min(wall_styles.len().saturating_sub(1));
            let wall_style = wall_styles[wi];

            let mut parts = String::new();
            if floor_styles.len() > 1 {
                parts.push_str(&format!("[Tab] Floor: {}  ", floor_style_label(floor_style)));
            }
            if wall_styles.len() > 1 {
                parts.push_str(&format!("[Shift+Tab] Wall: {}", wall_style_label(wall_style)));
            }
            parts
        }
        None => String::new(),
    };
    for mut t in &mut labels {
        **t = text.clone();
    }
}

pub(crate) fn setup_map_edit_preview_material(mut commands: Commands, mut materials: ResMut<Assets<StandardMaterial>>) {
    let h = materials.add(StandardMaterial {
        base_color: Color::srgba(0.35, 1.0, 0.25, 0.42),
        emissive: LinearRgba::BLACK,
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        depth_bias: 1.0,
        ..Default::default()
    });
    commands.insert_resource(MapEditPreviewMaterial(h));
}

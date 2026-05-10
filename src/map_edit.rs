//! In-game tile edit mode: pick a tile type, preview at cursor, place on click.

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::camera::StrategyCameraRig;
use crate::floor_level::{ActiveFloorLevel, HYPERMAP_FLOOR_HEIGHT};
use crate::hypermap::world_to_chunk_local;
use crate::hypermap_world::{
    build_floor0_road_mesh, build_floor0_wall_mesh, build_upper_road_mesh, build_upper_wall_mesh,
    ensure_chunk_generated, queue_hypermap_chunk_remesh, HypermapChunkRemeshQueue, HypermapRuntime,
};
use crate::world_map::{CellType, WallCorner, WallMask};

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapTileKind {
    Void,
    Road,
    Wall,
    Corner,
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
struct MapEditHoverCell(pub Option<(i32, i32)>);

#[derive(Component)]
struct MapEditPreviewRoot;

pub struct MapEditPlugin;

impl Plugin for MapEditPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MapEditState>()
            .init_resource::<MapEditVariantIndex>()
            .init_resource::<MapEditHoverCell>()
            .add_systems(Startup, setup_map_edit_preview_material)
            .add_systems(
                Update,
                (
                    sync_map_edit_toggle_button_label,
                    map_edit_toggle_panel,
                    map_edit_tile_pick_buttons,
                    map_edit_hover_under_cursor,
                    map_edit_update_preview,
                    map_edit_left_click_place,
                    map_edit_right_click_cancel_placement,
                    map_edit_scroll_variants,
                ),
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
                ("Corner", MapTileKind::Corner),
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
        "Edit ✓"
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
    mut palette: Query<&mut Visibility, With<MapEditPaletteRoot>>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        state.panel_open = !state.panel_open;
        if !state.panel_open {
            state.placement_tile = None;
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

fn map_edit_tile_pick_buttons(
    interactions: Query<
        (&Interaction, &MapEditTilePickButton),
        (Changed<Interaction>, With<Button>),
    >,
    mut state: ResMut<MapEditState>,
    mut variant: ResMut<MapEditVariantIndex>,
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

    let Some(cursor) = window.cursor_position() else {
        hover.0 = None;
        return;
    };
    let h = window.height();
    if cursor.y > h - 120.0 {
        hover.0 = None;
        return;
    }
    let Ok(ray) = cam.viewport_to_world(cam_gt, cursor) else {
        hover.0 = None;
        return;
    };

    let plane_y = floor.0 as f32 * HYPERMAP_FLOOR_HEIGHT;
    let Some(hit) = ray_intersect_horizontal_plane(ray, plane_y) else {
        hover.0 = None;
        return;
    };

    let ix = hit.x.floor() as i32;
    let iz = hit.z.floor() as i32;
    hover.0 = Some((ix, iz));
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
        MapTileKind::Wall => {
            let bits = ((variant % 15) + 1) as u8;
            CellType::Wall(WallMask::from_bits(bits).expect("1..=15 valid"))
        }
        MapTileKind::Corner => {
            let c = match variant % 4 {
                0 => WallCorner::Nw,
                1 => WallCorner::Ne,
                2 => WallCorner::Sw,
                _ => WallCorner::Se,
            };
            CellType::Corner(c)
        }
    }
}

fn map_edit_update_preview(
    mut commands: Commands,
    state: Res<MapEditState>,
    hover: Res<MapEditHoverCell>,
    variant: Res<MapEditVariantIndex>,
    floor: Res<ActiveFloorLevel>,
    mut meshes: ResMut<Assets<Mesh>>,
    preview_mat: Option<Res<MapEditPreviewMaterial>>,
    mut preview_entity: Local<Option<Entity>>,
) {
    let Some(preview_mat) = preview_mat else {
        return;
    };

    let show = state.placement_tile.is_some() && hover.0.is_some();
    if !show {
        if let Some(e) = *preview_entity {
            commands.entity(e).insert(Visibility::Hidden);
        }
        return;
    }

    let (ix, iz) = hover.0.unwrap();
    let kind = state.placement_tile.unwrap();
    let f = floor.0;
    let cell = resolved_cell(kind, variant.0);

    let mesh_opt = if f == 0 {
        match cell {
            CellType::Void => void_preview_plane(),
            CellType::Road => build_floor0_road_mesh(&[(0, 0, CellType::Road)], ix, iz),
            CellType::Wall(_) | CellType::Corner(_) => build_floor0_wall_mesh(&[(0, 0, cell)], ix, iz)
                .or_else(void_preview_plane),
        }
    } else {
        match cell {
            CellType::Void => void_preview_plane(),
            CellType::Road => build_upper_road_mesh(&[(0, 0, f, CellType::Road)], ix, iz),
            CellType::Wall(_) | CellType::Corner(_) => {
                build_upper_wall_mesh(&[(0, 0, f, cell)], ix, iz).or_else(void_preview_plane)
            }
        }
    };

    let Some(mesh) = mesh_opt else {
        return;
    };
    let mesh_h = meshes.add(mesh);

    let lift_y = if matches!(cell, CellType::Void | CellType::Road) {
        f as f32 * HYPERMAP_FLOOR_HEIGHT + 0.02
    } else {
        0.0
    };
    let transform = Transform::from_xyz(0.0, lift_y, 0.0);

    let entity = if let Some(e) = *preview_entity {
        e
    } else {
        let e = commands
            .spawn((
                Name::new("Map edit preview"),
                MapEditPreviewRoot,
                Mesh3d(mesh_h.clone()),
                MeshMaterial3d(preview_mat.0.clone()),
                transform,
                Visibility::Inherited,
            ))
            .id();
        *preview_entity = Some(e);
        e
    };

    commands.entity(entity).insert((
        Mesh3d(mesh_h),
        MeshMaterial3d(preview_mat.0.clone()),
        transform,
        Visibility::Inherited,
    ));
}

fn void_preview_plane() -> Option<Mesh> {
    Some(Plane3d::default().mesh().size(0.96, 0.96).into())
}

#[derive(Resource, Clone)]
struct MapEditPreviewMaterial(Handle<StandardMaterial>);

fn map_edit_left_click_place(
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    state: Res<MapEditState>,
    hover: Res<MapEditHoverCell>,
    variant: Res<MapEditVariantIndex>,
    floor: Res<ActiveFloorLevel>,
    runtime: Res<HypermapRuntime>,
    mut remesh: ResMut<HypermapChunkRemeshQueue>,
) {
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    if let Some(cursor) = window.cursor_position() {
        let h = window.height();
        if cursor.y > h - 120.0 {
            return;
        }
    }
    let Some(kind) = state.placement_tile else {
        return;
    };
    let Some((ix, iz)) = hover.0 else {
        return;
    };

    let cell = resolved_cell(kind, variant.0);
    let (chunk_coord, _) = world_to_chunk_local(ix, iz);
    ensure_chunk_generated(&runtime.map, chunk_coord);
    runtime
        .map
        .set_floor(ix, iz, floor.0.min(9), cell);
    queue_hypermap_chunk_remesh(&mut remesh, ix, iz);
}

fn map_edit_right_click_cancel_placement(
    mouse: Res<ButtonInput<MouseButton>>,
    mut state: ResMut<MapEditState>,
) {
    if mouse.just_pressed(MouseButton::Right) {
        state.placement_tile = None;
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
        MapTileKind::Wall => 15,
        MapTileKind::Corner => 4,
        MapTileKind::Void | MapTileKind::Road => return,
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

pub(crate) fn setup_map_edit_preview_material(mut commands: Commands, mut materials: ResMut<Assets<StandardMaterial>>) {
    let h = materials.add(StandardMaterial {
        base_color: Color::srgba(0.35, 1.0, 0.25, 0.42),
        emissive: LinearRgba::rgb(0.25, 1.0, 0.15) * 3.5,
        perceptual_roughness: 0.45,
        metallic: 0.0,
        alpha_mode: AlphaMode::Blend,
        depth_bias: 1.0,
        ..Default::default()
    });
    commands.insert_resource(MapEditPreviewMaterial(h));
}

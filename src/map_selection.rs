//! Grid cell selection from pointer hits on hypermap floor meshes.

use bevy::mesh::PlaneMeshBuilder;
use bevy::pbr::StandardMaterial;
use bevy::picking::prelude::*;
use bevy::prelude::*;

use crate::floor_level::HYPERMAP_FLOOR_HEIGHT;

/// Vertical offset of the duplicated floor tile for the active selection.
pub const SELECTED_CELL_LIFT_Y: f32 = 0.2;

/// Road material handle for the selection highlight; set from hypermap asset setup.
#[derive(Resource, Debug, Clone)]
pub struct MapSelectionRoadMaterial(pub Handle<StandardMaterial>);

/// Selected grid column, row, and floor index (`0..=9`).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectedMapCell(pub Option<(i32, i32, u8)>);

#[derive(Component)]
pub(crate) struct SelectedCellHighlight;

pub struct MapSelectionPlugin;

impl Plugin for MapSelectionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SelectedMapCell>()
            .add_systems(Update, sync_selected_cell_lift);
    }
}

pub(crate) fn floor_grid_click(click: On<Pointer<Click>>, mut selected: ResMut<SelectedMapCell>) {
    if click.event.button != PointerButton::Primary {
        return;
    }
    let Some(pos) = click.event.hit.position else {
        return;
    };
    let ix = pos.x.floor() as i32;
    let iz = pos.z.floor() as i32;
    let fy = (pos.y / HYPERMAP_FLOOR_HEIGHT).floor().clamp(0.0, 9.0) as u8;
    selected.0 = Some((ix, iz, fy));
}

fn sync_selected_cell_lift(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    road: Option<Res<MapSelectionRoadMaterial>>,
    selected: Res<SelectedMapCell>,
    mut highlight: Local<Option<Entity>>,
) {
    let need_sync = highlight.is_none() || selected.is_changed();
    if !need_sync {
        return;
    }

    let Some(road) = road else {
        return;
    };

    let entity = if let Some(e) = *highlight {
        e
    } else {
        let mesh = meshes.add(PlaneMeshBuilder::from_size(Vec2::splat(0.98)));
        let e = commands
            .spawn((
                Name::new("Selected map cell"),
                SelectedCellHighlight,
                Mesh3d(mesh),
                MeshMaterial3d(road.0.clone()),
                Transform::IDENTITY,
                Visibility::Hidden,
            ))
            .id();
        *highlight = Some(e);
        e
    };

    let Some((ix, iz, floor)) = selected.0 else {
        commands.entity(entity).insert(Visibility::Hidden);
        return;
    };

    let y_base = floor as f32 * HYPERMAP_FLOOR_HEIGHT;
    commands.entity(entity).insert((
        Transform::from_xyz(
            ix as f32 + 0.5,
            y_base + SELECTED_CELL_LIFT_Y,
            iz as f32 + 0.5,
        ),
        Visibility::Inherited,
    ));
}

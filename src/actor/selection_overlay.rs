//! World-space visualization of the currently selected actor.
//!
//! Driven by the [`SelectedActor`] resource (set by mesh picking in
//! `crate::hud::actor_inspector`). Two pieces, both for the selected bot only:
//!
//! - a small glowing green cube hovering above the bot, marking it; and
//! - immediate-mode gizmo **waypoints**: the bot's remaining simplified path,
//!   drawn as a polyline through each upcoming waypoint tile center with a node
//!   marker at each. Unlike the global path overlay (`paint_black_bot_targets`,
//!   gated on `PathOverlayEnabled`), this is always shown for the selection and
//!   independent of the overlay toggle.

use bevy::prelude::*;

use crate::actor::black_bot::BlackBotVisual;
use crate::actor::brain::{Brain, PathNode};
use crate::actor::ActorObject;
use crate::hud::actor_inspector::SelectedActor;
use crate::menu::main_menu::GameState;

/// Edge length of the marker cube (meters).
const MARKER_SIZE: f32 = 0.18;
/// Marker color (also its emissive tint, so bloom makes it glow).
const MARKER_COLOR: Color = Color::srgb(0.30, 1.0, 0.45);
/// Emissive multiplier — high enough to bloom against the dark scene.
const MARKER_EMISSIVE: f32 = 5.0;
/// Resting height of the marker center above floor 0. The bot sphere tops out
/// around 1.2 m, so this floats clearly above it.
const MARKER_HEIGHT: f32 = 1.55;
/// Vertical bob amplitude / speed for the hovering animation.
const MARKER_BOB_AMPLITUDE: f32 = 0.07;
const MARKER_BOB_SPEED: f32 = 3.0;
/// Spin rate (rad/s) around the vertical axis.
const MARKER_SPIN_SPEED: f32 = 1.6;
/// Fixed tilt so the spinning cube reads as a diamond rather than a flat face.
const MARKER_TILT: f32 = 0.62;

/// Height above floor 0 at which the waypoint route is drawn (just off the ground).
const WAYPOINT_Y: f32 = 0.12;
/// Route / waypoint color.
const WAYPOINT_COLOR: Color = Color::srgb(0.35, 1.0, 0.55);
/// Gizmo sphere radius at an intermediate waypoint and at the final target.
const WAYPOINT_RADIUS: f32 = 0.12;
const WAYPOINT_TARGET_RADIUS: f32 = 0.22;

/// Marks the singleton glowing selection cube.
#[derive(Component)]
struct SelectionMarker;

pub struct SelectionOverlayPlugin;

impl Plugin for SelectionOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (sync_selection_marker, draw_selected_waypoints)
                .run_if(in_state(GameState::InGame)),
        );
    }
}

/// Keeps the glowing marker cube hovering over the selected bot, hiding it when
/// nothing is selected. The cube is spawned lazily once and then reused.
fn sync_selection_marker(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    time: Res<Time>,
    selection: Res<SelectedActor>,
    actors: Query<&ActorObject, With<BlackBotVisual>>,
    mut marker: Local<Option<Entity>>,
    mut markers: Query<(&mut Transform, &mut Visibility), With<SelectionMarker>>,
) {
    let Some(entity) = *marker else {
        let mesh = meshes.add(Cuboid::from_size(Vec3::splat(MARKER_SIZE)));
        let material = materials.add(StandardMaterial {
            base_color: MARKER_COLOR,
            emissive: LinearRgba::from(MARKER_COLOR) * MARKER_EMISSIVE,
            ..default()
        });
        let e = commands
            .spawn((
                Name::new("Selection marker"),
                SelectionMarker,
                Mesh3d(mesh),
                MeshMaterial3d(material),
                Transform::IDENTITY,
                Visibility::Hidden,
            ))
            .id();
        *marker = Some(e);
        return;
    };

    let Ok((mut tf, mut vis)) = markers.get_mut(entity) else {
        return;
    };

    let target = selection.entity.and_then(|e| actors.get(e).ok());
    let Some(obj) = target else {
        if *vis != Visibility::Hidden {
            *vis = Visibility::Hidden;
        }
        return;
    };

    let center = obj.inner.state().center;
    let t = time.elapsed_secs();
    let bob = (t * MARKER_BOB_SPEED).sin() * MARKER_BOB_AMPLITUDE;
    tf.translation = Vec3::new(center.x, MARKER_HEIGHT + bob, center.y);
    tf.rotation = Quat::from_rotation_y(t * MARKER_SPIN_SPEED) * Quat::from_rotation_x(MARKER_TILT);
    if *vis != Visibility::Inherited {
        *vis = Visibility::Inherited;
    }
}

/// Draws the selected bot's remaining route as a gizmo polyline plus a node
/// marker at each upcoming waypoint. The bot's main tile is floor 0, so tile
/// `(tx, ty)` maps to world center `(tx + 0.5, ty + 0.5)`.
fn draw_selected_waypoints(
    selection: Res<SelectedActor>,
    bots: Query<(&Brain, &ActorObject), With<BlackBotVisual>>,
    mut gizmos: Gizmos,
) {
    let Some(selected) = selection.entity else {
        return;
    };
    let Ok((brain, obj)) = bots.get(selected) else {
        return;
    };
    let Some((path, idx)) = brain.route() else {
        return;
    };
    let remaining = path.get(idx..).unwrap_or(&[]);
    if remaining.is_empty() {
        return;
    }

    let center = obj.inner.state().center;
    let start = Vec3::new(center.x, WAYPOINT_Y, center.y);
    // Works for both coarse cell legs and spliced subcell detours — read the
    // node's tile-space center, never its grid kind.
    let waypoint_pos = |node: &PathNode| {
        let c = node.center();
        Vec3::new(c.x, WAYPOINT_Y, c.y)
    };

    gizmos.linestrip(
        std::iter::once(start).chain(remaining.iter().map(waypoint_pos)),
        WAYPOINT_COLOR,
    );

    let last = remaining.len() - 1;
    for (i, node) in remaining.iter().enumerate() {
        let radius = if i == last { WAYPOINT_TARGET_RADIUS } else { WAYPOINT_RADIUS };
        gizmos.sphere(Isometry3d::from_translation(waypoint_pos(node)), radius, WAYPOINT_COLOR);
    }
}

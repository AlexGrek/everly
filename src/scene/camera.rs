//! Strategy-style camera: a tilted overhead view that pans on the world's
//! XZ plane via WASD or arrow keys (with velocity and coasting), and zooms
//! with the mouse wheel.

use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::light::AmbientLight;
use bevy::pbr::{ScreenSpaceAmbientOcclusion, ScreenSpaceReflections};
use bevy::post_process::bloom::Bloom;
use bevy::prelude::*;
use bevy::render::view::Hdr;

use crate::edit::map_edit::{MapEditState, MapTileKind};
use crate::hud::actor_inspector::InspectorPointerOver;
use crate::map::floor_level::{
    ActiveFloorLevel, CAMERA_FLOOR_Y_SMOOTH_PER_S, HYPERMAP_FLOOR_HEIGHT,
};
use crate::menu::main_menu::GameState;

/// Tilt used for the normal RTS-style view (degrees → radians in [`StrategyCamera::default`]).
pub const STRATEGY_CAMERA_DEFAULT_PITCH: f32 = 55.0_f32.to_radians();
/// Near-vertical pitch for the map / top-down view (slightly off 90° so `look_at` stays stable).
pub const STRATEGY_CAMERA_MAP_PITCH: f32 = 89.0_f32.to_radians();

/// How the strategy camera interprets tilt: angled gameplay vs map-style top-down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrategyCameraViewMode {
    #[default]
    Strategy,
    Map,
}

/// Marker on the entity that carries [`Camera3d`] + [`StrategyCamera`] (for UI target wiring).
#[derive(Component, Debug, Clone, Copy)]
pub struct StrategyCameraRig;

/// Marker + parameters for the player-controlled strategy camera.
#[derive(Component, Debug, Clone, Copy)]
pub struct StrategyCamera {
    /// World-space ground point the camera orbits around.
    pub focus: Vec3,
    /// Ground-plane pan velocity in world space (Y should stay zero).
    pub pan_velocity: Vec3,
    /// Distance from `focus` to the camera position, in world units.
    pub distance: f32,
    /// Yaw around the world Y axis, in radians (0 looks toward -Z).
    pub yaw: f32,
    /// Tilt below the horizon, in radians (π/2 = straight down).
    pub pitch: f32,
    pub view_mode: StrategyCameraViewMode,
    /// Max pan speed in world units per second at the reference distance.
    pub pan_speed: f32,
    /// How quickly pan velocity approaches the target while keys are held
    /// (world units per second squared, scaled like `pan_speed`).
    pub pan_acceleration: f32,
    /// Exponential decay rate for `pan_velocity` when no pan input (per second).
    /// Higher values stop the camera sooner after key release.
    pub pan_drag: f32,
    /// Zoom speed in world units per scroll tick.
    pub zoom_speed: f32,
    /// Distance bounds the zoom is clamped to.
    pub min_distance: f32,
    pub max_distance: f32,
}

impl Default for StrategyCamera {
    fn default() -> Self {
        Self {
            focus: Vec3::ZERO,
            pan_velocity: Vec3::ZERO,
            distance: 30.0,
            yaw: 0.0,
            pitch: STRATEGY_CAMERA_DEFAULT_PITCH,
            view_mode: StrategyCameraViewMode::default(),
            pan_speed: 18.0,
            pan_acceleration: 55.0,
            pan_drag: 3.2,
            zoom_speed: 4.0,
            min_distance: 8.0,
            max_distance: 80.0,
        }
    }
}

/// Reference distance used to scale pan speed. Panning feels equally fast
/// regardless of how zoomed-in or zoomed-out the player currently is.
const PAN_REFERENCE_DISTANCE: f32 = 30.0;

/// Cool fill so unlit faces read in shadow; kept modest so the sun + emissive mood stay primary.
const STRATEGY_AMBIENT_COLOR: Color = Color::srgb(0.9, 0.93, 0.98);
const STRATEGY_AMBIENT_BRIGHTNESS_ON: f32 = 40.0;

/// Toolbar-controlled ambient fill on [`StrategyCameraRig`] ([`AmbientLight`] overrides [`GlobalAmbientLight`] for that view).
#[derive(Resource, Clone, Copy, PartialEq, Eq)]
pub struct AmbientFillEnabled(pub bool);

impl Default for AmbientFillEnabled {
    fn default() -> Self {
        Self(true)
    }
}

pub struct StrategyCameraPlugin;

impl Plugin for StrategyCameraPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AmbientFillEnabled>()
            .add_systems(OnEnter(GameState::Loading), spawn_camera)
            .add_systems(
                Update,
                (
                    pan_camera,
                    zoom_camera,
                    smooth_focus_y_for_active_floor,
                    sync_camera_transform,
                    sync_ambient_fill_brightness,
                )
                    .run_if(crate::menu::main_menu::in_world_session),
            );
    }
}

pub(crate) fn spawn_camera(mut commands: Commands, ambient_fill: Res<AmbientFillEnabled>) {
    let cam = StrategyCamera::default();
    let transform = strategy_transform(&cam);
    let ambient_brightness = if ambient_fill.0 {
        STRATEGY_AMBIENT_BRIGHTNESS_ON
    } else {
        0.0
    };

    commands.spawn((
        Name::new("Strategy Camera"),
        StrategyCameraRig,
        Camera3d::default(),
        Msaa::Off,
        Hdr,
        AmbientLight {
            color: STRATEGY_AMBIENT_COLOR,
            brightness: ambient_brightness,
            ..default()
        },
        Tonemapping::TonyMcMapface,
        Bloom {
            // Bloom strength for the whole view; per-mesh glow comes from
            // `StandardMaterial::emissive` in linear luminance (nits).
            intensity: 0.26,
            ..Bloom::default()
        },
        // Requires deferred + prepasses (`DepthPrepass`, `DeferredPrepass` via `#[require]`).
        ScreenSpaceReflections {
            // Only fairly smooth pixels trace SSR; rougher surfaces skip (less mirror-like).
            perceptual_roughness_threshold: 0.68,
            ..Default::default()
        },
        // SSAO: needs depth + normal prepasses (`#[require]` on the component).
        ScreenSpaceAmbientOcclusion::default(),
        transform,
        cam,
    ));
}

fn sync_ambient_fill_brightness(
    fill: Res<AmbientFillEnabled>,
    mut lights: Query<&mut AmbientLight, With<StrategyCameraRig>>,
) {
    if !fill.is_changed() {
        return;
    }
    let brightness = if fill.0 {
        STRATEGY_AMBIENT_BRIGHTNESS_ON
    } else {
        0.0
    };
    for mut light in &mut lights {
        light.brightness = brightness;
    }
}

fn pan_camera(keys: Res<ButtonInput<KeyCode>>, time: Res<Time>, mut cameras: Query<&mut StrategyCamera>) {
    let mut input = Vec2::ZERO;
    if keys.any_pressed([KeyCode::KeyW, KeyCode::ArrowUp]) {
        input.y += 1.0;
    }
    if keys.any_pressed([KeyCode::KeyS, KeyCode::ArrowDown]) {
        input.y -= 1.0;
    }
    if keys.any_pressed([KeyCode::KeyA, KeyCode::ArrowLeft]) {
        input.x -= 1.0;
    }
    if keys.any_pressed([KeyCode::KeyD, KeyCode::ArrowRight]) {
        input.x += 1.0;
    }

    let has_input = input != Vec2::ZERO;
    let input_dir = if has_input {
        input.normalize()
    } else {
        Vec2::ZERO
    };

    let dt = time.delta_secs();

    for mut cam in &mut cameras {
        let (sin_yaw, cos_yaw) = cam.yaw.sin_cos();
        // Ground-plane basis derived from yaw. At yaw = 0:
        //   forward = -Z (into the screen)
        //   right   = +X
        let forward = Vec3::new(-sin_yaw, 0.0, -cos_yaw);
        let right = Vec3::new(cos_yaw, 0.0, -sin_yaw);

        let scale = (cam.distance / PAN_REFERENCE_DISTANCE).max(0.25);

        if has_input {
            let desired = (right * input_dir.x + forward * input_dir.y) * cam.pan_speed * scale;
            let max_step = cam.pan_acceleration * scale * dt;
            cam.pan_velocity = cam.pan_velocity.move_towards(desired, max_step);
        } else {
            let factor = (-cam.pan_drag * dt).exp();
            cam.pan_velocity *= factor;
            if cam.pan_velocity.length_squared() < 1e-6 {
                cam.pan_velocity = Vec3::ZERO;
            }
        }

        let vel = cam.pan_velocity;
        cam.focus += vel * dt;
    }
}

fn zoom_camera(
    map_edit: Option<Res<MapEditState>>,
    panel_over: Option<Res<InspectorPointerOver>>,
    mut wheel_messages: MessageReader<MouseWheel>,
    mut cameras: Query<&mut StrategyCamera>,
) {
    // Scrolling over the docked inspector panel scrolls the panel, not the camera.
    if panel_over.as_ref().is_some_and(|p| p.0) {
        return;
    }

    if map_edit.as_ref().is_some_and(|s| {
        matches!(
            s.placement_tile,
            Some(MapTileKind::Wall | MapTileKind::Corner | MapTileKind::Charger)
        )
    }) {
        return;
    }

    let mut scroll = 0.0;
    for ev in wheel_messages.read() {
        scroll += match ev.unit {
            MouseScrollUnit::Line => ev.y,
            // Trackpads emit pixel deltas; rescale so they feel similar to a wheel notch.
            MouseScrollUnit::Pixel => ev.y * 0.05,
        };
    }

    if scroll == 0.0 {
        return;
    }

    for mut cam in &mut cameras {
        let new_distance = cam.distance - scroll * cam.zoom_speed;
        cam.distance = new_distance.clamp(cam.min_distance, cam.max_distance);
    }
}

fn smooth_focus_y_for_active_floor(
    floor: Res<ActiveFloorLevel>,
    time: Res<Time>,
    mut cameras: Query<&mut StrategyCamera>,
) {
    let target_y = floor.0 as f32 * HYPERMAP_FLOOR_HEIGHT;
    let dt = time.delta_secs();
    let blend = 1.0 - (-CAMERA_FLOOR_Y_SMOOTH_PER_S * dt).exp();
    for mut cam in &mut cameras {
        let dy = target_y - cam.focus.y;
        if dy.abs() < 1e-4 {
            cam.focus.y = target_y;
        } else {
            cam.focus.y += dy * blend;
        }
    }
}

fn sync_camera_transform(mut cameras: Query<(&StrategyCamera, &mut Transform), Changed<StrategyCamera>>) {
    for (cam, mut transform) in &mut cameras {
        *transform = strategy_transform(cam);
    }
}

/// Build a camera `Transform` that places it `distance` units away from
/// `focus`, tilted by `pitch` and rotated by `yaw`, looking at `focus`.
pub(crate) fn strategy_transform(cam: &StrategyCamera) -> Transform {
    let (sin_yaw, cos_yaw) = cam.yaw.sin_cos();
    let (sin_pitch, cos_pitch) = cam.pitch.sin_cos();

    // Direction from focus toward the camera.
    let offset = Vec3::new(sin_yaw * cos_pitch, sin_pitch, cos_yaw * cos_pitch);
    let position = cam.focus + offset * cam.distance;

    Transform::from_translation(position).looking_at(cam.focus, Vec3::Y)
}

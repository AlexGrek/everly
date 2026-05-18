//! Serializable strategy camera state for level save/load.
//!
//! Written to `levels/level_{name}/camera.json` when the map editor saves.

use std::fs;
use std::io;
use std::path::PathBuf;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::map::level::LevelName;
use crate::menu::main_menu::GameState;
use crate::scene::camera::{strategy_transform, StrategyCamera, StrategyCameraRig, StrategyCameraViewMode};

pub const CAMERA_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerVec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl From<Vec3> for SerVec3 {
    fn from(v: Vec3) -> Self {
        Self { x: v.x, y: v.y, z: v.z }
    }
}

impl From<SerVec3> for Vec3 {
    fn from(v: SerVec3) -> Self {
        Vec3::new(v.x, v.y, v.z)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StrategyCameraViewModeSnap {
    Strategy,
    Map,
}

impl From<StrategyCameraViewMode> for StrategyCameraViewModeSnap {
    fn from(m: StrategyCameraViewMode) -> Self {
        match m {
            StrategyCameraViewMode::Strategy => Self::Strategy,
            StrategyCameraViewMode::Map => Self::Map,
        }
    }
}

impl From<StrategyCameraViewModeSnap> for StrategyCameraViewMode {
    fn from(m: StrategyCameraViewModeSnap) -> Self {
        match m {
            StrategyCameraViewModeSnap::Strategy => Self::Strategy,
            StrategyCameraViewModeSnap::Map => Self::Map,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrategyCameraSnap {
    pub focus: SerVec3,
    pub pan_velocity: SerVec3,
    pub distance: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub view_mode: StrategyCameraViewModeSnap,
}

impl From<&StrategyCamera> for StrategyCameraSnap {
    fn from(cam: &StrategyCamera) -> Self {
        Self {
            focus: cam.focus.into(),
            pan_velocity: cam.pan_velocity.into(),
            distance: cam.distance,
            yaw: cam.yaw,
            pitch: cam.pitch,
            view_mode: cam.view_mode.into(),
        }
    }
}

impl From<StrategyCameraSnap> for StrategyCamera {
    fn from(s: StrategyCameraSnap) -> Self {
        let mut cam = StrategyCamera::default();
        cam.focus = s.focus.into();
        cam.pan_velocity = s.pan_velocity.into();
        cam.distance = s.distance;
        cam.yaw = s.yaw;
        cam.pitch = s.pitch;
        cam.view_mode = s.view_mode.into();
        cam
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LevelCameraFile {
    pub version: u32,
    pub camera: StrategyCameraSnap,
}

impl LevelCameraFile {
    pub fn from_camera(cam: &StrategyCamera) -> Self {
        Self {
            version: CAMERA_SNAPSHOT_VERSION,
            camera: cam.into(),
        }
    }
}

pub fn camera_path(level_name: &str) -> PathBuf {
    PathBuf::from("levels")
        .join(format!("level_{level_name}"))
        .join("camera.json")
}

pub fn save_level_camera(level_name: &str, file: &LevelCameraFile) -> io::Result<()> {
    let path = camera_path(level_name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, json)?;
    Ok(())
}

pub fn try_load_level_camera(level_name: &str) -> io::Result<Option<LevelCameraFile>> {
    let path = camera_path(level_name);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let file: LevelCameraFile = serde_json::from_str(&text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(file))
}

fn load_level_camera_on_enter(
    level: Res<LevelName>,
    mut cameras: Query<(&mut StrategyCamera, &mut Transform), With<StrategyCameraRig>>,
) {
    let level_name = level.0.as_str();
    let file = match try_load_level_camera(level_name) {
        Ok(Some(f)) => f,
        Ok(None) => return,
        Err(e) => {
            warn!("failed to read `levels/level_{level_name}/camera.json`: {e}");
            return;
        }
    };
    if file.version != CAMERA_SNAPSHOT_VERSION {
        warn!(
            "camera.json version {} (expected {CAMERA_SNAPSHOT_VERSION}); loading anyway",
            file.version
        );
    }
    for (mut cam, mut transform) in &mut cameras {
        *cam = file.camera.clone().into();
        *transform = strategy_transform(&cam);
    }
    info!("loaded strategy camera from `levels/level_{level_name}/camera.json`");
}

pub struct CameraSnapshotPlugin;

impl Plugin for CameraSnapshotPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            OnEnter(GameState::InGame),
            load_level_camera_on_enter.after(crate::scene::camera::spawn_camera),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_camera_snap_round_trip() {
        let cam = StrategyCamera {
            focus: Vec3::new(10.0, 5.0, 20.0),
            pan_velocity: Vec3::new(1.0, 0.0, -0.5),
            distance: 42.0,
            yaw: 0.7,
            pitch: 1.1,
            view_mode: StrategyCameraViewMode::Map,
            ..StrategyCamera::default()
        };
        let snap: StrategyCameraSnap = (&cam).into();
        let back: StrategyCamera = snap.into();
        assert_eq!(back.focus, cam.focus);
        assert_eq!(back.pan_velocity, cam.pan_velocity);
        assert_eq!(back.distance, cam.distance);
        assert_eq!(back.yaw, cam.yaw);
        assert_eq!(back.pitch, cam.pitch);
        assert_eq!(back.view_mode, cam.view_mode);
    }

    #[test]
    fn level_camera_file_json_round_trip() {
        let file = LevelCameraFile {
            version: CAMERA_SNAPSHOT_VERSION,
            camera: StrategyCameraSnap {
                focus: SerVec3 {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                },
                pan_velocity: SerVec3 {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
                distance: 25.0,
                yaw: 0.0,
                pitch: 0.96,
                view_mode: StrategyCameraViewModeSnap::Strategy,
            },
        };
        let json = serde_json::to_string_pretty(&file).unwrap();
        let parsed: LevelCameraFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, file);
    }
}

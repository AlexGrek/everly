//! Serializable actor snapshots for level save/load.
//!
//! Written to `levels/level_{name}/actors.yaml` when the map editor Save button runs.

use std::fs;
use std::io;
use std::path::PathBuf;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::actor::black_bot::{spawn_black_bot_from_snapshot, BotSpecialization, Breakable};
use crate::actor::brain::Brain;
use crate::actor::charge::Charge;
use crate::actor::{ActorMoveBuffer, ActorMovementError, ActorObject, ActorState, LevelActor};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::passability::DynamicPassabilityMap;
use crate::map::level::LevelName;
use crate::menu::main_menu::GameState;

pub const ACTOR_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerVec2 {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerIVec2 {
    pub x: i32,
    pub y: i32,
}

impl From<Vec2> for SerVec2 {
    fn from(v: Vec2) -> Self {
        Self { x: v.x, y: v.y }
    }
}

impl From<SerVec2> for Vec2 {
    fn from(v: SerVec2) -> Self {
        Vec2::new(v.x, v.y)
    }
}

impl From<IVec2> for SerIVec2 {
    fn from(v: IVec2) -> Self {
        Self { x: v.x, y: v.y }
    }
}

impl From<SerIVec2> for IVec2 {
    fn from(v: SerIVec2) -> Self {
        IVec2::new(v.x, v.y)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActorMoveBufferSnap {
    pub tile_delta: SerVec2,
    pub subtile_shift: SerIVec2,
    pub rotation_shift: f32,
}

impl From<&ActorMoveBuffer> for ActorMoveBufferSnap {
    fn from(b: &ActorMoveBuffer) -> Self {
        Self {
            tile_delta: b.tile_delta.into(),
            subtile_shift: SerIVec2 {
                x: b.subtile_shift.x,
                y: b.subtile_shift.y,
            },
            rotation_shift: b.rotation_shift,
        }
    }
}

impl From<ActorMoveBufferSnap> for ActorMoveBuffer {
    fn from(b: ActorMoveBufferSnap) -> Self {
        Self {
            tile_delta: b.tile_delta.into(),
            subtile_shift: IVec2::new(b.subtile_shift.x, b.subtile_shift.y),
            rotation_shift: b.rotation_shift,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActorMovementErrorSnap {
    BlockedByOccupancy {
        world_subtile_x: i32,
        world_subtile_y: i32,
    },
    BlockedByStatic {
        world_subtile_x: i32,
        world_subtile_y: i32,
    },
    InvalidRadius(i32),
}

impl From<&ActorMovementError> for ActorMovementErrorSnap {
    fn from(e: &ActorMovementError) -> Self {
        match e {
            ActorMovementError::BlockedByOccupancy {
                world_subtile_x,
                world_subtile_y,
            } => Self::BlockedByOccupancy {
                world_subtile_x: *world_subtile_x,
                world_subtile_y: *world_subtile_y,
            },
            ActorMovementError::BlockedByStatic {
                world_subtile_x,
                world_subtile_y,
            } => Self::BlockedByStatic {
                world_subtile_x: *world_subtile_x,
                world_subtile_y: *world_subtile_y,
            },
            ActorMovementError::InvalidRadius(r) => Self::InvalidRadius(*r),
        }
    }
}

impl From<ActorMovementErrorSnap> for ActorMovementError {
    fn from(e: ActorMovementErrorSnap) -> Self {
        match e {
            ActorMovementErrorSnap::BlockedByOccupancy {
                world_subtile_x,
                world_subtile_y,
            } => Self::BlockedByOccupancy {
                world_subtile_x,
                world_subtile_y,
            },
            ActorMovementErrorSnap::BlockedByStatic {
                world_subtile_x,
                world_subtile_y,
            } => Self::BlockedByStatic {
                world_subtile_x,
                world_subtile_y,
            },
            ActorMovementErrorSnap::InvalidRadius(r) => Self::InvalidRadius(r),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActorStateSnap {
    pub center: SerVec2,
    pub radius_subtiles: i32,
    pub rotation: f32,
    pub move_buffer: ActorMoveBufferSnap,
    pub last_movement_error: Option<ActorMovementErrorSnap>,
    pub last_accepted_center_subtile: Option<SerIVec2>,
    pub last_accepted_radius_subtiles: i32,
}

impl From<&ActorState> for ActorStateSnap {
    fn from(s: &ActorState) -> Self {
        Self {
            center: s.center.into(),
            radius_subtiles: s.radius_subtiles,
            rotation: s.rotation,
            move_buffer: (&s.move_buffer).into(),
            last_movement_error: s.last_movement_error.as_ref().map(Into::into),
            last_accepted_center_subtile: s.last_accepted_center_subtile.map(Into::into),
            last_accepted_radius_subtiles: s.last_accepted_radius_subtiles,
        }
    }
}

impl From<ActorStateSnap> for ActorState {
    fn from(s: ActorStateSnap) -> Self {
        Self {
            center: s.center.into(),
            radius_subtiles: s.radius_subtiles,
            rotation: s.rotation,
            move_buffer: s.move_buffer.into(),
            last_movement_error: s.last_movement_error.map(Into::into),
            last_accepted_center_subtile: s.last_accepted_center_subtile.map(Into::into),
            last_accepted_radius_subtiles: s.last_accepted_radius_subtiles,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BreakablePartSnap {
    pub wear: f32,
    pub broken: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BreakableSnap {
    pub movement_engine: BreakablePartSnap,
    pub control_plane: BreakablePartSnap,
    pub sensory_system: BreakablePartSnap,
}

impl Default for BreakableSnap {
    fn default() -> Self {
        let fresh = || BreakablePartSnap { wear: 0.0, broken: false };
        Self {
            movement_engine: fresh(),
            control_plane: fresh(),
            sensory_system: fresh(),
        }
    }
}

/// Persisted brain state for a BlackBot. The behavior set is fixed by the actor
/// type, so only the RNG seed is stored; the brain re-plans from scratch on load.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BlackBotBrainSnap {
    #[serde(default)]
    pub rng_seed: u64,
}

/// Charge level for actors saved before charge persistence existed: a missing
/// `charge` field loads as full so older `actors.yaml` files keep working.
fn default_charge() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SavedActor {
    BlackBot {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        name: String,
        state: ActorStateSnap,
        #[serde(default)]
        brain: BlackBotBrainSnap,
        #[serde(default = "default_charge")]
        charge: f32,
        #[serde(default)]
        breakable: BreakableSnap,
        /// Behavior + ring specialization; missing in older saves loads as
        /// [`BotSpecialization::DoNothing`].
        #[serde(default)]
        specialization: BotSpecialization,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LevelActorsFile {
    pub version: u32,
    pub actors: Vec<SavedActor>,
}

fn saved_name_from_entity(name: Option<&Name>) -> String {
    name.map(|n| n.to_string()).unwrap_or_default()
}

impl LevelActorsFile {
    pub fn collect(
        black_bots: &Query<(
            &ActorObject,
            &Brain,
            Option<&Charge>,
            Option<&Name>,
            Option<&Breakable>,
            Option<&BotSpecialization>,
        )>,
    ) -> Self {
        let mut actors = Vec::new();
        for (obj, brain, charge, name, breakable, specialization) in black_bots.iter() {
            actors.push(SavedActor::BlackBot {
                name: saved_name_from_entity(name),
                state: obj.inner.state().into(),
                brain: BlackBotBrainSnap { rng_seed: brain.rng_seed() },
                charge: charge.map_or(1.0, |c| c.level),
                breakable: breakable.map(|b| b.to_snapshot()).unwrap_or_default(),
                specialization: specialization.copied().unwrap_or_default(),
            });
        }
        Self {
            version: ACTOR_SNAPSHOT_VERSION,
            actors,
        }
    }
}

pub fn actors_path(level_name: &str) -> PathBuf {
    PathBuf::from("levels")
        .join(format!("level_{level_name}"))
        .join("actors.yaml")
}

pub fn save_level_actors(level_name: &str, file: &LevelActorsFile) -> io::Result<()> {
    let path = actors_path(level_name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let yaml = serde_yaml::to_string(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, yaml)?;
    Ok(())
}

/// Reads `levels/level_{name}/actors.yaml` when present.
pub fn try_load_level_actors(level_name: &str) -> io::Result<Option<LevelActorsFile>> {
    let path = actors_path(level_name);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let file: LevelActorsFile = serde_yaml::from_str(&text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(file))
}

/// Spawns every actor described in `file` and tags each root with [`LevelActor`].
pub fn spawn_level_actors(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    file: &LevelActorsFile,
) {
    for saved in &file.actors {
        match saved {
            SavedActor::BlackBot { name, state, brain, charge, breakable, specialization } => {
                let entity = spawn_black_bot_from_snapshot(
                    commands,
                    meshes,
                    materials,
                    name,
                    state.clone().into(),
                    brain.rng_seed,
                    breakable.clone(),
                    *specialization,
                );
                commands.entity(entity).insert((LevelActor, Charge::new(*charge)));
            }
        }
    }
}

/// Stamps each loaded actor's accepted footprint into the dynamic passability write buffer.
pub(crate) fn restore_loaded_actor_footprints(
    passability: &DynamicPassabilityMap,
    hypermap: &HypermapRuntime,
    actors: &Query<&ActorObject, With<LevelActor>>,
) {
    let static_cache = hypermap.static_subtile_cache.as_ref();
    for obj in actors.iter() {
        let center = match obj.inner.state().last_accepted_center_subtile {
            Some(c) => c,
            None => continue,
        };
        let radius = obj.inner.state().radius_subtiles;
        let blocked = obj.inner.blocked_flags();
        let _ = passability.try_update_footprint(center, radius, None, blocked, static_cache);
    }
    passability.flush();
}

fn load_level_actors_on_enter(
    mut commands: Commands,
    level: Res<LevelName>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let level_name = level.0.as_str();
    let file = match try_load_level_actors(level_name) {
        Ok(Some(f)) => f,
        Ok(None) => return,
        Err(e) => {
            warn!("failed to read `levels/level_{level_name}/actors.yaml`: {e}");
            return;
        }
    };
    if file.version != ACTOR_SNAPSHOT_VERSION {
        warn!(
            "actors.yaml version {} (expected {ACTOR_SNAPSHOT_VERSION}); loading anyway",
            file.version
        );
    }
    let count = file.actors.len();
    spawn_level_actors(&mut commands, &mut meshes, &mut materials, &file);
    info!("loaded {count} actor(s) from `levels/level_{level_name}/actors.yaml`");
}

fn restore_loaded_actor_footprints_system(
    passability: Res<DynamicPassabilityMap>,
    hypermap: Res<HypermapRuntime>,
    actors: Query<&ActorObject, With<LevelActor>>,
) {
    restore_loaded_actor_footprints(&passability, &hypermap, &actors);
}

pub struct ActorSnapshotPlugin;

impl Plugin for ActorSnapshotPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            OnEnter(GameState::InGame),
            (
                load_level_actors_on_enter,
                restore_loaded_actor_footprints_system,
            )
                .chain()
                .after(crate::map::hypermap_world::setup_hypermap_runtime),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_state_snap_round_trip() {
        let state = ActorState {
            center: Vec2::new(10.5, 20.25),
            radius_subtiles: 2,
            rotation: 1.5,
            move_buffer: ActorMoveBuffer {
                tile_delta: Vec2::new(0.1, 0.0),
                subtile_shift: IVec2::new(1, 0),
                rotation_shift: 0.05,
            },
            last_movement_error: Some(ActorMovementError::BlockedByStatic {
                world_subtile_x: 3,
                world_subtile_y: 4,
            }),
            last_accepted_center_subtile: Some(IVec2::new(50, 60)),
            last_accepted_radius_subtiles: 2,
            next_waypoint_hint: None,
            field_main_tile: None,
            dirtiness: 0.0,
            shadow: crate::actor::ActorShadow::default(),
        };
        let snap: ActorStateSnap = (&state).into();
        let back: ActorState = snap.into();
        assert_eq!(back.center, state.center);
        assert_eq!(back.radius_subtiles, state.radius_subtiles);
        assert_eq!(back.move_buffer.tile_delta, state.move_buffer.tile_delta);
        assert_eq!(back.move_buffer.subtile_shift, state.move_buffer.subtile_shift);
        assert_eq!(back.last_movement_error, state.last_movement_error);
        assert_eq!(
            back.last_accepted_center_subtile,
            state.last_accepted_center_subtile
        );
    }

    #[test]
    fn saved_actor_deserializes_missing_name_as_empty() {
        // Flush-left so the block mapping starts at column 0 (no root indentation).
        let yaml = r#"
type: black_bot
state:
  center: { x: 0.0, y: 0.0 }
  radius_subtiles: 2
  rotation: 0.0
  move_buffer:
    tile_delta: { x: 0.0, y: 0.0 }
    subtile_shift: { x: 0, y: 0 }
    rotation_shift: 0.0
  last_movement_error: null
  last_accepted_center_subtile: null
  last_accepted_radius_subtiles: 2
brain:
  rng_seed: 1
"#;
        let actor: SavedActor = serde_yaml::from_str(yaml).unwrap();
        let SavedActor::BlackBot { name, charge, breakable, .. } = actor;
        assert_eq!(name, "");
        // No `charge` field in the YAML above → defaults to full.
        assert_eq!(charge, 1.0);
        // No `breakable` field → defaults to fresh (0 wear, not broken).
        assert_eq!(breakable, BreakableSnap::default());
    }

    #[test]
    fn level_actors_file_yaml_round_trip() {
        let file = LevelActorsFile {
            version: ACTOR_SNAPSHOT_VERSION,
            actors: vec![
                SavedActor::BlackBot {
                    name: String::new(),
                    state: ActorStateSnap {
                        center: SerVec2 { x: 3.0, y: 4.0 },
                        radius_subtiles: 2,
                        rotation: 0.0,
                        move_buffer: ActorMoveBufferSnap {
                            tile_delta: SerVec2 { x: 0.0, y: 0.0 },
                            subtile_shift: SerIVec2 { x: 0, y: 0 },
                            rotation_shift: 0.0,
                        },
                        last_movement_error: None,
                        last_accepted_center_subtile: Some(SerIVec2 { x: 15, y: 20 }),
                        last_accepted_radius_subtiles: 2,
                    },
                    brain: BlackBotBrainSnap { rng_seed: 99 },
                    charge: 0.4,
                    breakable: BreakableSnap::default(),
                    specialization: BotSpecialization::Patrol,
                },
            ],
        };
        let yaml = serde_yaml::to_string(&file).unwrap();
        let parsed: LevelActorsFile = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, file);
    }
}

//! Ground plane split around **holes** (no mesh). [`WATER_SURFACE_Y`] sits under the
//! nominal play plane so [`bevy_water`](https://crates.io/crates/bevy_water) fills each
//! opening.

use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::PlaneMeshBuilder;
use bevy::prelude::*;
use bevy_water::water::material::{StandardWaterMaterial, WaterMaterial};
use bevy_water::{
    setup_water, WaterQuality, WaterSettings, WaterTile, WaterTiles, WaveDirection,
};

/// Marker for the world's ground entities (each cell is separate mesh).
#[derive(Component, Debug)]
pub struct Ground;

/// Edge length of the ground playfield, in world units (holes are inside this square).
pub const GROUND_SIZE: f32 = 200.0;

/// World Y of the procedural water surface (below the ground plane at `y = 0`).
pub const WATER_SURFACE_Y: f32 = -2.4;

/// Tessellation cell size for ground quads (and hole tests).
const GROUND_CELL: f32 = 20.0;

/// Large axis-aligned openings in the ground; water is spawned for each.
#[derive(Clone, Copy, Debug)]
pub struct GroundHole {
    pub min_x: f32,
    pub max_x: f32,
    pub min_z: f32,
    pub max_z: f32,
}

/// Kept strictly **inside** the integer column playfield `±MAP_GRID_HALF` on each axis
/// (see `boxes.rs`), or every `(gx, gz)` would be culled and `COLUMN_COUNT` could not be met.
pub const GROUND_HOLES: &[GroundHole] = &[
    GroundHole {
        min_x: -30.0,
        max_x: 28.0,
        min_z: -32.0,
        max_z: 26.0,
    },
    GroundHole {
        min_x: 14.0,
        max_x: 36.0,
        min_z: 14.0,
        max_z: 36.0,
    },
];

/// True if a world point on the XZ plane lies inside any configured hole.
pub fn xz_in_ground_hole(x: f32, z: f32) -> bool {
    GROUND_HOLES.iter().any(|h| {
        x >= h.min_x && x <= h.max_x && z >= h.min_z && z <= h.max_z
    })
}

pub struct GroundPlugin;

impl Plugin for GroundPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_ground)
            .add_systems(Startup, spawn_hole_water.after(setup_water));
    }
}

fn spawn_ground(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.96, 0.96, 0.98),
        emissive: LinearRgba::BLACK,
        perceptual_roughness: 0.86,
        metallic: 0.0,
        reflectance: 0.48,
        fog_enabled: false,
        ..default()
    });

    let half = GROUND_SIZE * 0.5;
    let n = (GROUND_SIZE / GROUND_CELL).round() as i32;
    assert!(
        (n as f32 * GROUND_CELL - GROUND_SIZE).abs() < 0.001,
        "GROUND_SIZE must be divisible by GROUND_CELL"
    );

    for ix in 0..n {
        for iz in 0..n {
            let x0 = -half + ix as f32 * GROUND_CELL;
            let z0 = -half + iz as f32 * GROUND_CELL;
            if cell_touches_any_hole(x0, z0, GROUND_CELL) {
                continue;
            }
            let cx = x0 + GROUND_CELL * 0.5;
            let cz = z0 + GROUND_CELL * 0.5;
            let cell_mesh = meshes.add(
                Plane3d::default()
                    .mesh()
                    .size(GROUND_CELL, GROUND_CELL),
            );
            commands.spawn((
                Name::new(format!("Ground cell {ix},{iz}")),
                Ground,
                Mesh3d(cell_mesh),
                MeshMaterial3d(material.clone()),
                Transform::from_xyz(cx, 0.0, cz),
            ));
        }
    }
}

fn cell_touches_any_hole(x0: f32, z0: f32, size: f32) -> bool {
    let x1 = x0 + size;
    let z1 = z0 + size;
    GROUND_HOLES.iter().any(|h| {
        !(x1 <= h.min_x || x0 >= h.max_x || z1 <= h.min_z || z0 >= h.max_z)
    })
}

fn spawn_hole_water(
    mut commands: Commands,
    settings: Res<WaterSettings>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardWaterMaterial>>,
) {
    let normalized_dir = settings.wave_direction.normalize_or_zero();
    let quality: WaterQuality = settings.water_quality;

    commands
        .spawn((WaterTiles, Name::new("Lake water")))
        .with_children(|parent| {
            for (hi, hole) in GROUND_HOLES.iter().enumerate() {
                let width = hole.max_x - hole.min_x;
                let depth = hole.max_z - hole.min_z;
                let cx = (hole.min_x + hole.max_x) * 0.5;
                let cz = (hole.min_z + hole.max_z) * 0.5;
                let coord_offset = Vec2::new(hole.min_x, hole.min_z);
                let coord_scale = Vec2::new(width, depth);

                let mut plane_builder = PlaneMeshBuilder::from_size(Vec2::new(width, depth));
                plane_builder = match quality {
                    WaterQuality::Basic | WaterQuality::Medium => plane_builder,
                    WaterQuality::High => {
                        let sub = ((width.max(depth)) as u32 / 16).clamp(1, 24);
                        plane_builder.subdivisions(sub)
                    }
                    WaterQuality::Ultra => {
                        let sub = ((width.max(depth)) as u32 / 4).clamp(4, 48);
                        plane_builder.subdivisions(sub)
                    }
                };

                let mesh = Mesh3d(meshes.add(plane_builder));
                let tile_hash = (hi as i32).wrapping_mul(73856093) as f32;
                let tile_offset = (tile_hash.abs() % 1000.0) / 1000.0 * 0.3;

                let mut wave_dir = WaveDirection::with_duration(
                    settings.wave_direction,
                    settings.wave_direction_blend_duration,
                );
                wave_dir.tile_offset = tile_offset;

                let material = MeshMaterial3d(materials.add(StandardWaterMaterial {
                    base: StandardMaterial {
                        base_color: settings.base_color,
                        alpha_mode: settings.alpha_mode,
                        perceptual_roughness: 0.22,
                        ..default()
                    },
                    extension: WaterMaterial {
                        amplitude: settings.amplitude,
                        clarity: settings.clarity,
                        deep_color: settings.deep_color,
                        shallow_color: settings.shallow_color,
                        edge_color: settings.edge_color,
                        edge_scale: settings.edge_scale,
                        coord_offset,
                        coord_scale,
                        wave_dir_a: normalized_dir,
                        wave_dir_b: normalized_dir,
                        wave_blend: 1.0,
                        quality: settings.water_quality.into(),
                    },
                }));

                let mut tile_bundle = parent.spawn((
                    WaterTile {
                        offset: coord_offset,
                    },
                    Name::new(format!("Lake surface {hi}")),
                    mesh,
                    material,
                    wave_dir,
                    Transform::from_xyz(cx, settings.height, cz),
                    NotShadowCaster,
                ));

                match quality {
                    WaterQuality::Basic | WaterQuality::Medium => {
                        tile_bundle.insert(NotShadowReceiver);
                    }
                    _ => {}
                };
            }
        });
}

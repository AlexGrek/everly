//! **Strict integer grid** on the XZ plane: each column sits on a single cell
//! `(gx, gz) ∈ ℤ²` with **1×1** footprint and a **discrete integer height** in
//! \[`MIN_COLUMN_HEIGHT`, `MAX_COLUMN_HEIGHT`\] world units. Exactly `COLUMN_COUNT`
//! cells are filled, chosen by a **uniform random sample without replacement**
//! over the full `MAP_GRID_HALF` playfield (seeded, reproducible).
//! Most columns are dark, a minority use emissive materials. Emissive does not
//! light other surfaces in PBR, so each **glowing** column gets a matching
//! [`PointLight`] at a **fixed world position** on the ground under that cell
//! (not parented under the scaled mesh, so placement stays correct).

use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use crate::ground::{xz_in_ground_hole, GROUND_SIZE};

/// Marker for grid scenery cubes.
#[derive(Component, Debug)]
pub struct ScatterBox;

const RNG_SEED: u64 = 0xE7E2_1FE7_u64;

/// Integer half-extent of the **playfield** on each axis (matches the old dense map).
/// Columns are sampled on integer coordinates inside `[-MAP_GRID_HALF, MAP_GRID_HALF]`;
/// clamped to the ground size.
const MAP_GRID_HALF: i32 = 36;

/// Number of occupied cells (no overlaps); each at a random integer grid position.
const COLUMN_COUNT: usize = 121;

/// Number of distinct dark / glow material presets (shared across all tiles).
const PALETTE_SLOTS: usize = 24;

/// Fraction of cells that use a glowing emissive preset (rest are dark).
const GLOW_FRACTION: f64 = 0.34;

/// Discrete column heights (world units), inclusive integers only.
const MIN_COLUMN_HEIGHT: i32 = 3;
const MAX_COLUMN_HEIGHT: i32 = 8;

pub struct BoxesPlugin;

impl Plugin for BoxesPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_boxes);
    }
}

fn cell_rng(gx: i32, gz: i32) -> StdRng {
    let x = gx as u64;
    let z = gz as u64;
    let h = RNG_SEED ^ x.wrapping_mul(0x9E37_79B9_85F3_7D87) ^ z.wrapping_mul(0xC2B2_AE3D_27D4_F4F5);
    StdRng::seed_from_u64(h)
}

fn spawn_boxes(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let max_half = (GROUND_SIZE * 0.5) as i32 - 1;
    let map_half = MAP_GRID_HALF.min(max_half);
    let span = 2 * map_half + 1;
    let cell_capacity = (span as usize).saturating_mul(span as usize);
    assert!(
        cell_capacity >= COLUMN_COUNT,
        "integer playfield must be large enough before hole culling"
    );

    let unit_mesh = meshes.add(Cuboid::new(1.0, 1.0, 1.0));

    let mut setup = StdRng::seed_from_u64(RNG_SEED);
    let mut dark_handles = Vec::with_capacity(PALETTE_SLOTS);
    let mut glow_handles = Vec::with_capacity(PALETTE_SLOTS);
    let mut glow_colors: Vec<Color> = Vec::with_capacity(PALETTE_SLOTS);

    for slot in 0..PALETTE_SLOTS {
        dark_handles.push(materials.add(StandardMaterial {
            base_color: Color::srgb(0.97, 0.97, 0.98),
            emissive: LinearRgba::BLACK,
            perceptual_roughness: 0.88,
            metallic: 0.0,
            reflectance: 0.48,
            fog_enabled: true,
            alpha_mode: AlphaMode::Opaque,
            ..default()
        }));

        let hue = (slot as f32 / PALETTE_SLOTS as f32) * 360.0;
        let glow_surface = Color::hsl(
            hue + setup.gen_range(-14.0_f32..14.0_f32),
            setup.gen_range(0.62_f32..0.95_f32),
            setup.gen_range(0.12_f32..0.22_f32),
        );
        glow_handles.push(materials.add(StandardMaterial {
            base_color: glow_surface,
            emissive: glow_surface.to_linear() * 52.0,
            perceptual_roughness: 0.76,
            metallic: 0.07,
            reflectance: 0.34,
            fog_enabled: false,
            alpha_mode: AlphaMode::Opaque,
            ..default()
        }));
        glow_colors.push(glow_surface);
    }

    let mut cells: Vec<(i32, i32)> = Vec::with_capacity(cell_capacity);
    for gx in -map_half..=map_half {
        for gz in -map_half..=map_half {
            if xz_in_ground_hole(gx as f32, gz as f32) {
                continue;
            }
            cells.push((gx, gz));
        }
    }
    cells.shuffle(&mut setup);

    assert!(
        cells.len() >= COLUMN_COUNT,
        "after removing ground holes, need at least COLUMN_COUNT cells for columns"
    );

    for &(gx, gz) in cells.iter().take(COLUMN_COUNT) {
        let mut rng = cell_rng(gx, gz);
        let glowing = rng.gen_bool(GLOW_FRACTION);
        let slot = rng.gen_range(0..PALETTE_SLOTS);
        let mat = if glowing {
            glow_handles[slot].clone()
        } else {
            dark_handles[slot].clone()
        };
        let h = rng.gen_range(MIN_COLUMN_HEIGHT..=MAX_COLUMN_HEIGHT);
        let y = h as f32 * 0.5;
        commands.spawn((
            Name::new(format!("GridTile {gx},{gz} h={h}")),
            ScatterBox,
            Mesh3d(unit_mesh.clone()),
            MeshMaterial3d(mat),
            Transform {
                translation: Vec3::new(gx as f32, y, gz as f32),
                rotation: Quat::IDENTITY,
                scale: Vec3::new(1.0, h as f32, 1.0),
            },
        ));

        if glowing {
            let color = glow_colors[slot];
            // World-space ground contact: avoids wrong positions when the light was a scaled child.
            commands.spawn((
                Name::new(format!("Column spill {gx},{gz}")),
                PointLight {
                    color,
                    intensity: 180_000.0,
                    range: 48.0,
                    radius: 1.2,
                    shadows_enabled: true,
                    ..default()
                },
                Transform::from_xyz(gx as f32, 0.55, gz as f32),
            ));
        }
    }
}

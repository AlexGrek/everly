//! Text-driven world map parsing and rendering.
//!
//! Encoding uses two characters per map cell:
//! - `..` = void
//! - `__` = road
//! - `wn`, `ws`, `we`, `ww` = wall on north/south/east/west edge

use std::fmt::{Display, Formatter};
use std::ops::Index;
use std::path::Path;
use std::sync::Arc;

use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::PlaneMeshBuilder;
use bevy::prelude::*;
use bevy_water::water::material::{StandardWaterMaterial, WaterMaterial};
use bevy_water::{setup_water, WaterQuality, WaterSettings, WaterTile, WaterTiles, WaveDirection};

/// Vertical position for map water so void cells reveal it.
pub const WATER_SURFACE_Y: f32 = -0.25;

const WALL_THICKNESS: f32 = 0.2; // 1/5 of a cell
const WALL_HEIGHT: f32 = 1.0;
const WATER_MARGIN: f32 = 24.0;

const WORLD_MAP_FILE_PATH: &str = "world_map.txt";

/// Sides a wall can occupy inside a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WallSide {
    North,
    South,
    East,
    West,
}

/// High-level map cell type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellType {
    Void,
    Road,
    Wall(WallSide),
}

/// Single cell object stored by reference in the map.
#[derive(Debug)]
pub struct Cell {
    cell_type: CellType,
}

impl Cell {
    pub fn new(cell_type: CellType) -> Self {
        Self { cell_type }
    }

    pub fn get_passability(&self) -> f32 {
        match self.cell_type {
            CellType::Void | CellType::Wall(_) => 0.0,
            CellType::Road => 1.0,
        }
    }

    pub fn get_cell_type(&self) -> CellType {
        self.cell_type
    }
}

pub type CellRef = Arc<Cell>;

/// Parse failures for the compact two-character-per-cell format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapParseError {
    EmptyMap,
    OddLineLength { row: usize, len: usize },
    NonRectangular { row: usize, expected: usize, found: usize },
    InvalidToken { row: usize, col: usize, token: String },
}

impl Display for MapParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyMap => write!(f, "map contains no rows"),
            Self::OddLineLength { row, len } => {
                write!(f, "row {row} has odd char count {len}; expected pairs")
            }
            Self::NonRectangular {
                row,
                expected,
                found,
            } => write!(
                f,
                "row {row} has {found} cells but expected {expected} cells"
            ),
            Self::InvalidToken { row, col, token } => {
                write!(f, "invalid token `{token}` at row {row}, col {col}")
            }
        }
    }
}

impl std::error::Error for MapParseError {}

/// One 2D floor map; future world maps can stack multiple floors.
#[derive(Debug, Clone)]
pub struct WorldMapFloor {
    width: usize,
    height: usize,
    cells: Vec<CellRef>,
}

impl WorldMapFloor {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn in_bounds(&self, x: usize, y: usize) -> bool {
        x < self.width && y < self.height
    }

    pub fn get(&self, x: usize, y: usize) -> Option<&CellRef> {
        self.in_bounds(x, y).then(|| &self.cells[self.idx(x, y)])
    }

    pub fn set_cell_type(&mut self, x: usize, y: usize, cell_type: CellType) -> Option<()> {
        if !self.in_bounds(x, y) {
            return None;
        }
        let idx = self.idx(x, y);
        self.cells[idx] = Arc::new(Cell::new(cell_type));
        Some(())
    }

    pub fn iter_xy(&self) -> impl Iterator<Item = (usize, usize, &CellRef)> {
        self.cells
            .iter()
            .enumerate()
            .map(|(i, cell)| (i % self.width, i / self.width, cell))
    }

    pub fn row(&self, y: usize) -> Option<&[CellRef]> {
        if y >= self.height {
            return None;
        }
        let start = y * self.width;
        let end = start + self.width;
        Some(&self.cells[start..end])
    }

    pub fn from_ascii(input: &str) -> Result<Self, MapParseError> {
        let mut width: Option<usize> = None;
        let mut cells = Vec::new();
        let mut row_count = 0usize;

        for (row_index, raw_line) in input.lines().enumerate() {
            let compact: String = raw_line.chars().filter(|c| !c.is_whitespace()).collect();
            if compact.is_empty() {
                continue;
            }
            if compact.len() % 2 != 0 {
                return Err(MapParseError::OddLineLength {
                    row: row_index,
                    len: compact.len(),
                });
            }

            let row_width = compact.len() / 2;
            if let Some(expected) = width {
                if row_width != expected {
                    return Err(MapParseError::NonRectangular {
                        row: row_index,
                        expected,
                        found: row_width,
                    });
                }
            } else {
                width = Some(row_width);
            }

            for (col, chunk) in compact.as_bytes().chunks_exact(2).enumerate() {
                let token = std::str::from_utf8(chunk).expect("2-byte token is valid UTF-8");
                let cell_type = parse_cell_token(token).ok_or_else(|| MapParseError::InvalidToken {
                    row: row_index,
                    col,
                    token: token.to_owned(),
                })?;
                cells.push(Arc::new(Cell::new(cell_type)));
            }
            row_count += 1;
        }

        let width = width.ok_or(MapParseError::EmptyMap)?;
        Ok(Self {
            width,
            height: row_count,
            cells,
        })
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }
}

impl Index<(usize, usize)> for WorldMapFloor {
    type Output = CellRef;

    fn index(&self, index: (usize, usize)) -> &Self::Output {
        let (x, y) = index;
        &self.cells[self.idx(x, y)]
    }
}

fn parse_cell_token(token: &str) -> Option<CellType> {
    match token {
        ".." => Some(CellType::Void),
        "__" => Some(CellType::Road),
        "wn" => Some(CellType::Wall(WallSide::North)),
        "ws" => Some(CellType::Wall(WallSide::South)),
        "we" => Some(CellType::Wall(WallSide::East)),
        "ww" => Some(CellType::Wall(WallSide::West)),
        _ => None,
    }
}

#[derive(Resource, Debug, Clone)]
pub struct ActiveWorldMapFloor(pub WorldMapFloor);

pub struct WorldMapPlugin;

impl Plugin for WorldMapPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_world_map).add_systems(
            Startup,
            spawn_world_water.after(spawn_world_map).after(setup_water),
        );
    }
}

fn spawn_world_map(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let map_text = std::fs::read_to_string(Path::new(WORLD_MAP_FILE_PATH))
        .unwrap_or_else(|err| panic!("failed to read `{WORLD_MAP_FILE_PATH}`: {err}"));
    let map = WorldMapFloor::from_ascii(&map_text)
        .unwrap_or_else(|err| panic!("failed to parse `{WORLD_MAP_FILE_PATH}`: {err}"));
    commands.insert_resource(ActiveWorldMapFloor(map.clone()));

    let map_w = map.width() as f32;
    let map_h = map.height() as f32;
    let origin = Vec2::new(-map_w * 0.5, -map_h * 0.5);

    let floor_mesh = meshes.add(Plane3d::default().mesh().size(1.0, 1.0));
    let wall_ns_mesh = meshes.add(Cuboid::new(1.0, WALL_HEIGHT, WALL_THICKNESS));
    let wall_ew_mesh = meshes.add(Cuboid::new(WALL_THICKNESS, WALL_HEIGHT, 1.0));

    let road_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.36, 0.36, 0.38),
        perceptual_roughness: 0.98,
        metallic: 0.0,
        ..default()
    });
    let wall_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.78, 0.79, 0.82),
        perceptual_roughness: 0.72,
        metallic: 0.02,
        ..default()
    });

    for (x, y, cell) in map.iter_xy() {
        let cell_type = cell.get_cell_type();
        if cell_type == CellType::Void {
            continue;
        }

        let world_x = origin.x + x as f32 + 0.5;
        let world_z = origin.y + y as f32 + 0.5;

        commands.spawn((
            Name::new(format!("Map floor {x},{y}")),
            Mesh3d(floor_mesh.clone()),
            MeshMaterial3d(road_material.clone()),
            Transform::from_xyz(world_x, 0.0, world_z),
        ));

        if let CellType::Wall(side) = cell_type {
            let (mesh, offset) = match side {
                WallSide::North => (wall_ns_mesh.clone(), Vec3::new(0.0, WALL_HEIGHT * 0.5, -0.4)),
                WallSide::South => (wall_ns_mesh.clone(), Vec3::new(0.0, WALL_HEIGHT * 0.5, 0.4)),
                WallSide::East => (wall_ew_mesh.clone(), Vec3::new(0.4, WALL_HEIGHT * 0.5, 0.0)),
                WallSide::West => (wall_ew_mesh.clone(), Vec3::new(-0.4, WALL_HEIGHT * 0.5, 0.0)),
            };

            commands.spawn((
                Name::new(format!("Map wall {x},{y}")),
                Mesh3d(mesh),
                MeshMaterial3d(wall_material.clone()),
                Transform::from_translation(Vec3::new(world_x, 0.0, world_z) + offset),
            ));
        }
    }
}

fn spawn_world_water(
    mut commands: Commands,
    map: Res<ActiveWorldMapFloor>,
    settings: Res<WaterSettings>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardWaterMaterial>>,
) {
    let width = map.0.width() as f32 + WATER_MARGIN * 2.0;
    let depth = map.0.height() as f32 + WATER_MARGIN * 2.0;
    let coord_offset = Vec2::new(-width * 0.5, -depth * 0.5);
    let coord_scale = Vec2::new(width, depth);
    let normalized_dir = settings.wave_direction.normalize_or_zero();
    let quality: WaterQuality = settings.water_quality;

    let mut plane_builder = PlaneMeshBuilder::from_size(Vec2::new(width, depth));
    plane_builder = match quality {
        WaterQuality::Basic | WaterQuality::Medium => plane_builder,
        WaterQuality::High => {
            let sub = (width.max(depth) as u32 / 16).clamp(1, 24);
            plane_builder.subdivisions(sub)
        }
        WaterQuality::Ultra => {
            let sub = (width.max(depth) as u32 / 4).clamp(4, 48);
            plane_builder.subdivisions(sub)
        }
    };

    let mesh = Mesh3d(meshes.add(plane_builder));
    let mut wave_dir = WaveDirection::with_duration(
        settings.wave_direction,
        settings.wave_direction_blend_duration,
    );
    wave_dir.tile_offset = 0.0;

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

    commands
        .spawn((WaterTiles, Name::new("WorldMap water layer")))
        .with_children(|parent| {
            let mut tile = parent.spawn((
                WaterTile {
                    offset: coord_offset,
                },
                Name::new("WorldMap water tile"),
                mesh,
                material,
                wave_dir,
                Transform::from_xyz(0.0, settings.height, 0.0),
                NotShadowCaster,
            ));

            match quality {
                WaterQuality::Basic | WaterQuality::Medium => {
                    tile.insert(NotShadowReceiver);
                }
                _ => {}
            };
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rows_into_cells() {
        let map = WorldMapFloor::from_ascii(
            "\
            ..__\n\
            wnws\n",
        )
        .expect("map should parse");

        assert_eq!(map.width(), 2);
        assert_eq!(map.height(), 2);
        assert_eq!(map[(0, 0)].get_cell_type(), CellType::Void);
        assert_eq!(map[(1, 0)].get_cell_type(), CellType::Road);
        assert_eq!(map[(0, 1)].get_cell_type(), CellType::Wall(WallSide::North));
        assert_eq!(map[(1, 1)].get_cell_type(), CellType::Wall(WallSide::South));
    }

    #[test]
    fn reports_invalid_token() {
        let err = WorldMapFloor::from_ascii("..xy\n").expect_err("must reject unknown token");
        assert!(matches!(err, MapParseError::InvalidToken { .. }));
    }
}

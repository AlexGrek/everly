//! Text-driven world map parsing and rendering.
//!
//! Encoding uses two characters per map cell:
//! - `..` = void, `__` = road
//! - Walls: bitmask over north / south / east / west edges of the cell (see
//!   [`MASK_NORTH`], [`MASK_SOUTH`], [`MASK_EAST`], [`MASK_WEST`]).
//! - `w` + one hex digit (`w1` … `wf`, `wA` … `wF`) = explicit mask 1–15.
//! - Shortcuts `wn`, `ws`, `we`, `ww` = single-edge masks (same as `w1`, `w2`,
//!   `w4`, `w8`). `w0` is invalid.
//! - Corner pillars `c7` / `c9` / `c1` / `c3` = one 0.2×0.2 m wall column in
//!   that cell corner (numpad layout; see [`WallCorner`]).

use std::fmt::{Display, Formatter};
use std::num::NonZeroU8;
use std::ops::Index;
use std::path::Path;
use std::sync::Arc;

use bevy::light::{NotShadowCaster, NotShadowReceiver};
use bevy::mesh::PlaneMeshBuilder;
use bevy::prelude::*;
use bevy_water::water::material::{StandardWaterMaterial, WaterMaterial};
use bevy_water::{setup_water, WaterQuality, WaterSettings, WaterTile, WaterTiles, WaveDirection};

use crate::floor_level::HYPERMAP_WALL_HEIGHT;

/// Vertical position for map water so void cells reveal it.
pub const WATER_SURFACE_Y: f32 = -0.25;

/// Slab thickness perpendicular to the cell edge — **one fifth** of a 1 m × 1 m cell (0.2 m).
pub(crate) const WALL_THICKNESS: f32 = 0.2;

/// Wall height — [`HYPERMAP_WALL_HEIGHT`] (storey spacing is slightly larger; see `floor_level`).
const WALL_HEIGHT: f32 = HYPERMAP_WALL_HEIGHT;
const WATER_MARGIN: f32 = 24.0;

const WORLD_MAP_FILE_PATH: &str = "world_map.txt";

/// North edge of the cell (+Z / “back” in default overhead view).
pub const MASK_NORTH: u8 = 1;
/// South edge.
pub const MASK_SOUTH: u8 = 2;
/// East edge.
pub const MASK_EAST: u8 = 4;
/// West edge.
pub const MASK_WEST: u8 = 8;

/// Non-zero 4-bit mask: which cell edges carry wall geometry (corners and
/// T-junctions are any combination of bits).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WallMask(NonZeroU8);

impl WallMask {
    pub fn from_bits(bits: u8) -> Option<Self> {
        let b = bits & 0x0f;
        NonZeroU8::new(b).map(Self)
    }

    pub fn bits(self) -> u8 {
        self.0.get()
    }
}

/// Which corner of a 1 m × 1 m cell holds a [`CellType::Corner`] pillar (same
/// thickness as wall slabs: [`WALL_THICKNESS`]). Numpad on a north-up map row:
/// `7` NW, `9` NE, `1` SW, `3` SE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WallCorner {
    Nw,
    Ne,
    Sw,
    Se,
}

impl WallCorner {
    /// Cell-center XZ offset for the pillar (matches wall slab [`inset`](for_each_wall_segment)).
    pub fn xz_offset_from_cell_center(self) -> (f32, f32) {
        let inset = 0.5 - WALL_THICKNESS * 0.5;
        match self {
            WallCorner::Nw => (-inset, inset),
            WallCorner::Ne => (inset, inset),
            WallCorner::Sw => (-inset, -inset),
            WallCorner::Se => (inset, -inset),
        }
    }
}

/// High-level map cell type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellType {
    Void,
    Road,
    Wall(WallMask),
    /// Single corner column (footprint [`WALL_THICKNESS`]²) to plug gaps between wall slabs.
    Corner(WallCorner),
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
            CellType::Void | CellType::Wall(_) | CellType::Corner(_) => 0.0,
            CellType::Road => 1.0,
        }
    }

    pub fn get_cell_type(&self) -> CellType {
        self.cell_type
    }
}

pub type CellRef = Arc<Cell>;

/// Optional `>A` / `>B` path endpoints from [`WorldMapFloor::from_ascii_with_path_markers`].
/// Coordinates are zero-based `(x, y)` column-major indices matching [`WorldMapFloor::get`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorldMapPathMarkers {
    pub path_a: Option<(usize, usize)>,
    pub path_b: Option<(usize, usize)>,
}

/// Parse failures for the compact two-character-per-cell format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapParseError {
    EmptyMap,
    OddLineLength { row: usize, len: usize },
    NonRectangular { row: usize, expected: usize, found: usize },
    InvalidToken { row: usize, col: usize, token: String },
    DuplicatePathMarker { label: char, row: usize, col: usize },
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
            Self::DuplicatePathMarker { label, row, col } => {
                write!(
                    f,
                    "duplicate path marker `>{label}` at row {row}, col {col}"
                )
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
        let ParsedAsciiGrid {
            width,
            height,
            cells,
            markers: _,
        } = parse_ascii_grid(input)?;
        Ok(Self::from_cell_types(width, height, cells))
    }

    /// Same grid as [`Self::from_ascii`], plus at most one `>A` and one `>B` marker each
    /// (stored as [`CellType::Road`] in the floor; coordinates are only in [`WorldMapPathMarkers`]).
    pub fn from_ascii_with_path_markers(
        input: &str,
    ) -> Result<(Self, WorldMapPathMarkers), MapParseError> {
        let ParsedAsciiGrid {
            width,
            height,
            cells,
            markers,
        } = parse_ascii_grid(input)?;
        Ok((Self::from_cell_types(width, height, cells), markers))
    }

    fn from_cell_types(width: usize, height: usize, cells: Vec<CellType>) -> Self {
        let cells: Vec<CellRef> = cells
            .into_iter()
            .map(|ct| Arc::new(Cell::new(ct)))
            .collect();
        Self {
            width,
            height,
            cells,
        }
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

fn ascii_hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
enum AsciiPathToken {
    Cell(CellType),
    PathMarkerA,
    PathMarkerB,
}

struct ParsedAsciiGrid {
    width: usize,
    height: usize,
    cells: Vec<CellType>,
    markers: WorldMapPathMarkers,
}

fn parse_ascii_grid(input: &str) -> Result<ParsedAsciiGrid, MapParseError> {
    let mut width: Option<usize> = None;
    let mut cells = Vec::new();
    let mut row_count = 0usize;
    let mut markers = WorldMapPathMarkers::default();

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
            let path_tok = parse_ascii_path_token(token).ok_or_else(|| MapParseError::InvalidToken {
                row: row_index,
                col,
                token: token.to_owned(),
            })?;
            match path_tok {
                AsciiPathToken::Cell(cell_type) => cells.push(cell_type),
                AsciiPathToken::PathMarkerA => {
                    if markers.path_a.replace((col, row_count)).is_some() {
                        return Err(MapParseError::DuplicatePathMarker {
                            label: 'A',
                            row: row_index,
                            col,
                        });
                    }
                    cells.push(CellType::Road);
                }
                AsciiPathToken::PathMarkerB => {
                    if markers.path_b.replace((col, row_count)).is_some() {
                        return Err(MapParseError::DuplicatePathMarker {
                            label: 'B',
                            row: row_index,
                            col,
                        });
                    }
                    cells.push(CellType::Road);
                }
            }
        }
        row_count = row_count.saturating_add(1);
    }

    let width = width.ok_or(MapParseError::EmptyMap)?;
    Ok(ParsedAsciiGrid {
        width,
        height: row_count,
        cells,
        markers,
    })
}

fn parse_ascii_path_token(token: &str) -> Option<AsciiPathToken> {
    match token {
        ">A" => Some(AsciiPathToken::PathMarkerA),
        ">B" => Some(AsciiPathToken::PathMarkerB),
        _ => parse_cell_token(token).map(AsciiPathToken::Cell),
    }
}

fn parse_cell_token(token: &str) -> Option<CellType> {
    let bytes = token.as_bytes();
    if bytes.len() != 2 {
        return None;
    }
    match token {
        ".." => Some(CellType::Void),
        "__" => Some(CellType::Road),
        "wn" => WallMask::from_bits(MASK_NORTH).map(CellType::Wall),
        "ws" => WallMask::from_bits(MASK_SOUTH).map(CellType::Wall),
        "we" => WallMask::from_bits(MASK_EAST).map(CellType::Wall),
        "ww" => WallMask::from_bits(MASK_WEST).map(CellType::Wall),
        "c7" | "C7" => Some(CellType::Corner(WallCorner::Nw)),
        "c9" | "C9" => Some(CellType::Corner(WallCorner::Ne)),
        "c1" | "C1" => Some(CellType::Corner(WallCorner::Sw)),
        "c3" | "C3" => Some(CellType::Corner(WallCorner::Se)),
        _ if bytes[0] == b'w' || bytes[0] == b'W' => {
            let v = ascii_hex_value(bytes[1])?;
            WallMask::from_bits(v).map(CellType::Wall)
        }
        _ => None,
    }
}

/// Invokes `f(sx, sz, ox, oz)` for each wall slab in cell space (`append_box`
/// half-extents and center offsets match `hypermap_world`).
pub(crate) fn for_each_wall_segment(mask_bits: u8, mut f: impl FnMut(f32, f32, f32, f32)) {
    let m = mask_bits & 0x0f;
    if m == 0 {
        return;
    }
    let th = WALL_THICKNESS;
    let inset = 0.5 - th * 0.5;
    let n = (1.0, th, 0.0, -inset);
    let s = (1.0, th, 0.0, inset);
    let e = (th, 1.0, inset, 0.0);
    let w = (th, 1.0, -inset, 0.0);
    if m & MASK_NORTH != 0 {
        let (sx, sz, ox, oz) = n;
        f(sx, sz, ox, oz);
    }
    if m & MASK_SOUTH != 0 {
        let (sx, sz, ox, oz) = s;
        f(sx, sz, ox, oz);
    }
    if m & MASK_EAST != 0 {
        let (sx, sz, ox, oz) = e;
        f(sx, sz, ox, oz);
    }
    if m & MASK_WEST != 0 {
        let (sx, sz, ox, oz) = w;
        f(sx, sz, ox, oz);
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
    let corner_mesh = meshes.add(Cuboid::new(
        WALL_THICKNESS,
        WALL_HEIGHT,
        WALL_THICKNESS,
    ));

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

        if let CellType::Wall(mask) = cell_type {
            let mut part_index = 0usize;
            for_each_wall_segment(mask.bits(), |sx, sz, ox, oz| {
                let use_ns = sz <= sx;
                let mesh = if use_ns {
                    wall_ns_mesh.clone()
                } else {
                    wall_ew_mesh.clone()
                };
                let offset = Vec3::new(ox, WALL_HEIGHT * 0.5, oz);
                commands.spawn((
                    Name::new(format!("Map wall {x},{y}-{part_index}")),
                    Mesh3d(mesh),
                    MeshMaterial3d(wall_material.clone()),
                    Transform::from_translation(Vec3::new(world_x, 0.0, world_z) + offset),
                ));
                part_index += 1;
            });
        }
        if let CellType::Corner(corner) = cell_type {
            let (ox, oz) = corner.xz_offset_from_cell_center();
            let offset = Vec3::new(ox, WALL_HEIGHT * 0.5, oz);
            commands.spawn((
                Name::new(format!("Map corner pillar {x},{y}-{corner:?}")),
                Mesh3d(corner_mesh.clone()),
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
        assert_eq!(
            map[(0, 1)].get_cell_type(),
            CellType::Wall(WallMask::from_bits(MASK_NORTH).unwrap())
        );
        assert_eq!(
            map[(1, 1)].get_cell_type(),
            CellType::Wall(WallMask::from_bits(MASK_SOUTH).unwrap())
        );
    }

    #[test]
    fn parses_hex_wall_mask() {
        let map = WorldMapFloor::from_ascii("w3wF\n").expect("map should parse");
        assert_eq!(
            map[(0, 0)].get_cell_type(),
            CellType::Wall(WallMask::from_bits(MASK_NORTH | MASK_SOUTH).unwrap())
        );
        assert_eq!(
            map[(1, 0)].get_cell_type(),
            CellType::Wall(WallMask::from_bits(0x0f).unwrap())
        );
    }

    #[test]
    fn reports_invalid_token() {
        let err = WorldMapFloor::from_ascii("..xy\n").expect_err("must reject unknown token");
        assert!(matches!(err, MapParseError::InvalidToken { .. }));
    }

    #[test]
    fn rejects_zero_wall_mask() {
        let err = WorldMapFloor::from_ascii("..w0\n").expect_err("w0 must be invalid");
        assert!(matches!(err, MapParseError::InvalidToken { .. }));
    }

    #[test]
    fn path_markers_parse_as_road_with_metadata() {
        let (floor, markers) = WorldMapFloor::from_ascii_with_path_markers(
            "\
            >A____>B\n\
            ",
        )
        .expect("parse");
        assert_eq!(markers.path_a, Some((0, 0)));
        assert_eq!(markers.path_b, Some((3, 0)));
        assert_eq!(floor[(0, 0)].get_cell_type(), CellType::Road);
        assert_eq!(floor[(3, 0)].get_cell_type(), CellType::Road);
    }

    #[test]
    fn from_ascii_accepts_markers_as_floor_without_metadata() {
        let floor = WorldMapFloor::from_ascii(">A__>B\n").expect("parse");
        assert_eq!(floor.width(), 3);
        assert!(floor.iter_xy().all(|(_, _, c)| c.get_cell_type() == CellType::Road));
    }

    #[test]
    fn duplicate_path_marker_errors() {
        let err = WorldMapFloor::from_ascii_with_path_markers(">A__>A\n").expect_err("dup A");
        assert!(matches!(
            err,
            MapParseError::DuplicatePathMarker { label: 'A', .. }
        ));
    }

    #[test]
    fn parses_corner_pillar_tokens() {
        let map = WorldMapFloor::from_ascii("c7c9\nc1c3\n").expect("parse");
        assert_eq!(map[(0, 0)].get_cell_type(), CellType::Corner(WallCorner::Nw));
        assert_eq!(map[(1, 0)].get_cell_type(), CellType::Corner(WallCorner::Ne));
        assert_eq!(map[(0, 1)].get_cell_type(), CellType::Corner(WallCorner::Sw));
        assert_eq!(map[(1, 1)].get_cell_type(), CellType::Corner(WallCorner::Se));
        let upper = WorldMapFloor::from_ascii("C7__\n").expect("parse");
        assert_eq!(
            upper[(0, 0)].get_cell_type(),
            CellType::Corner(WallCorner::Nw)
        );
    }
}

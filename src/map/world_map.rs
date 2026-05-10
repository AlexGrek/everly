//! Text-driven world map parsing.
//!
//! Encoding uses two characters per map cell:
//! - `..` = void, `__` = road
//! - Walls: bitmask over four named edges; each bit selects a thin slab offset
//!   from the cell center in **world XZ** (see [`MASK_NORTH`] … [`MASK_WEST`] and
//!   [`for_each_wall_segment`]).
//! - `w` + one **uppercase** hex digit (`w1` … `w9`, `wA` … `wF`) = explicit
//!   mask 1–15. **Lowercase letters are reserved for the single-edge aliases**
//!   below and are rejected as hex (so `we` is never mask 14 — use `wE`).
//! - Shortcuts `wn`, `ws`, `we`, `ww` = single-edge masks (same as `w1`, `w2`,
//!   `w4`, `w8`). `w0` is invalid.
//! - Corner pillars `c7` / `c9` / `c1` / `c3` = one 0.2×0.2 m wall column in
//!   that cell corner (numpad layout; see [`WallCorner`]).

use std::fmt::{Display, Formatter};
use std::num::NonZeroU8;
use std::ops::Index;
use std::sync::Arc;

/// Vertical position for map water so void cells reveal it.
pub const WATER_SURFACE_Y: f32 = -0.25;

pub(crate) const WORLD_MAP_FILE_PATH: &str = "world_map.txt";

/// Slab thickness perpendicular to the cell edge — **one fifth** of a 1 m × 1 m cell (0.2 m).
pub(crate) const WALL_THICKNESS: f32 = 0.2;

/// Thin wall slab toward **decreasing world Z** (−Z from cell center; `for_each_wall_segment` uses `oz = −inset`).
pub const MASK_NORTH: u8 = 1;
/// Thin slab toward **increasing world Z** (+Z; `oz = +inset`).
pub const MASK_SOUTH: u8 = 2;
/// Thin slab toward **increasing world X** (+X; `ox = +inset`).
pub const MASK_EAST: u8 = 4;
/// Thin slab toward **decreasing world X** (−X; `ox = −inset`).
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

/// Static (geometry-only) passability of a [`CellType`]: `1.0` for [`CellType::Road`],
/// `0.0` for everything else. The mirror written into
/// [`crate::map::hypermap_world::HypermapRuntime::static_passability_map`].
#[inline]
pub fn cell_passability(cell: CellType) -> f32 {
    match cell {
        CellType::Road => 1.0,
        CellType::Void | CellType::Wall(_) | CellType::Corner(_) => 0.0,
    }
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
        cell_passability(self.cell_type)
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

/// Hex digit value for a cell-token byte. **Letters must be uppercase**: `wa`
/// is _not_ accepted as mask 10 — that token would otherwise collide with the
/// single-edge `we` alias semantics. Use uppercase `wA` … `wF` for masks 10–15.
/// (Digits `0`–`9` are the same in both cases and remain unambiguous.)
fn ascii_hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
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

/// Renders a [`CellType`] back as the canonical 2-character token consumed by
/// [`parse_cell_token`]. Inverse of parsing for the level save/load format.
pub(crate) fn cell_to_token(cell: CellType) -> &'static str {
    match cell {
        CellType::Void => "..",
        CellType::Road => "__",
        // Uppercase hex digits for masks 10-15 so they don't collide with the
        // single-edge aliases `wn` / `ws` / `we` / `ww` that the parser checks
        // **before** falling through to the generic `w` + hex-digit branch.
        CellType::Wall(mask) => match mask.bits() & 0x0f {
            0x1 => "w1",
            0x2 => "w2",
            0x3 => "w3",
            0x4 => "w4",
            0x5 => "w5",
            0x6 => "w6",
            0x7 => "w7",
            0x8 => "w8",
            0x9 => "w9",
            0xa => "wA",
            0xb => "wB",
            0xc => "wC",
            0xd => "wD",
            0xe => "wE",
            0xf => "wF",
            _ => unreachable!("WallMask::from_bits enforces non-zero 4-bit value"),
        },
        CellType::Corner(WallCorner::Nw) => "c7",
        CellType::Corner(WallCorner::Ne) => "c9",
        CellType::Corner(WallCorner::Sw) => "c1",
        CellType::Corner(WallCorner::Se) => "c3",
    }
}

pub(crate) fn parse_cell_token(token: &str) -> Option<CellType> {
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
    fn rejects_lowercase_hex_letters() {
        // `wA` is the canonical mask-10 token; lowercase `wa` must not parse —
        // it would otherwise blur the line between the hex form and the
        // single-edge `we` alias and break the "uppercase hex" invariant.
        for token in ["wa", "wb", "wc", "wd", "wf"] {
            let line = format!("{token}\n");
            let err = WorldMapFloor::from_ascii(&line)
                .expect_err(&format!("{token} must be invalid (lowercase hex)"));
            assert!(matches!(err, MapParseError::InvalidToken { .. }));
        }
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

//! On-disk level geometry: `levels/level_{name}/geometry/{chunk_x}_{chunk_y}.txt`.
//!
//! Each file is one chunk (`HYPERMAP_CHUNK_SIZE`²) and all vertical floors that are
//! not entirely void. Format:
//! - Sections `# floor N` where `N` is `0..=9`.
//! - After each header, exactly `HYPERMAP_CHUNK_SIZE` lines; each line has `HYPERMAP_CHUNK_SIZE`
//!   space-separated two-character tokens (same encoding as `world_map.txt`).
//! - Floors omitted from the file are treated as all [`CellType::Void`].

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use bevy::prelude::*;

use crate::map::hypermap::{
    ChunkCoord, Hypermap, HypermapChunk, LocalCoord, HYPERMAP_CHUNK_SIZE, HYPERMAP_FLOOR_COUNT,
};
use crate::map::world_map::{cell_to_token, parse_cell_token, parse_style_token, CellType, TileStyle};

/// Active level folder name under `levels/level_{name}/`. Currently fixed to `"default"`.
#[derive(Resource, Debug, Clone)]
pub struct LevelName(pub String);

impl Default for LevelName {
    fn default() -> Self {
        Self("default".to_string())
    }
}

pub struct LevelPlugin;

impl Plugin for LevelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LevelName>();
    }
}

pub fn geometry_dir(level_name: &str) -> PathBuf {
    PathBuf::from("levels").join(format!("level_{level_name}")).join("geometry")
}

pub fn chunk_geometry_path(level_name: &str, coord: ChunkCoord) -> PathBuf {
    geometry_dir(level_name).join(format!("{}_{}.txt", coord.x, coord.y))
}

fn floor_all_void(chunk: &HypermapChunk<CellType>, floor: i32) -> bool {
    for y in 0..HYPERMAP_CHUNK_SIZE {
        for x in 0..HYPERMAP_CHUNK_SIZE {
            if *chunk.get_local_floor(LocalCoord::new(x, y), floor) != CellType::Void {
                return false;
            }
        }
    }
    true
}

/// Serializes one chunk (all non–all-void floors) to the level geometry text format.
pub fn encode_chunk_geometry(chunk: &HypermapChunk<CellType>) -> String {
    let sz = HYPERMAP_CHUNK_SIZE as usize;
    let mut out = String::new();
    for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
        if floor_all_void(chunk, floor) {
            continue;
        }
        out.push_str(&format!("# floor {floor}\n"));
        for y in 0..sz {
            let mut first = true;
            for x in 0..sz {
                let cell = *chunk.get_local_floor(LocalCoord::new(x as i32, y as i32), floor);
                if !first {
                    out.push(' ');
                }
                first = false;
                out.push_str(cell_to_token(cell));
            }
            out.push('\n');
        }
    }
    out
}

fn clear_chunk_all_void(chunk: &mut HypermapChunk<CellType>) {
    for y in 0..HYPERMAP_CHUNK_SIZE {
        for x in 0..HYPERMAP_CHUNK_SIZE {
            let local = LocalCoord::new(x, y);
            for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
                chunk.set_local_floor(local, floor, CellType::Void);
            }
        }
    }
}

/// Parses geometry text into `(floor, rows)` with each row length `HYPERMAP_CHUNK_SIZE`.
pub fn parse_level_geometry_sections(text: &str) -> Result<Vec<(i32, Vec<Vec<CellType>>)>, String> {
    let sz = HYPERMAP_CHUNK_SIZE as usize;
    let mut sections: Vec<(i32, Vec<Vec<CellType>>)> = Vec::new();
    let mut current_floor: Option<i32> = None;
    let mut rows: Vec<Vec<CellType>> = Vec::new();

    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# floor ") {
            if let Some(f) = current_floor {
                if rows.len() != sz {
                    return Err(format!(
                        "line {}: floor {f} had {} rows (expected {sz})",
                        line_no + 1,
                        rows.len()
                    ));
                }
                sections.push((f, std::mem::take(&mut rows)));
            }
            let n: i32 = rest
                .trim()
                .parse()
                .map_err(|_| format!("line {}: bad floor index `{rest}`", line_no + 1))?;
            if n < 0 || n >= HYPERMAP_FLOOR_COUNT as i32 {
                return Err(format!("line {}: floor {n} out of range", line_no + 1));
            }
            current_floor = Some(n);
            continue;
        }

        let Some(floor) = current_floor else {
            return Err(format!(
                "line {}: expected `# floor N` before grid data",
                line_no + 1
            ));
        };

        if rows.len() >= sz {
            return Err(format!("line {}: too many rows for floor {floor}", line_no + 1));
        }

        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() != sz {
            return Err(format!(
                "line {}: floor {floor} row {} has {} tokens (expected {sz})",
                line_no + 1,
                rows.len(),
                tokens.len()
            ));
        }
        let mut row_cells = Vec::with_capacity(sz);
        for (x, tok) in tokens.iter().enumerate() {
            let cell = parse_cell_token(tok).ok_or_else(|| {
                format!(
                    "line {}: invalid token `{tok}` at floor {floor} row {} col {x}",
                    line_no + 1,
                    rows.len()
                )
            })?;
            row_cells.push(cell);
        }
        rows.push(row_cells);
    }

    if let Some(f) = current_floor {
        if rows.len() != sz {
            return Err(format!(
                "floor {f}: section ended with {} rows (expected {sz})",
                rows.len()
            ));
        }
        sections.push((f, rows));
    } else if text.lines().any(|l| !l.trim().is_empty()) {
        return Err("no `# floor N` section found".to_string());
    }

    Ok(sections)
}

fn apply_parsed_sections(chunk: &mut HypermapChunk<CellType>, sections: &[(i32, Vec<Vec<CellType>>)]) {
    clear_chunk_all_void(chunk);
    for (floor, rows) in sections {
        for (y, row) in rows.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                chunk.set_local_floor(LocalCoord::new(x as i32, y as i32), *floor, *cell);
            }
        }
    }
}

/// Clears the chunk to void on every floor, then applies geometry from `text`.
pub fn apply_level_geometry_text(chunk: &mut HypermapChunk<CellType>, text: &str) -> Result<(), String> {
    let sections = parse_level_geometry_sections(text)?;
    apply_parsed_sections(chunk, &sections);
    Ok(())
}

/// Writes the given chunk coordinates to `levels/level_{name}/geometry/{x}_{y}.txt`
/// (skips coordinates with no loaded chunk data).
pub fn save_level_geometry_for_chunks(
    level_name: &str,
    map: &Hypermap<CellType>,
    coords: impl IntoIterator<Item = ChunkCoord>,
) -> io::Result<usize> {
    let dir = geometry_dir(level_name);
    fs::create_dir_all(&dir)?;
    let mut count = 0usize;
    for coord in coords {
        if !map.has_chunk(coord) {
            continue;
        }
        let path = chunk_geometry_path(level_name, coord);
        let text = map
            .with_chunk_read(coord, |c| encode_chunk_geometry(c))
            .unwrap_or_default();
        let mut f = fs::File::create(&path)?;
        f.write_all(text.as_bytes())?;
        count += 1;
    }
    Ok(count)
}

/// Writes every in-memory chunk (any chunk ever generated) to disk.
pub fn save_all_loaded_level_geometry(level_name: &str, map: &Hypermap<CellType>) -> io::Result<usize> {
    save_level_geometry_for_chunks(level_name, map, map.loaded_chunks())
}

/// Picks the next free `new_N` level name not already in `existing` (case-insensitive).
/// `existing` is the list returned by the menu's `scan_available_levels`.
pub fn pick_new_level_name(existing: &[String]) -> String {
    let taken: std::collections::HashSet<String> =
        existing.iter().map(|s| s.to_ascii_lowercase()).collect();
    for n in 1u32.. {
        let candidate = format!("new_{n:03}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 range exhausted")
}

/// Creates `levels/level_{name}/geometry/` and writes a `0_0.txt` whose floor `0`
/// is entirely [`CellType::Road`] (all upper floors stay void). Other chunks are
/// generated lazily by [`crate::map::hypermap_world::ensure_chunk_generated`].
pub fn create_new_level_with_road_origin(level_name: &str) -> io::Result<()> {
    let dir = geometry_dir(level_name);
    fs::create_dir_all(&dir)?;
    let map = Hypermap::new(CellType::Road);
    map.with_chunk_write(ChunkCoord::new(0, 0), |chunk| {
        for y in 0..HYPERMAP_CHUNK_SIZE {
            for x in 0..HYPERMAP_CHUNK_SIZE {
                chunk.set_local(LocalCoord::new(x, y), CellType::Road);
                for floor in 1..HYPERMAP_FLOOR_COUNT as i32 {
                    chunk.set_local_floor(LocalCoord::new(x, y), floor, CellType::Void);
                }
            }
        }
    });
    let path = chunk_geometry_path(level_name, ChunkCoord::new(0, 0));
    let text = map
        .with_chunk_read(ChunkCoord::new(0, 0), |c| encode_chunk_geometry(c))
        .unwrap_or_default();
    let mut f = fs::File::create(&path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

/// If `path` exists and is readable, applies it to `chunk`. Returns `true` if loaded.
pub fn try_load_chunk_geometry_file(path: &Path, chunk: &mut HypermapChunk<CellType>) -> io::Result<bool> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let sections = parse_level_geometry_sections(&text).map_err(|msg| {
                io::Error::new(io::ErrorKind::InvalidData, msg)
            })?;
            apply_parsed_sections(chunk, &sections);
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

// ─── Style layer ────────────────────────────────────────────────────────────

pub fn style_floor_dir(level_name: &str) -> PathBuf {
    PathBuf::from("levels").join(format!("level_{level_name}")).join("style_floor")
}

pub fn style_wall_dir(level_name: &str) -> PathBuf {
    PathBuf::from("levels").join(format!("level_{level_name}")).join("style_wall")
}

pub fn chunk_style_floor_path(level_name: &str, coord: ChunkCoord) -> PathBuf {
    style_floor_dir(level_name).join(format!("{}_{}.txt", coord.x, coord.y))
}

pub fn chunk_style_wall_path(level_name: &str, coord: ChunkCoord) -> PathBuf {
    style_wall_dir(level_name).join(format!("{}_{}.txt", coord.x, coord.y))
}

fn floor_all_default(chunk: &HypermapChunk<TileStyle>, floor: i32) -> bool {
    for y in 0..HYPERMAP_CHUNK_SIZE {
        for x in 0..HYPERMAP_CHUNK_SIZE {
            if *chunk.get_local_floor(LocalCoord::new(x, y), floor) != TileStyle::DEFAULT {
                return false;
            }
        }
    }
    true
}

/// Serializes one chunk's style layer to text. Floors where every cell is
/// [`TileStyle::DEFAULT`] are omitted. Returns an empty string if the whole
/// chunk is default style.
pub fn encode_chunk_style(chunk: &HypermapChunk<TileStyle>) -> String {
    let sz = HYPERMAP_CHUNK_SIZE as usize;
    let mut out = String::new();
    for floor in 0..HYPERMAP_FLOOR_COUNT as i32 {
        if floor_all_default(chunk, floor) {
            continue;
        }
        out.push_str(&format!("# floor {floor}\n"));
        for y in 0..sz {
            let mut first = true;
            for x in 0..sz {
                let style = *chunk.get_local_floor(LocalCoord::new(x as i32, y as i32), floor);
                if !first {
                    out.push(' ');
                }
                first = false;
                out.push_str(style.as_str());
            }
            out.push('\n');
        }
    }
    out
}

fn apply_parsed_style_sections(chunk: &mut HypermapChunk<TileStyle>, sections: &[(i32, Vec<Vec<TileStyle>>)]) {
    for (floor, rows) in sections {
        for (y, row) in rows.iter().enumerate() {
            for (x, style) in row.iter().enumerate() {
                chunk.set_local_floor(LocalCoord::new(x as i32, y as i32), *floor, *style);
            }
        }
    }
}

/// Parses a style text into `(floor, rows)` sections.
pub fn parse_level_style_sections(text: &str) -> Result<Vec<(i32, Vec<Vec<TileStyle>>)>, String> {
    let sz = HYPERMAP_CHUNK_SIZE as usize;
    let mut sections: Vec<(i32, Vec<Vec<TileStyle>>)> = Vec::new();
    let mut current_floor: Option<i32> = None;
    let mut rows: Vec<Vec<TileStyle>> = Vec::new();

    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# floor ") {
            if let Some(f) = current_floor {
                if rows.len() != sz {
                    return Err(format!(
                        "line {}: floor {f} had {} rows (expected {sz})",
                        line_no + 1,
                        rows.len()
                    ));
                }
                sections.push((f, std::mem::take(&mut rows)));
            }
            let n: i32 = rest
                .trim()
                .parse()
                .map_err(|_| format!("line {}: bad floor index `{rest}`", line_no + 1))?;
            if n < 0 || n >= HYPERMAP_FLOOR_COUNT as i32 {
                return Err(format!("line {}: floor {n} out of range", line_no + 1));
            }
            current_floor = Some(n);
            continue;
        }

        let Some(floor) = current_floor else {
            return Err(format!(
                "line {}: expected `# floor N` before grid data",
                line_no + 1
            ));
        };

        if rows.len() >= sz {
            return Err(format!("line {}: too many rows for floor {floor}", line_no + 1));
        }

        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() != sz {
            return Err(format!(
                "line {}: floor {floor} row {} has {} tokens (expected {sz})",
                line_no + 1,
                rows.len(),
                tokens.len()
            ));
        }
        let mut row_styles = Vec::with_capacity(sz);
        for (x, tok) in tokens.iter().enumerate() {
            let style = parse_style_token(tok).ok_or_else(|| {
                format!(
                    "line {}: invalid style token `{tok}` at floor {floor} row {} col {x}",
                    line_no + 1,
                    rows.len()
                )
            })?;
            row_styles.push(style);
        }
        rows.push(row_styles);
    }

    if let Some(f) = current_floor {
        if rows.len() != sz {
            return Err(format!(
                "floor {f}: section ended with {} rows (expected {sz})",
                rows.len()
            ));
        }
        sections.push((f, rows));
    }

    Ok(sections)
}

/// Loads a style file into the given map chunk, creating the chunk lazily.
/// Returns `Ok(true)` if loaded, `Ok(false)` if the file does not exist.
pub fn try_load_chunk_style_file_into_map(
    path: &Path,
    map: &Hypermap<TileStyle>,
    coord: ChunkCoord,
) -> io::Result<bool> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let sections = parse_level_style_sections(&text)
                .map_err(|msg| io::Error::new(io::ErrorKind::InvalidData, msg))?;
            map.with_chunk_write(coord, |chunk| apply_parsed_style_sections(chunk, &sections));
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn save_style_map_to_dir(
    dir: &std::path::Path,
    path_fn: impl Fn(ChunkCoord) -> PathBuf,
    map: &Hypermap<TileStyle>,
    coords: impl IntoIterator<Item = ChunkCoord>,
) -> io::Result<usize> {
    let mut count = 0usize;
    for coord in coords {
        if !map.has_chunk(coord) {
            continue;
        }
        let text = map
            .with_chunk_read(coord, |c| encode_chunk_style(c))
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        fs::create_dir_all(dir)?;
        let path = path_fn(coord);
        let mut f = fs::File::create(&path)?;
        f.write_all(text.as_bytes())?;
        count += 1;
    }
    Ok(count)
}

/// Writes floor style files for the given chunk coordinates.
pub fn save_level_floor_style_for_chunks(
    level_name: &str,
    map: &Hypermap<TileStyle>,
    coords: impl IntoIterator<Item = ChunkCoord>,
) -> io::Result<usize> {
    let dir = style_floor_dir(level_name);
    save_style_map_to_dir(&dir, |c| chunk_style_floor_path(level_name, c), map, coords)
}

/// Writes wall style files for the given chunk coordinates.
pub fn save_level_wall_style_for_chunks(
    level_name: &str,
    map: &Hypermap<TileStyle>,
    coords: impl IntoIterator<Item = ChunkCoord>,
) -> io::Result<usize> {
    let dir = style_wall_dir(level_name);
    save_style_map_to_dir(&dir, |c| chunk_style_wall_path(level_name, c), map, coords)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::hypermap::Hypermap;
    use crate::map::world_map::{WallMask, MASK_NORTH};

    #[test]
    fn encode_decode_roundtrip_single_floor() {
        let map = Hypermap::new(CellType::Void);
        map.with_chunk_write(ChunkCoord::new(0, 0), |c| {
            c.set_local(LocalCoord::new(0, 0), CellType::Road);
            c.set_local(
                LocalCoord::new(1, 0),
                CellType::Wall(WallMask::from_bits(MASK_NORTH).unwrap()),
            );
        });
        let enc = map
            .with_chunk_read(ChunkCoord::new(0, 0), |c| encode_chunk_geometry(c))
            .unwrap();
        let map2 = Hypermap::new(CellType::Road);
        map2.with_chunk_write(ChunkCoord::new(0, 0), |c| {
            apply_level_geometry_text(c, &enc).unwrap();
            assert_eq!(*c.get_local(LocalCoord::new(0, 0)), CellType::Road);
            assert!(matches!(
                *c.get_local(LocalCoord::new(1, 0)),
                CellType::Wall(_)
            ));
        });
    }
}

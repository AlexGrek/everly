//! Binary tile fields: `levels/level_{name}/dirt.bin` and `temperature.bin`.
//!
//! One file holds every chunk that was saved. Ground floor only: `128×128` `f32`
//! samples per chunk (row-major, local chunk coords).

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::map::hypermap::{
    ChunkCoord, Hypermap, HypermapChunk, LocalCoord, HYPERMAP_CHUNK_SIZE,
};

const MAGIC: &[u8; 4] = b"EVTF";
const VERSION: u32 = 1;
const TILES_PER_CHUNK: usize =
    (HYPERMAP_CHUNK_SIZE as usize) * (HYPERMAP_CHUNK_SIZE as usize);
const CHUNK_BYTES: usize = TILES_PER_CHUNK * 4;

pub fn dirt_bin_path(level_name: &str) -> PathBuf {
    level_field_bin_path(level_name, "dirt.bin")
}

pub fn temperature_bin_path(level_name: &str) -> PathBuf {
    level_field_bin_path(level_name, "temperature.bin")
}

fn level_field_bin_path(level_name: &str, filename: &str) -> PathBuf {
    PathBuf::from("levels")
        .join(format!("level_{level_name}"))
        .join(filename)
}

fn chunk_samples(chunk: &HypermapChunk<f32>) -> Vec<f32> {
    let mut out = Vec::with_capacity(TILES_PER_CHUNK);
    for y in 0..HYPERMAP_CHUNK_SIZE {
        for x in 0..HYPERMAP_CHUNK_SIZE {
            out.push(*chunk.get_local(LocalCoord::new(x, y)));
        }
    }
    out
}

fn write_chunk_samples(chunk: &mut HypermapChunk<f32>, samples: &[f32]) {
    debug_assert_eq!(samples.len(), TILES_PER_CHUNK);
    for y in 0..HYPERMAP_CHUNK_SIZE {
        for x in 0..HYPERMAP_CHUNK_SIZE {
            let idx = (y as usize) * (HYPERMAP_CHUNK_SIZE as usize) + (x as usize);
            chunk.set_local(LocalCoord::new(x, y), samples[idx]);
        }
    }
}

fn encode_file(chunks: &[(ChunkCoord, Vec<f32>)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12 + chunks.len() * (8 + CHUNK_BYTES));
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for (coord, samples) in chunks {
        debug_assert_eq!(samples.len(), TILES_PER_CHUNK);
        buf.extend_from_slice(&coord.x.to_le_bytes());
        buf.extend_from_slice(&coord.y.to_le_bytes());
        for &v in samples {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    buf
}

fn decode_file(bytes: &[u8]) -> io::Result<Vec<(ChunkCoord, Vec<f32>)>> {
    if bytes.len() < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tile field file too short",
        ));
    }
    if bytes[0..4] != *MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tile field bad magic",
        ));
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("tile field unsupported version {version}"),
        ));
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let mut offset = 12usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if offset + 8 + CHUNK_BYTES > bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tile field truncated chunk record",
            ));
        }
        let x = i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        offset += 4;
        let y = i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        offset += 4;
        let mut samples = Vec::with_capacity(TILES_PER_CHUNK);
        for _ in 0..TILES_PER_CHUNK {
            samples.push(f32::from_le_bytes(
                bytes[offset..offset + 4].try_into().unwrap(),
            ));
            offset += 4;
        }
        out.push((ChunkCoord::new(x, y), samples));
    }
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tile field trailing bytes",
        ));
    }
    Ok(out)
}

/// Writes every loaded chunk in `map` into one binary field file.
pub fn save_tile_field_bin(
    path: &PathBuf,
    map: &Hypermap<f32>,
    coords: impl IntoIterator<Item = ChunkCoord>,
) -> io::Result<usize> {
    let mut records = Vec::new();
    for coord in coords {
        if !map.has_chunk(coord) {
            continue;
        }
        let Some(samples) = map.with_chunk_read(coord, chunk_samples) else {
            continue;
        };
        records.push((coord, samples));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, encode_file(&records))?;
    Ok(records.len())
}

/// Loads all chunk records from `path` into `map` (write buffer, then flush caller).
pub fn load_tile_field_bin(path: &PathBuf, map: &Hypermap<f32>) -> io::Result<bool> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let records = decode_file(&bytes)?;
    for (coord, samples) in records {
        map.with_chunk_write(coord, |chunk| write_chunk_samples(chunk, &samples));
    }
    Ok(true)
}

pub fn save_dirt_bin(
    level_name: &str,
    map: &Hypermap<f32>,
    coords: impl IntoIterator<Item = ChunkCoord>,
) -> io::Result<usize> {
    save_tile_field_bin(&dirt_bin_path(level_name), map, coords)
}

pub fn save_temperature_bin(
    level_name: &str,
    map: &Hypermap<f32>,
    coords: impl IntoIterator<Item = ChunkCoord>,
) -> io::Result<usize> {
    save_tile_field_bin(&temperature_bin_path(level_name), map, coords)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::hypermap::Hypermap;

    #[test]
    fn bin_round_trip() {
        let map = Hypermap::new(0.0);
        map.with_chunk_write(ChunkCoord::new(1, -2), |chunk| {
            chunk.set_local(LocalCoord::new(3, 4), 0.5);
        });
        let dir = std::env::temp_dir().join("everly_tile_field_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.bin");
        save_tile_field_bin(&path, &map, [ChunkCoord::new(1, -2)]).unwrap();
        let map2 = Hypermap::new(0.0);
        assert!(load_tile_field_bin(&path, &map2).unwrap());
        let wx = 1 * HYPERMAP_CHUNK_SIZE + 3;
        let wy = -2 * HYPERMAP_CHUNK_SIZE + 4;
        assert_eq!(map2.get(wx, wy), 0.5);
    }
}

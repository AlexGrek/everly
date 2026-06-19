//! Cross-chunk road corridors through the void margin (3–4 tiles wide).

use rand::Rng;

use crate::map::hypermap::{ChunkCoord, Hypermap, HypermapChunk, LocalCoord};
use crate::map::world_map::CellType;

use crate::rng::{self, StdRng};

use super::draft::{DraftTile, MapDraft};
use super::types::{
    ChunkRoadConnectors, RoadConnector, CHUNK_CONNECTOR_WIDTH_MAX, CHUNK_CONNECTOR_WIDTH_MIN,
    CONNECTORS_PER_SIDE_MAX, CONNECTORS_PER_SIDE_MIN,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkSide {
    West,
    East,
    South,
    North,
}

impl ChunkSide {
    const ALL: [ChunkSide; 4] = [
        ChunkSide::West,
        ChunkSide::East,
        ChunkSide::South,
        ChunkSide::North,
    ];

    fn neighbor_coord_and_their_side(self, coord: ChunkCoord) -> (ChunkCoord, ChunkSide) {
        match self {
            ChunkSide::West => (ChunkCoord::new(coord.x - 1, coord.y), ChunkSide::East),
            ChunkSide::East => (ChunkCoord::new(coord.x + 1, coord.y), ChunkSide::West),
            ChunkSide::South => (ChunkCoord::new(coord.x, coord.y - 1), ChunkSide::North),
            ChunkSide::North => (ChunkCoord::new(coord.x, coord.y + 1), ChunkSide::South),
        }
    }
}

fn cell_is_road(cell: &CellType) -> bool {
    matches!(
        cell,
        CellType::Road | CellType::Charger(_) | CellType::PartsDepot(_)
    )
}

fn runs_to_connectors(road_along: &[bool]) -> Vec<RoadConnector> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < road_along.len() {
        if !road_along[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < road_along.len() && road_along[i] {
            i += 1;
        }
        out.push(RoadConnector {
            start: start as i32,
            width: (i - start) as i32,
        });
    }
    out
}

fn scan_chunk_edge_roads(
    chunk: &HypermapChunk<CellType>,
    side: ChunkSide,
    margin: i32,
    size: i32,
) -> Vec<RoadConnector> {
    let mut road_along = vec![false; size as usize];
    match side {
        ChunkSide::West => {
            for z in 0..size {
                for x in 0..margin {
                    if cell_is_road(chunk.get_local_floor(LocalCoord::new(x, z), 0)) {
                        road_along[z as usize] = true;
                        break;
                    }
                }
            }
        }
        ChunkSide::East => {
            for z in 0..size {
                for x in (size - margin)..size {
                    if cell_is_road(chunk.get_local_floor(LocalCoord::new(x, z), 0)) {
                        road_along[z as usize] = true;
                        break;
                    }
                }
            }
        }
        ChunkSide::South => {
            for x in 0..size {
                for z in 0..margin {
                    if cell_is_road(chunk.get_local_floor(LocalCoord::new(x, z), 0)) {
                        road_along[x as usize] = true;
                        break;
                    }
                }
            }
        }
        ChunkSide::North => {
            for x in 0..size {
                for z in (size - margin)..size {
                    if cell_is_road(chunk.get_local_floor(LocalCoord::new(x, z), 0)) {
                        road_along[x as usize] = true;
                        break;
                    }
                }
            }
        }
    }
    runs_to_connectors(&road_along)
}

fn plan_random_connectors(rng: &mut StdRng, margin: i32, size: i32) -> Vec<RoadConnector> {
    let count = rng.gen_range(CONNECTORS_PER_SIDE_MIN..=CONNECTORS_PER_SIDE_MAX) as usize;
    let along_lo = margin;
    let along_hi = size - margin - 1;
    let mut placed = Vec::new();

    for _ in 0..count {
        let width = rng.gen_range(CHUNK_CONNECTOR_WIDTH_MIN..=CHUNK_CONNECTOR_WIDTH_MAX);
        let max_start = along_hi - width + 1;
        if max_start < along_lo {
            break;
        }

        let mut chosen = None;
        for _ in 0..32 {
            let start = rng.gen_range(along_lo..=max_start);
            let end = start + width;
            let overlaps = placed
                .iter()
                .any(|c: &RoadConnector| start < c.start + c.width && end > c.start);
            if !overlaps {
                chosen = Some(RoadConnector { start, width });
                break;
            }
        }
        if let Some(connector) = chosen {
            placed.push(connector);
        }
    }

    placed
}

fn connector_plan_seed(base_seed: u64, coord: ChunkCoord) -> u64 {
    base_seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(coord.x as u64)
        .wrapping_mul(0xBF58_476D_1CE4_E5B9)
        .wrapping_add(coord.y as u64)
}

pub(crate) fn plan_chunk_connectors(
    map: &Hypermap<CellType>,
    coord: ChunkCoord,
    margin: i32,
    size: i32,
    generator_seed: u64,
) -> ChunkRoadConnectors {
    let mut rng = rng::seeded(connector_plan_seed(generator_seed, coord));
    let mut plan = ChunkRoadConnectors::default();

    for side in ChunkSide::ALL {
        let (neighbor_coord, their_side) = side.neighbor_coord_and_their_side(coord);
        let connectors = if map.has_chunk(neighbor_coord) {
            map.with_chunk_read(neighbor_coord, |chunk| {
                scan_chunk_edge_roads(chunk, their_side, margin, size)
            })
            .unwrap_or_default()
        } else {
            Vec::new()
        };

        let connectors = if connectors.is_empty() {
            plan_random_connectors(&mut rng, margin, size)
        } else {
            connectors
        };

        match side {
            ChunkSide::West => plan.west = connectors,
            ChunkSide::East => plan.east = connectors,
            ChunkSide::South => plan.south = connectors,
            ChunkSide::North => plan.north = connectors,
        }
    }

    plan
}

impl MapDraft {
    pub(crate) fn step_stamp_chunk_connectors(&mut self) {
        let m = self.margin;
        for connector in &self.connector_plan.west.clone() {
            stamp_west_connector(self, connector, m);
        }
        for connector in &self.connector_plan.east.clone() {
            stamp_east_connector(self, connector, m);
        }
        for connector in &self.connector_plan.south.clone() {
            stamp_south_connector(self, connector, m);
        }
        for connector in &self.connector_plan.north.clone() {
            stamp_north_connector(self, connector, m);
        }
    }
}

fn stamp_west_connector(draft: &mut MapDraft, connector: &RoadConnector, margin: i32) {
    for z in connector.start..connector.start + connector.width {
        for x in 0..=margin {
            if draft.in_bounds(x, z) {
                draft.set(x, z, DraftTile::Open);
            }
        }
    }
}

fn stamp_east_connector(draft: &mut MapDraft, connector: &RoadConnector, margin: i32) {
    let sz = draft.size;
    for z in connector.start..connector.start + connector.width {
        for x in (sz - margin - 1)..sz {
            if draft.in_bounds(x, z) {
                draft.set(x, z, DraftTile::Open);
            }
        }
    }
}

fn stamp_south_connector(draft: &mut MapDraft, connector: &RoadConnector, margin: i32) {
    for x in connector.start..connector.start + connector.width {
        for z in 0..=margin {
            if draft.in_bounds(x, z) {
                draft.set(x, z, DraftTile::Open);
            }
        }
    }
}

fn stamp_north_connector(draft: &mut MapDraft, connector: &RoadConnector, margin: i32) {
    let sz = draft.size;
    for x in connector.start..connector.start + connector.width {
        for z in (sz - margin - 1)..sz {
            if draft.in_bounds(x, z) {
                draft.set(x, z, DraftTile::Open);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::hypermap::HYPERMAP_CHUNK_SIZE;
    use crate::map::map_generator::types::MapGeneratorConfig;
    use crate::map::map_generator::{MapDraft, CHUNK_VOID_MARGIN};
    use crate::map::world_map::CellType;

    #[test]
    fn random_connectors_respect_width_and_count() {
        for seed in 0..64u64 {
            let plan = plan_chunk_connectors(
                &Hypermap::new(CellType::Void),
                ChunkCoord::new(0, 0),
                CHUNK_VOID_MARGIN,
                HYPERMAP_CHUNK_SIZE,
                seed,
            );
            for side in [
                &plan.west,
                &plan.east,
                &plan.south,
                &plan.north,
            ] {
                assert!(
                    (CONNECTORS_PER_SIDE_MIN as usize..=CONNECTORS_PER_SIDE_MAX as usize)
                        .contains(&side.len()),
                    "seed {seed}: side connector count {}",
                    side.len()
                );
                for c in side.iter() {
                    assert!(
                        (CHUNK_CONNECTOR_WIDTH_MIN..=CHUNK_CONNECTOR_WIDTH_MAX)
                            .contains(&c.width),
                        "seed {seed}: width {}",
                        c.width
                    );
                }
            }
        }
    }

    #[test]
    fn neighbor_connectors_align_across_chunks() {
        let map = Hypermap::new(CellType::Void);
        let west_coord = ChunkCoord::new(0, 0);
        let east_coord = ChunkCoord::new(1, 0);
        let seed = 0xC0FF_EE01_u64;

        let west_meta = map.with_chunk_write(west_coord, |chunk| {
            let mut draft = MapDraft::new(MapGeneratorConfig {
                seed,
                ..Default::default()
            });
            draft.connector_plan =
                plan_chunk_connectors(&map, west_coord, draft.margin, draft.size, seed);
            draft.run_pipeline();
            let size = draft.size;
            let meta = draft.build_metadata();
            let cells = draft.finish();
            for z in 0..size as usize {
                for x in 0..size as usize {
                    chunk.set_local(LocalCoord::new(x as i32, z as i32), cells[z][x]);
                }
            }
            meta
        });

        let east_meta = map.with_chunk_write(east_coord, |chunk| {
            let mut draft = MapDraft::new(MapGeneratorConfig {
                seed: seed.wrapping_add(1),
                ..Default::default()
            });
            draft.connector_plan =
                plan_chunk_connectors(&map, east_coord, draft.margin, draft.size, seed);
            draft.run_pipeline();
            let size = draft.size;
            let meta = draft.build_metadata();
            let cells = draft.finish();
            for z in 0..size as usize {
                for x in 0..size as usize {
                    chunk.set_local(LocalCoord::new(x as i32, z as i32), cells[z][x]);
                }
            }
            meta
        });

        assert_eq!(west_meta.road_connectors.east, east_meta.road_connectors.west);

        for z in 0..HYPERMAP_CHUNK_SIZE {
            let west_road = map
                .with_chunk_read(west_coord, |c| {
                    cell_is_road(c.get_local_floor(
                        LocalCoord::new(HYPERMAP_CHUNK_SIZE - 1, z),
                        0,
                    ))
                })
                .unwrap();
            let east_road = map
                .with_chunk_read(east_coord, |c| {
                    cell_is_road(c.get_local_floor(LocalCoord::new(0, z), 0))
                })
                .unwrap();
            assert_eq!(
                west_road, east_road,
                "z={z}: west east-edge road {west_road} != east west-edge road {east_road}"
            );
        }
    }

    #[test]
    fn stamped_margin_connectors_become_road() {
        let mut draft = MapDraft::new(MapGeneratorConfig {
            seed: 7,
            ..Default::default()
        });
        draft.connector_plan = plan_chunk_connectors(
            &Hypermap::new(CellType::Void),
            ChunkCoord::new(3, -2),
            draft.margin,
            draft.size,
            draft.generator_seed,
        );
        draft.step_init_carpet();
        draft.step_stamp_chunk_connectors();
        let west_connectors = draft.connector_plan.west.clone();
        let cells = draft.finish();
        for connector in &west_connectors {
            for z in connector.start..connector.start + connector.width {
                assert_eq!(
                    cells[z as usize][0],
                    CellType::Road,
                    "west margin z={z}"
                );
            }
        }
    }
}

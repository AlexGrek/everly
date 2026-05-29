//! Shared test fixture: a hand-checked, **procedurally generated 6×6-chunk
//! world** saved to disk under `test_fixtures/level_test_world/`. Every game-logic
//! unit test that needs a realistic world should load it through
//! [`TestWorld::load`] rather than hand-building tiles, so assertions run against
//! one canonical world with real generated houses, walls, doors, and chargers.
//!
//! # Layout
//!
//! 36 chunks at coordinates `(0,0)..=(5,5)` — `768 × 768` tiles on floor `0`. Each
//! chunk is generated independently with a deterministic per-chunk seed
//! ([`chunk_seed`]) and carries the generator's [`CHUNK_VOID_MARGIN`] void border,
//! so **chunks are separate connected components**: a start tile in one chunk can
//! only reach chargers in that same chunk. That split is what makes
//! [`InteractiveEntityMap::find_accessible_within`] interesting on this fixture
//! (chargers in other chunks are genuinely unreachable) without hand-editing.
//!
//! # Regenerating
//!
//! The geometry files are committed. To rebuild them (after a generator change, or
//! to reseed), run the ignored builder test:
//!
//! ```sh
//! cargo test -p everly regenerate_test_world_fixture -- --ignored
//! ```
//!
//! then hand-edit individual `geometry/{x}_{y}.txt` chunks if a test needs a
//! specific tweak (they are plain level-geometry text).

use std::path::PathBuf;

use crate::map::hypermap::{
    ChunkCoord, Hypermap, LocalCoord, HYPERMAP_CHUNK_SIZE,
};
use crate::map::interactive_entity::{
    ChargerEntity, EntityCoordinates, InteractiveEntity, InteractiveEntityMap,
};
use crate::map::level::try_load_chunk_geometry_file;
use crate::map::world_map::{cell_passability, CellType};

/// Fixture root, relative to the crate manifest.
pub const TEST_WORLD_DIR: &str = "test_fixtures/level_test_world";

/// World is this many chunks on each axis (`(0,0)..=(SPAN-1, SPAN-1)`).
pub const TEST_WORLD_CHUNK_SPAN: i32 = 6;

/// Base seed mixed with chunk coordinates by [`chunk_seed`]. Changing this
/// reshuffles every chunk on the next regenerate.
const SEED_BASE: u64 = 0x_E5E1_7_0_0D_C0FF_EE11;

/// Deterministic generator seed for chunk `(cx, cy)` — distinct per chunk so the
/// 36 chunks differ, but stable across runs so the committed fixture is reproducible.
pub fn chunk_seed(cx: i32, cy: i32) -> u64 {
    let x = (cx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let y = (cy as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    SEED_BASE ^ x ^ y.rotate_left(32)
}

/// Absolute path to the fixture directory (resolved from `CARGO_MANIFEST_DIR` so
/// it works regardless of the test's working directory).
pub fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(TEST_WORLD_DIR)
}

fn geometry_dir() -> PathBuf {
    fixture_dir().join("geometry")
}

fn chunk_geometry_file(cx: i32, cy: i32) -> PathBuf {
    geometry_dir().join(format!("{cx}_{cy}.txt"))
}

/// The loaded fixture world: tile geometry plus the two views game logic actually
/// queries — a passability map for pathfinding and an [`InteractiveEntityMap`] of
/// the chargers found in the geometry.
pub struct TestWorld {
    /// Floor-0 [`CellType`] grid for all 36 chunks.
    pub cells: Hypermap<CellType>,
    /// `cell_passability` of every loaded tile (`> 0.0` walkable). Floor 0.
    pub passability: Hypermap<f32>,
    /// One [`ChargerEntity`] per `Charger` tile in the geometry. Interactive
    /// entities are not yet populated by generation, so the fixture loader is the
    /// thing that derives them from the tiles.
    pub entities: InteractiveEntityMap,
}

impl TestWorld {
    /// Loads the committed fixture. Panics if a chunk file is missing or malformed
    /// — a broken fixture is a test-suite bug that should fail loudly.
    pub fn load() -> Self {
        let cells = Hypermap::new(CellType::Void);
        for cy in 0..TEST_WORLD_CHUNK_SPAN {
            for cx in 0..TEST_WORLD_CHUNK_SPAN {
                let coord = ChunkCoord::new(cx, cy);
                let path = chunk_geometry_file(cx, cy);
                let loaded = cells
                    .with_chunk_write(coord, |chunk| try_load_chunk_geometry_file(&path, chunk))
                    .unwrap_or_else(|e| panic!("read fixture chunk {}: {e}", path.display()));
                assert!(loaded, "fixture chunk missing: {}", path.display());
            }
        }

        let passability = Hypermap::new(0.0);
        let mut entities = InteractiveEntityMap::new();
        let sz = HYPERMAP_CHUNK_SIZE;
        for coord in cells.loaded_chunks() {
            let chargers = cells
                .with_chunk_read(coord, |cell_chunk| {
                    let mut chargers = Vec::new();
                    passability.with_chunk_write(coord, |pass_chunk| {
                        for y in 0..sz {
                            for x in 0..sz {
                                let local = LocalCoord::new(x, y);
                                let cell = *cell_chunk.get_local_floor(local, 0);
                                pass_chunk.set_local_floor(local, 0, cell_passability(cell));
                                if let CellType::Charger(facing) = cell {
                                    let wx = coord.x * sz + x;
                                    let wy = coord.y * sz + y;
                                    chargers.push((wx, wy, facing));
                                }
                            }
                        }
                    });
                    chargers
                })
                .unwrap_or_default();
            for (wx, wy, facing) in chargers {
                entities.insert(InteractiveEntity::Charger(ChargerEntity::new(
                    EntityCoordinates::ground(wx, wy),
                    facing,
                )));
            }
        }

        Self {
            cells,
            passability,
            entities,
        }
    }

    /// Convenience: the static-passability map pathfinding consumes.
    pub fn passability(&self) -> &Hypermap<f32> {
        &self.passability
    }

    /// Convenience: the interactive-entity submap.
    pub fn entities(&self) -> &InteractiveEntityMap {
        &self.entities
    }

    /// First charger tile in unspecified order, as `(x, y)`. Handy for tests that
    /// just need "some real charger" without caring which.
    pub fn any_charger_tile(&self) -> (i32, i32) {
        let e = self
            .entities
            .iter()
            .next()
            .expect("fixture has at least one charger");
        (e.coordinates.x, e.coordinates.y)
    }

    /// A walkable tile adjacent to `(x, y)`, if any (entities back onto walls, so
    /// their own tile is the dock and neighbors are where an actor stands).
    pub fn walkable_neighbor(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        [(1, 0), (-1, 0), (0, 1), (0, -1)]
            .into_iter()
            .map(|(dx, dy)| (x + dx, y + dy))
            .find(|&(nx, ny)| self.passability.get(nx, ny) > 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::hypermap_pathfind::{
        astar_shortest_world_path, HypermapPathResult, HypermapSearchLimits,
    };
    use crate::map::interactive_entity::EntityType;

    /// Builder for the committed fixture. Ignored in normal runs; run explicitly to
    /// regenerate the geometry files (then optionally hand-edit chunks).
    #[test]
    #[ignore = "writes the committed fixture; run with --ignored to regenerate"]
    fn regenerate_test_world_fixture() {
        use crate::map::map_generator::{generate_chunk_geometry, MapGeneratorConfig};
        use std::fs;

        let dir = geometry_dir();
        fs::create_dir_all(&dir).expect("create fixture geometry dir");
        let mut written = 0;
        for cy in 0..TEST_WORLD_CHUNK_SPAN {
            for cx in 0..TEST_WORLD_CHUNK_SPAN {
                let config = MapGeneratorConfig {
                    seed: chunk_seed(cx, cy),
                    ..Default::default()
                };
                let text = generate_chunk_geometry(&config);
                fs::write(chunk_geometry_file(cx, cy), text).expect("write chunk file");
                written += 1;
            }
        }
        assert_eq!(written, TEST_WORLD_CHUNK_SPAN * TEST_WORLD_CHUNK_SPAN);
    }

    fn sorted_coords(entries: &[&crate::map::interactive_entity::InteractiveEntityEntry]) -> Vec<(i32, i32)> {
        let mut v: Vec<(i32, i32)> = entries.iter().map(|e| (e.coordinates.x, e.coordinates.y)).collect();
        v.sort();
        v
    }

    #[test]
    #[ignore = "inspection helper for baking golden values"]
    fn dump_locator_truth() {
        let w = TestWorld::load();
        let mut all: Vec<(i32, i32)> = w.entities.iter().map(|e| (e.coordinates.x, e.coordinates.y)).collect();
        all.sort();
        println!("ALL ({}): {all:?}", all.len());

        let c0 = EntityCoordinates::ground(all[0].0, all[0].1);
        let r = w.entities.find_within_radius(c0, 40, None);
        println!("RADIUS center={:?} r=40 -> {:?}", all[0], sorted_coords(&r));

        let cc = EntityCoordinates::ground(2 * HYPERMAP_CHUNK_SIZE + 64, 2 * HYPERMAP_CHUNK_SIZE + 64);
        let rc = w.entities.find_in_rendered_chunks(cc, None);
        println!("RENDERED center=(320,320) -> {:?}", sorted_coords(&rc));

        let (cx, cy) = all[0];
        let start = w.walkable_neighbor(cx, cy).unwrap();
        let acc = w.entities.find_accessible_within(&w.passability, start, 0, 4 * HYPERMAP_CHUNK_SIZE as u32, None);
        println!("ACCESSIBLE start={start:?} -> {:?}", sorted_coords(&acc));

        // A pure-road start near the middle of chunk (0,0): scan for the first Road.
        let mut road_start = None;
        'outer: for y in 0..HYPERMAP_CHUNK_SIZE {
            for x in 0..HYPERMAP_CHUNK_SIZE {
                if matches!(w.cells.get(x, y), CellType::Road) {
                    road_start = Some((x, y));
                    break 'outer;
                }
            }
        }
        let road_start = road_start.unwrap();
        let acc2 = w.entities.find_accessible_within(&w.passability, road_start, 0, 4 * HYPERMAP_CHUNK_SIZE as u32, None);
        println!("ACCESSIBLE road_start={road_start:?} -> {:?}", sorted_coords(&acc2));
    }

    #[test]
    fn fixture_loads_with_houses_and_chargers() {
        let world = TestWorld::load();
        assert_eq!(world.cells.loaded_chunk_count(), 36, "6×6 chunks");

        // Real generated geometry: walls (houses) and at least one charger.
        let mut walls = 0usize;
        for coord in world.cells.loaded_chunks() {
            world.cells.with_chunk_read(coord, |c| {
                for y in 0..HYPERMAP_CHUNK_SIZE {
                    for x in 0..HYPERMAP_CHUNK_SIZE {
                        if matches!(
                            c.get_local_floor(LocalCoord::new(x, y), 0),
                            CellType::Wall(_)
                        ) {
                            walls += 1;
                        }
                    }
                }
            });
        }
        assert!(walls > 0, "generated houses should produce wall tiles");
        assert!(!world.entities.is_empty(), "generator places chargers");
    }

    #[test]
    fn every_charger_entity_sits_on_a_walkable_charger_tile() {
        let world = TestWorld::load();
        for entry in world.entities.iter() {
            assert_eq!(entry.entity_type, EntityType::Charger);
            let (x, y) = (entry.coordinates.x, entry.coordinates.y);
            assert!(
                matches!(world.cells.get(x, y), CellType::Charger(_)),
                "entity at ({x},{y}) must mirror a Charger tile"
            );
            assert!(
                world.passability.get(x, y) > 0.0,
                "charger tiles are walkable"
            );
        }
    }

    // Search range for the accessible locator: a few chunk-widths, enough to
    // exhaust any single chunk's connected component (chunks are isolated by the
    // generator's void margins).
    const REACH: u32 = 4 * HYPERMAP_CHUNK_SIZE as u32;

    /// **Source of truth** for the three entity-search functions against the
    /// committed fixture. These exact results were captured from a known-good run
    /// and independently verified by [`golden_locator_values_are_correct`]; a diff
    /// here means either a real regression in a locator or an intentional fixture
    /// regeneration (rebake via `dump_locator_truth`).
    ///
    /// FRAGILE BY DESIGN. Before editing any literal here, follow the mandatory
    /// verification protocol in `docs/test-world.md` — this storing test and its
    /// `*_are_correct` partner must be re-baked together, and a failing golden must
    /// be diagnosed as regression vs. intended fixture change *before* any edit.
    #[test]
    fn golden_locator_values() {
        let w = TestWorld::load();
        assert_eq!(w.entities.len(), 192, "total chargers in the fixture");

        // (1) find_within_radius — circle of radius 40 around the charger at (29,37).
        let radius = w.entities.find_within_radius(EntityCoordinates::ground(29, 37), 40, None);
        assert_eq!(sorted_coords(&radius), vec![(29, 37), (42, 59)]);

        // (2) find_in_rendered_chunks — centered in chunk (2,2); footprint is
        // chunks (2,2), (3,2), (2,3).
        let rendered = w.entities.find_in_rendered_chunks(
            EntityCoordinates::ground(2 * HYPERMAP_CHUNK_SIZE + 64, 2 * HYPERMAP_CHUNK_SIZE + 64),
            None,
        );
        assert_eq!(
            sorted_coords(&rendered),
            vec![
                (284, 301), (290, 324), (299, 464), (321, 292), (325, 344), (326, 352),
                (333, 351), (338, 321), (352, 412), (354, 446), (437, 360), (453, 295),
                (456, 298), (472, 295),
            ]
        );

        // (3) find_accessible_within — two starts in chunk (0,0) that partition its
        // six chargers: inside the isolated house at (29,37) vs. the open road.
        let from_house = w.entities.find_accessible_within(&w.passability, (30, 37), 0, REACH, None);
        assert_eq!(sorted_coords(&from_house), vec![(29, 37)]);

        let from_road = w.entities.find_accessible_within(&w.passability, (2, 2), 0, REACH, None);
        assert_eq!(
            sorted_coords(&from_road),
            vec![(42, 59), (47, 85), (63, 97), (74, 34), (91, 33)]
        );
    }

    /// Re-derives each golden by an independent route (not the function under test),
    /// proving the stored source-of-truth values are actually correct rather than
    /// just self-consistent.
    #[test]
    fn golden_locator_values_are_correct() {
        let w = TestWorld::load();
        let mut all: Vec<(i32, i32)> =
            w.entities.iter().map(|e| (e.coordinates.x, e.coordinates.y)).collect();
        all.sort();

        // (1) radius: brute-force distance filter over every charger.
        let manual_radius: Vec<(i32, i32)> = all
            .iter()
            .copied()
            .filter(|&(x, y)| {
                let (dx, dy) = ((x - 29) as i64, (y - 37) as i64);
                dx * dx + dy * dy <= 40 * 40
            })
            .collect();
        assert_eq!(manual_radius, vec![(29, 37), (42, 59)]);

        // (2) rendered: brute-force chunk-membership filter over every charger.
        let footprint = crate::map::hypermap_world::rendered_chunks_around(
            2 * HYPERMAP_CHUNK_SIZE + 64,
            2 * HYPERMAP_CHUNK_SIZE + 64,
        );
        let manual_rendered: Vec<(i32, i32)> = all
            .iter()
            .copied()
            .filter(|&(x, y)| {
                footprint.contains(&crate::map::hypermap::world_to_chunk_local(x, y).0)
            })
            .collect();
        assert_eq!(manual_rendered.len(), 14);
        assert_eq!(
            manual_rendered,
            vec![
                (284, 301), (290, 324), (299, 464), (321, 292), (325, 344), (326, 352),
                (333, 351), (338, 321), (352, 412), (354, 446), (437, 360), (453, 295),
                (456, 298), (472, 295),
            ]
        );

        // (3) accessible: re-derive via A* (a different algorithm than the locator's
        // BFS). Chargers are walkable, so A* to the charger tile is exact.
        let chunk00: Vec<(i32, i32)> =
            all.iter().copied().filter(|&(x, y)| x < 128 && y < 128).collect();
        assert_eq!(
            chunk00,
            vec![(29, 37), (42, 59), (47, 85), (63, 97), (74, 34), (91, 33)],
            "the six chargers of chunk (0,0)"
        );
        let limits = HypermapSearchLimits { max_expanded: 200_000 };
        let astar_reaches = |start: (i32, i32)| -> Vec<(i32, i32)> {
            chunk00
                .iter()
                .copied()
                .filter(|&goal| {
                    matches!(
                        astar_shortest_world_path(&w.passability, start, goal, limits),
                        HypermapPathResult::Found { .. }
                    )
                })
                .collect()
        };
        // The two starts partition the chunk's chargers exactly as the golden says.
        assert_eq!(astar_reaches((30, 37)), vec![(29, 37)]);
        assert_eq!(
            astar_reaches((2, 2)),
            vec![(42, 59), (47, 85), (63, 97), (74, 34), (91, 33)]
        );
    }
}

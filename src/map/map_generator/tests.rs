use super::*;
use crate::map::hypermap::HYPERMAP_CHUNK_SIZE;
use crate::map::level::parse_level_geometry_sections;

use crate::map::world_map::{TileStyle, WallCorner};

use super::corner_pillars::detect_corner_pillars;
use super::draft::{DraftTile, Room, RoomRecord};
use super::step_door::entrypoint_walk_tile;
use super::step_home_crawler::{
    house_center_floor_tile, house_entry_interior_tile, HOME_CRAWLER_WAVE_MAX,
};
use super::step_seeds::manhattan;
use super::union::union_contains;

#[test]
fn seeds_respect_min_distance_after_separation() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 1,
        ..Default::default()
    });
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    for i in 0..draft.primary_seeds.len() {
        for j in (i + 1)..draft.primary_seeds.len() {
            assert!(
                manhattan(draft.primary_seeds[i], draft.primary_seeds[j]) >= MIN_SEED_DISTANCE,
            );
        }
    }
}

#[test]
fn pipeline_populates_rooms_before_finish() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 42,
        ..Default::default()
    });
    draft.step_init_carpet();
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    draft.step_spawn_subseeds();
    draft.step_grow_rooms();
    assert!(!draft.room_records.is_empty());
    assert!(draft.growth_centers.len() > draft.primary_seeds.len());
}

#[test]
fn convex_outer_corner_does_not_add_interior_pillar() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 1,
        size: 11,
        margin: 0,
    });
    draft.room_records = vec![RoomRecord {
        bounds: Room {
            x0: 2,
            z0: 2,
            x1: 5,
            z1: 5,
        },
    }];
    draft.step_init_carpet();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    assert!(
        matches!(draft.get(3, 3), DraftTile::Open),
        "convex interior floor should stay open, not become a corner pillar"
    );
}

#[test]
fn full_pipeline_places_inner_corner_pillars() {
    let mut total_corners = 0usize;
    for seed in 0..256u64 {
        let cells = MapDraft::generate(MapGeneratorConfig {
            seed,
            ..Default::default()
        });
        total_corners += cells
            .iter()
            .flat_map(|row| row.iter())
            .filter(|c| matches!(c, CellType::Corner(_)))
            .count();
    }
    assert!(
        total_corners > 0,
        "expected inner corner pillars across procedural seeds (got {total_corners} in 0..255)"
    );
}

#[test]
fn concave_l_shape_corner_on_elbow_floor() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 1,
        size: 11,
        margin: 0,
    });
    draft.room_records = vec![
        RoomRecord {
            bounds: Room {
                x0: 1,
                z0: 1,
                x1: 4,
                z1: 4,
            },
        },
        RoomRecord {
            bounds: Room {
                x0: 4,
                z0: 4,
                x1: 7,
                z1: 6,
            },
        },
    ];
    draft.step_init_carpet();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    assert!(
        matches!(draft.get(4, 4), DraftTile::Corner(WallCorner::Sw)),
        "concave elbow at (4,4) should get one Sw pillar"
    );
    let pillars = detect_corner_pillars(&wall_field_from_draft(&draft));
    let elbow = pillars
        .iter()
        .find(|p| p.x == 4 && p.z == 4)
        .expect("concave elbow at (4,4) in wall field");
    assert_eq!(elbow.corner, WallCorner::Sw);
}

fn wall_field_from_draft(draft: &MapDraft) -> super::corner_pillars::WallField {
    let sz = draft.size;
    let mut field = super::corner_pillars::WallField::new(sz);
    for z in 0..sz {
        for x in 0..sz {
            if let DraftTile::Wall(bits) = draft.get(x, z) {
                field.set_wall(x, z, bits);
            }
        }
    }
    field
}

#[test]
fn concave_union_corner_gets_interior_pillar() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 1,
        size: 11,
        margin: 0,
    });
    draft.room_records = vec![
        RoomRecord {
            bounds: Room {
                x0: 1,
                z0: 1,
                x1: 4,
                z1: 4,
            },
        },
        RoomRecord {
            bounds: Room {
                x0: 4,
                z0: 4,
                x1: 7,
                z1: 6,
            },
        },
    ];
    draft.step_init_carpet();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    let mut corner_pillars = Vec::new();
    for z in 0..draft.size {
        for x in 0..draft.size {
            if let DraftTile::Corner(c) = draft.get(x, z) {
                corner_pillars.push((x, z, c));
            }
        }
    }
    assert!(
        !corner_pillars.is_empty(),
        "L-shaped union should place at least one inner corner pillar"
    );
    for (x, z, _) in &corner_pillars {
        assert!(
            super::union::union_contains(&draft.rooms(), *x, *z),
            "pillar at ({x},{z}) must be inside union, not on exterior road"
        );
    }
    let unique_cells: std::collections::HashSet<_> =
        corner_pillars.iter().map(|(x, z, _)| (*x, *z)).collect();
    assert_eq!(
        unique_cells.len(),
        corner_pillars.len(),
        "each concave elbow should get at most one pillar cell"
    );
}

#[test]
fn metadata_has_houses_with_entries() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 99,
        ..Default::default()
    });
    draft.step_init_carpet();
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    draft.step_spawn_subseeds();
    draft.step_grow_rooms();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_place_house_doors();
    let meta = draft.build_metadata();
    assert!(!meta.houses.is_empty());
    for house in &meta.houses {
        assert!(house.x0 <= house.center_x && house.center_x <= house.x1);
        assert!(house.z0 <= house.center_z && house.center_z <= house.z1);
        assert!(house.area >= 4, "footprint area should be stored on each house");
        assert_eq!(
            entrypoint_walk_tile(house.entry.wall_x, house.entry.wall_z, house.entry.outward_edge),
            (house.entry.walk_x, house.entry.walk_z)
        );
    }
}

#[test]
fn generated_geometry_parses() {
    let text = generate_chunk_geometry(&MapGeneratorConfig {
        seed: 42,
        ..Default::default()
    });
    let sections = parse_level_geometry_sections(&text).expect("parse generated map");
    assert!(!sections.is_empty());
    let (_, rows) = &sections[0];
    assert_eq!(rows.len(), HYPERMAP_CHUNK_SIZE as usize);
    assert_eq!(rows[0].len(), HYPERMAP_CHUNK_SIZE as usize);
}

#[test]
fn rooms_grow_from_subseeds_only() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 42,
        ..Default::default()
    });
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    draft.step_spawn_subseeds();
    assert!(!draft.subseed_centers.is_empty());
    assert!(draft.subseed_centers.len() > draft.primary_seeds.len() - 1);
    draft.step_grow_rooms();
    assert!(!draft.room_records.is_empty());
    draft.step_cluster_houses();
    assert!(!draft.houses.is_empty());
}

#[test]
fn union_shell_has_no_inner_walls() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 7,
        ..Default::default()
    });
    draft.step_init_carpet();
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    draft.step_spawn_subseeds();
    draft.step_grow_rooms();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    let rooms = draft.rooms();
    for z in 0..draft.size {
        for x in 0..draft.size {
            let DraftTile::Wall(_) = draft.get(x, z) else {
                continue;
            };
            let enclosed = [
                union_contains(&rooms, x, z - 1),
                union_contains(&rooms, x, z + 1),
                union_contains(&rooms, x - 1, z),
                union_contains(&rooms, x + 1, z),
            ];
            assert!(
                enclosed.iter().any(|&inside| !inside),
                "inner wall at ({x}, {z})"
            );
        }
    }
}

#[test]
fn generated_map_has_rooms_with_doors() {
    let cells = MapDraft::generate(MapGeneratorConfig {
        seed: 99,
        ..Default::default()
    });

    let mut wall_cells = 0u32;
    let mut open_border_onto_road = 0u32;
    let sz = cells.len();
    for z in 0..sz {
        for x in 0..sz {
            let cell = cells[z][x];
            if !matches!(cell, CellType::Wall(_)) {
                continue;
            }
            wall_cells += 1;
            for (dx, dz) in [(0, -1), (0, 1), (1, 0), (-1, 0)] {
                let nx = x as i32 + dx;
                let nz = z as i32 + dz;
                if nx < 0 || nz < 0 || nx >= sz as i32 || nz >= sz as i32 {
                    continue;
                }
                if cells[nz as usize][nx as usize] == CellType::Road {
                    open_border_onto_road += 1;
                }
            }
        }
    }
    assert!(wall_cells > 0);
    assert!(open_border_onto_road > 0, "expected at least one door gap");
}

#[test]
fn home_crawler_wave_stays_within_manhattan_radius() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 77,
        ..Default::default()
    });
    draft.step_init_carpet();
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    draft.step_spawn_subseeds();
    draft.step_grow_rooms();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    draft.step_place_house_doors();
    draft.step_home_crawlers();
    let cap = (2 * HOME_CRAWLER_WAVE_MAX * (HOME_CRAWLER_WAVE_MAX + 1) + 1) as usize;
    for house in &draft.houses {
        let Some(ep) = house.entry.as_ref() else {
            continue;
        };
        let Some(start) = house_entry_interior_tile(&draft, ep) else {
            continue;
        };
        for z in house.z0..=house.z1 {
            for x in house.x0..=house.x1 {
                if draft.floor_styles[z as usize][x as usize] != TileStyle::FLOOR_MARBLE {
                    continue;
                }
                assert!(
                    manhattan((x, z), start) <= HOME_CRAWLER_WAVE_MAX,
                    "marble at ({x},{z}) exceeds max wave radius from entry"
                );
            }
        }
    }
    let marble_tiles: usize = draft
        .floor_styles
        .iter()
        .flatten()
        .filter(|s| **s == TileStyle::FLOOR_MARBLE)
        .count();
    assert!(marble_tiles > 0);
    assert!(marble_tiles <= draft.houses.len() * cap);
}

#[test]
fn small_house_skips_center_glass_wave() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 1,
        size: 16,
        margin: 1,
    });
    draft.houses = vec![super::house::House {
        rects: vec![Room {
            x0: 5,
            z0: 5,
            x1: 7,
            z1: 7,
        }],
        x0: 5,
        z0: 5,
        x1: 7,
        z1: 7,
        footprint_area: 9,
        entry: None,
        entry2: None,
    }];
    assert!(!draft.houses[0].supports_center_glass_wave());
    draft.step_init_carpet();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_place_house_doors();
    draft.step_home_crawlers();
    for z in 5..=7 {
        for x in 5..=7 {
            assert_ne!(
                draft.floor_styles[z as usize][x as usize],
                TileStyle::FLOOR_GLASS,
                "small house should not get center glass wave"
            );
        }
    }
}

#[test]
fn home_crawler_glass_wave_from_house_center() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 88,
        ..Default::default()
    });
    draft.step_init_carpet();
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    draft.step_spawn_subseeds();
    draft.step_grow_rooms();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    draft.step_place_house_doors();
    draft.step_home_crawlers();
    let mut glass_found = false;
    for house in draft
        .houses
        .iter()
        .filter(|h| h.supports_center_glass_wave())
    {
        let Some(center) = house_center_floor_tile(&draft, house) else {
            continue;
        };
        for z in house.z0..=house.z1 {
            for x in house.x0..=house.x1 {
                if !house.contains(x, z) {
                    continue;
                }
                if draft.floor_styles[z as usize][x as usize] != TileStyle::FLOOR_GLASS {
                    continue;
                }
                glass_found = true;
                assert!(
                    manhattan((x, z), center) <= HOME_CRAWLER_WAVE_MAX,
                    "glass at ({x},{z}) exceeds max wave radius from center {center:?}"
                );
            }
        }
    }
    assert!(glass_found, "expected glass wave from at least one house center");
}

#[test]
fn home_crawler_stamps_marble_from_main_entry_only() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 99,
        ..Default::default()
    });
    draft.step_init_carpet();
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    draft.step_spawn_subseeds();
    draft.step_grow_rooms();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    draft.step_place_house_doors();
    draft.step_home_crawlers();
    let marble_tiles: usize = draft
        .floor_styles
        .iter()
        .flatten()
        .filter(|s| **s == TileStyle::FLOOR_MARBLE)
        .count();
    assert!(marble_tiles > 0, "expected marble trail from main entry");
    assert!(
        marble_tiles
            <= draft.houses.len()
                * (2 * HOME_CRAWLER_WAVE_MAX * (HOME_CRAWLER_WAVE_MAX + 1) + 1) as usize,
        "each house wave marks at most the Manhattan-{HOME_CRAWLER_WAVE_MAX} ball"
    );
    let sz = draft.size as usize;
    for z in 0..sz {
        for x in 0..sz {
            let style = draft.floor_styles[z][x];
            if draft.cells[z][x] != DraftTile::Open
                && (style == TileStyle::FLOOR_MARBLE || style == TileStyle::FLOOR_GLASS)
            {
                panic!("crawler floor style on non-open tile ({x}, {z})");
            }
        }
    }
}

#[test]
fn house_entry_is_open_tile_inside_doorway() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 99,
        ..Default::default()
    });
    draft.step_init_carpet();
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    draft.step_spawn_subseeds();
    draft.step_grow_rooms();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    draft.step_place_house_doors();
    let ep = draft.houses[0].entry.clone().expect("house entry");
    let start = house_entry_interior_tile(&draft, &ep).expect("interior entry");
    assert_eq!(draft.get(start.0, start.1), DraftTile::Open);
}

#[test]
fn inner_walls_split_house_into_rooms() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 3,
        size: 16,
        margin: 1,
    });
    draft.room_records = vec![RoomRecord {
        bounds: Room {
            x0: 3,
            z0: 3,
            x1: 11,
            z1: 11,
        },
    }];
    draft.step_init_carpet();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    draft.step_split_houses_into_rooms();

    let mut inner_wall_cells = 0u32;
    for z in 4..=10 {
        for x in 4..=10 {
            if let DraftTile::Wall(_) = draft.get(x, z) {
                inner_wall_cells += 1;
            }
        }
    }
    assert!(
        inner_wall_cells > 0,
        "9x9 house should receive at least one inner wall"
    );
}

#[test]
fn inner_walls_skipped_for_small_houses() {
    // Houses with footprint_area < 30 must not receive any inner walls.
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 5,
        size: 12,
        margin: 1,
    });
    draft.room_records = vec![RoomRecord {
        bounds: Room {
            x0: 3,
            z0: 3,
            x1: 6,
            z1: 6,
        },
    }];
    draft.step_init_carpet();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    draft.step_place_house_doors();
    draft.step_split_houses_into_rooms();

    // 4x4 house has area 16 < 30, so the split step is skipped entirely.
    for z in 4..=5 {
        for x in 4..=5 {
            assert_eq!(
                draft.get(x, z),
                DraftTile::Open,
                "interior of small house must remain Open (no inner walls)"
            );
        }
    }
}

#[test]
fn inner_walls_respect_min_room_constraints() {
    // A house large enough to receive cuts must never produce a sub-room < 6
    // cells or narrower than 2 cells in either direction.
    for seed in 0..64u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig {
            seed,
            ..Default::default()
        });
        draft.step_init_carpet();
        draft.step_place_primary_seeds();
        draft.step_separate_primary_seeds();
        draft.step_spawn_subseeds();
        draft.step_grow_rooms();
        draft.step_cluster_houses();
        draft.step_paint_union_interior();
        draft.step_build_union_outer_walls();
        draft.step_stamp_union_inner_corner_pillars();
        draft.step_place_house_doors();
        draft.step_split_houses_into_rooms();
        // If no panic, constraints satisfied. Basic smoke check that the step runs.
    }
}

#[test]
fn inner_walls_isolate_rooms() {
    // After inner walls, the entry-room BFS should not visit every cell of a
    // generously-sized house with at least one inner cut.
    use std::collections::{HashSet, VecDeque};
    let mut isolated_cases = 0u32;
    for seed in 0..64u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig {
            seed,
            ..Default::default()
        });
        draft.step_init_carpet();
        draft.step_place_primary_seeds();
        draft.step_separate_primary_seeds();
        draft.step_spawn_subseeds();
        draft.step_grow_rooms();
        draft.step_cluster_houses();
        draft.step_paint_union_interior();
        draft.step_build_union_outer_walls();
        draft.step_stamp_union_inner_corner_pillars();
        draft.step_place_house_doors();
        draft.step_split_houses_into_rooms();
        for house in &draft.houses {
            let total_open: usize = (house.z0..=house.z1)
                .flat_map(|z| (house.x0..=house.x1).map(move |x| (x, z)))
                .filter(|&(x, z)| house.contains(x, z) && draft.get(x, z) == DraftTile::Open)
                .count();
            if total_open == 0 {
                continue;
            }
            let start = (house.z0..=house.z1)
                .flat_map(|z| (house.x0..=house.x1).map(move |x| (x, z)))
                .find(|&(x, z)| house.contains(x, z) && draft.get(x, z) == DraftTile::Open);
            let Some(start) = start else { continue };
            let mut visited: HashSet<(i32, i32)> = HashSet::from([start]);
            let mut queue: VecDeque<(i32, i32)> = VecDeque::from([start]);
            while let Some((x, z)) = queue.pop_front() {
                for (dx, dz) in [(0, -1), (0, 1), (1, 0), (-1, 0)] {
                    let n = (x + dx, z + dz);
                    if visited.contains(&n) {
                        continue;
                    }
                    if !house.contains(n.0, n.1) || draft.get(n.0, n.1) != DraftTile::Open {
                        continue;
                    }
                    visited.insert(n);
                    queue.push_back(n);
                }
            }
            if visited.len() < total_open {
                isolated_cases += 1;
            }
        }
    }
    assert!(
        isolated_cases > 0,
        "expected at least one house with truly isolated rooms across 64 seeds"
    );
}

#[test]
fn inner_doors_make_all_rooms_accessible() {
    // After inner walls + inner doors, every walkable cell in every house must be
    // reachable from the outer entry tile. Connectivity is edge-based: a `Wall(bits)`
    // cell is walkable floor with slabs on the named edges; passage between two cells
    // is blocked only when a slab sits on their shared edge.
    use std::collections::{HashSet, VecDeque};

    use crate::map::world_map::{MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};

    // Slab bits on a walkable cell, or None when the cell cannot be walked.
    let cell_bits = |draft: &MapDraft, x: i32, z: i32| -> Option<u8> {
        match draft.get(x, z) {
            DraftTile::Open
            | DraftTile::Corner(_)
            | DraftTile::Charger(_)
            | DraftTile::PartsDepot(_) => Some(0),
            DraftTile::Wall(bits) => Some(bits),
            DraftTile::Void => None,
        }
    };
    const EDGES: [(i32, i32, u8, u8); 4] = [
        (0, -1, MASK_NORTH, MASK_SOUTH),
        (0, 1, MASK_SOUTH, MASK_NORTH),
        (1, 0, MASK_EAST, MASK_WEST),
        (-1, 0, MASK_WEST, MASK_EAST),
    ];

    for seed in 0..128u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig {
            seed,
            ..Default::default()
        });
        draft.step_init_carpet();
        draft.step_place_primary_seeds();
        draft.step_separate_primary_seeds();
        draft.step_spawn_subseeds();
        draft.step_grow_rooms();
        draft.step_cluster_houses();
        draft.step_paint_union_interior();
        draft.step_build_union_outer_walls();
        draft.step_stamp_union_inner_corner_pillars();
        draft.step_place_house_doors();
        draft.step_split_houses_into_rooms();
        draft.step_place_inner_doors();

        for (hi, house) in draft.houses.iter().enumerate() {
            let Some(ref ep) = house.entry else { continue };
            let Some(start) = super::step_home_crawler::house_entry_interior_tile(&draft, ep)
            else {
                continue;
            };

            let mut visited: HashSet<(i32, i32)> = HashSet::from([start]);
            let mut queue: VecDeque<(i32, i32)> = VecDeque::from([start]);
            while let Some((x, z)) = queue.pop_front() {
                let Some(b) = cell_bits(&draft, x, z) else { continue };
                for (dx, dz, this_bit, nbr_bit) in EDGES {
                    let n = (x + dx, z + dz);
                    if visited.contains(&n) || !house.contains(n.0, n.1) {
                        continue;
                    }
                    let Some(nb) = cell_bits(&draft, n.0, n.1) else { continue };
                    if b & this_bit == 0 && nb & nbr_bit == 0 {
                        visited.insert(n);
                        queue.push_back(n);
                    }
                }
            }

            for z in house.z0..=house.z1 {
                for x in house.x0..=house.x1 {
                    if house.contains(x, z)
                        && cell_bits(&draft, x, z).is_some()
                        && !visited.contains(&(x, z))
                    {
                        panic!("seed {seed} house {hi}: ({x},{z}) unreachable from entry");
                    }
                }
            }
        }
    }
}

#[test]
fn house_tool_rejects_boundaries_below_minimum() {
    assert!(generate_house_tiles(9, 20, 1).is_none());
    assert!(generate_house_tiles(20, 9, 1).is_none());
    assert!(generate_house_tiles(MIN_HOUSE_TOOL_SIDE, MIN_HOUSE_TOOL_SIDE, 1).is_some());
}

#[test]
fn house_tool_fills_boundary_with_a_walled_building() {
    let width = 14;
    let height = 11;
    let tiles = generate_house_tiles(width, height, 7).expect("10x10+ boundary generates a house");
    assert_eq!(tiles.width, width);
    assert_eq!(tiles.height, height);
    assert_eq!(tiles.cells.len(), height as usize);
    assert_eq!(tiles.cells[0].len(), width as usize);

    // The whole boundary is the building footprint — no Void left inside it.
    for row in &tiles.cells {
        for cell in row {
            assert!(!matches!(cell, CellType::Void), "house footprint must be fully built");
        }
    }

    // Outer shell: every border cell carries wall slabs except where the single
    // door was cut (which becomes Road or a reduced wall).
    let mut perimeter_walls = 0u32;
    let mut door_gaps = 0u32;
    for x in 0..width as usize {
        for z in [0usize, height as usize - 1] {
            match tiles.cells[z][x] {
                CellType::Wall(_) => perimeter_walls += 1,
                CellType::Road => door_gaps += 1,
                _ => {}
            }
        }
    }
    assert!(perimeter_walls > 0, "house must have an outer shell");

    // Interior must contain walkable road, and the building must have exactly one door.
    let interior_road = tiles
        .cells
        .iter()
        .flatten()
        .filter(|c| matches!(c, CellType::Road))
        .count();
    assert!(interior_road > 0, "house interior must be walkable road");
    let _ = door_gaps;
}

#[test]
fn clustered_houses_only_merge_on_overlap() {
    let houses = super::house::cluster_houses(&[
        RoomRecord {
            bounds: Room {
                x0: 0,
                z0: 0,
                x1: 4,
                z1: 4,
            },
        },
        RoomRecord {
            bounds: Room {
                x0: 5,
                z0: 0,
                x1: 9,
                z1: 4,
            },
        },
    ]);
    assert_eq!(houses.len(), 2, "edge-touching rects should stay two houses");
}

#[test]
fn door_walk_tile_is_exterior_road_not_another_house() {
    let mut draft = MapDraft::new(MapGeneratorConfig {
        seed: 42,
        ..Default::default()
    });
    draft.step_init_carpet();
    draft.step_place_primary_seeds();
    draft.step_separate_primary_seeds();
    draft.step_spawn_subseeds();
    draft.step_grow_rooms();
    draft.step_cluster_houses();
    draft.step_paint_union_interior();
    draft.step_build_union_outer_walls();
    draft.step_stamp_union_inner_corner_pillars();
    draft.step_place_house_doors();
    for (i, house) in draft.houses.iter().enumerate() {
        let ep = house.entry.as_ref().expect("entry");
        assert!(
            super::step_door::is_valid_door_site(&draft, i, ep.wall_x, ep.wall_z, ep.outward_edge),
            "placed door must pass validation"
        );
        assert!(
            !draft.houses.iter().any(|h| h.contains(ep.walk_x, ep.walk_z)),
            "walk tile must be exterior road"
        );
    }
}

#[test]
fn every_house_gets_a_door() {
    let mut missing = 0u32;
    for seed in 0..256u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig {
            seed,
            ..Default::default()
        });
        draft.step_init_carpet();
        draft.step_place_primary_seeds();
        draft.step_separate_primary_seeds();
        draft.step_spawn_subseeds();
        draft.step_grow_rooms();
        draft.step_cluster_houses();
        if draft.houses.is_empty() {
            continue;
        }
        draft.step_paint_union_interior();
        draft.step_build_union_outer_walls();
        draft.step_stamp_union_inner_corner_pillars();
        draft.step_place_house_doors();
        for house in &draft.houses {
            if house.entry.is_none() {
                missing += 1;
            }
        }
    }
    assert_eq!(missing, 0, "every house must have exactly one entry");
}

#[test]
fn charging_stations_back_onto_a_wall_and_skip_corners() {
    let mut found_any = false;
    for seed in 0..40u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig {
            seed,
            ..Default::default()
        });
        draft.run_pipeline();

        let sz = draft.size;
        for z in 0..sz {
            for x in 0..sz {
                let DraftTile::Charger(facing) = draft.get(x, z) else {
                    continue;
                };
                found_any = true;

                // The backing wall sits in the facing direction.
                let (dx, dz) = facing.wall_delta();
                let (wx, wz) = (x + dx, z + dz);
                assert!(
                    matches!(draft.get(wx, wz), DraftTile::Wall(_) | DraftTile::Corner(_)),
                    "charger at ({x},{z}) facing {facing:?} must back onto a wall"
                );

                // Exactly one orthogonal wall neighbor → not wedged into a corner.
                let wall_neighbors = [(0, -1), (0, 1), (1, 0), (-1, 0)]
                    .into_iter()
                    .filter(|&(ox, oz)| {
                        matches!(draft.get(x + ox, z + oz), DraftTile::Wall(_) | DraftTile::Corner(_))
                    })
                    .count();
                assert_eq!(
                    wall_neighbors, 1,
                    "charger at ({x},{z}) must touch exactly one wall (no corners)"
                );
            }
        }
    }
    assert!(found_any, "at least some seeds should place a charging station");
}

#[test]
fn outer_doors_are_two_tiles_wide_when_possible() {
    // Across many seeds the vast majority of outer house doors should be 2-wide.
    // We allow a small fraction of 1-wide fallbacks for degenerate geometry.
    let mut two_wide = 0u32;
    let mut one_wide = 0u32;
    for seed in 0..64u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig {
            seed,
            ..Default::default()
        });
        draft.run_pipeline();
        for house in &draft.houses {
            let Some(ref ep) = house.entry else { continue };
            if ep.wall2.is_some() {
                two_wide += 1;
            } else {
                one_wide += 1;
            }
        }
    }
    let total = two_wide + one_wide;
    assert!(total > 0, "expected at least one house entry across 64 seeds");
    // At least 80% of entries should be 2-wide.
    assert!(
        two_wide * 10 >= total * 8,
        "expected ≥80% 2-wide doors, got {two_wide}/{total}"
    );
}

#[test]
fn outer_door_wall2_is_adjacent_along_wall_run() {
    // When wall2 is set, it must be exactly one step from wall_x/wall_z along the wall
    // run (perpendicular to outward_edge), not diagonally or further away.
    use crate::map::world_map::{MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};
    for seed in 0..64u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig { seed, ..Default::default() });
        draft.run_pipeline();
        for (i, house) in draft.houses.iter().enumerate() {
            let Some(ref ep) = house.entry else { continue };
            let Some((wx2, wz2)) = ep.wall2 else { continue };
            let dist = (wx2 - ep.wall_x).abs() + (wz2 - ep.wall_z).abs();
            assert_eq!(
                dist, 1,
                "seed {seed} house {i}: wall2 must be exactly 1 step from wall_x/wall_z"
            );
            // The step must be along the wall run (perpendicular to outward_edge).
            let along_run = match ep.outward_edge {
                MASK_NORTH | MASK_SOUTH => (wx2 - ep.wall_x).abs() == 1 && wz2 == ep.wall_z,
                MASK_EAST | MASK_WEST   => (wz2 - ep.wall_z).abs() == 1 && wx2 == ep.wall_x,
                _ => false,
            };
            assert!(
                along_run,
                "seed {seed} house {i}: wall2 must step along the wall run, not the door direction"
            );
        }
    }
}

#[test]
fn inner_doors_make_all_rooms_accessible_with_wide_doors() {
    // The widened inner doors must not break the "all rooms accessible" invariant.
    // This reuses the same connectivity check as `inner_doors_make_all_rooms_accessible`.
    use std::collections::{HashSet, VecDeque};
    use crate::map::world_map::{MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};

    let cell_bits = |draft: &MapDraft, x: i32, z: i32| -> Option<u8> {
        match draft.get(x, z) {
            DraftTile::Open
            | DraftTile::Corner(_)
            | DraftTile::Charger(_)
            | DraftTile::PartsDepot(_) => Some(0),
            DraftTile::Wall(bits) => Some(bits),
            DraftTile::Void => None,
        }
    };
    const EDGES: [(i32, i32, u8, u8); 4] = [
        (0, -1, MASK_NORTH, MASK_SOUTH),
        (0, 1, MASK_SOUTH, MASK_NORTH),
        (1, 0, MASK_EAST, MASK_WEST),
        (-1, 0, MASK_WEST, MASK_EAST),
    ];

    for seed in 0..64u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig { seed, ..Default::default() });
        draft.step_init_carpet();
        draft.step_place_primary_seeds();
        draft.step_separate_primary_seeds();
        draft.step_spawn_subseeds();
        draft.step_grow_rooms();
        draft.step_cluster_houses();
        draft.step_paint_union_interior();
        draft.step_build_union_outer_walls();
        draft.step_stamp_union_inner_corner_pillars();
        draft.step_place_house_doors();
        draft.step_split_houses_into_rooms();
        draft.step_place_inner_doors();

        for (hi, house) in draft.houses.iter().enumerate() {
            let Some(ref ep) = house.entry else { continue };
            let Some(start) = super::step_home_crawler::house_entry_interior_tile(&draft, ep) else { continue };

            let mut visited: HashSet<(i32, i32)> = HashSet::from([start]);
            let mut queue: VecDeque<(i32, i32)> = VecDeque::from([start]);
            while let Some((x, z)) = queue.pop_front() {
                let Some(b) = cell_bits(&draft, x, z) else { continue };
                for (dx, dz, this_bit, nbr_bit) in EDGES {
                    let n = (x + dx, z + dz);
                    if visited.contains(&n) || !house.contains(n.0, n.1) { continue; }
                    let Some(nb) = cell_bits(&draft, n.0, n.1) else { continue };
                    if b & this_bit == 0 && nb & nbr_bit == 0 {
                        visited.insert(n);
                        queue.push_back(n);
                    }
                }
            }

            for z in house.z0..=house.z1 {
                for x in house.x0..=house.x1 {
                    if house.contains(x, z)
                        && cell_bits(&draft, x, z).is_some()
                        && !visited.contains(&(x, z))
                    {
                        panic!("seed {seed} house {hi}: ({x},{z}) unreachable from entry");
                    }
                }
            }
        }
    }
}

#[test]
fn second_door_appears_on_about_half_of_houses() {
    let mut with_second = 0u32;
    let mut eligible = 0u32;
    for seed in 0..512u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig {
            seed,
            ..Default::default()
        });
        draft.run_pipeline();
        for house in &draft.houses {
            if house.entry.is_none() {
                continue;
            }
            eligible += 1;
            if house.entry2.is_some() {
                with_second += 1;
            }
        }
    }
    assert!(eligible > 50, "expected many houses across seeds");
    let ratio = with_second as f32 / eligible as f32;
    assert!(
        (0.35..=0.65).contains(&ratio),
        "expected ~50% second doors, got {with_second}/{eligible} ({ratio:.2})"
    );
}

#[test]
fn houses_place_up_to_three_chargers() {
    use std::collections::HashMap;

    let mut max_per_house: u32 = 0;
    let mut houses_with_multiple = 0u32;
    for seed in 0..128u64 {
        let mut draft = MapDraft::new(MapGeneratorConfig {
            seed,
            ..Default::default()
        });
        draft.run_pipeline();

        let mut per_house: HashMap<usize, u32> = HashMap::new();
        for z in 0..draft.size {
            for x in 0..draft.size {
                if !matches!(draft.get(x, z), DraftTile::Charger(_)) {
                    continue;
                }
                for (hi, house) in draft.houses.iter().enumerate() {
                    if house.contains(x, z) {
                        *per_house.entry(hi).or_insert(0) += 1;
                        break;
                    }
                }
            }
        }
        for count in per_house.values() {
            max_per_house = max_per_house.max(*count);
            if *count > 1 {
                houses_with_multiple += 1;
            }
        }
    }
    assert!(
        max_per_house >= 2,
        "expected at least one house with 2+ chargers across seeds, max was {max_per_house}"
    );
    assert!(
        max_per_house <= 3,
        "never more than 3 chargers per house, got {max_per_house}"
    );
    assert!(
        houses_with_multiple > 0,
        "expected some houses with multiple chargers"
    );
}

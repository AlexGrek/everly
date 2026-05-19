use super::*;
use crate::map::hypermap::HYPERMAP_CHUNK_SIZE;
use crate::map::level::parse_level_geometry_sections;

use crate::map::world_map::{TileStyle, WallCorner};

use super::corner_pillars::detect_corner_pillars;
use super::draft::{DraftTile, Room, RoomRecord};
use super::step_door::entrypoint_walk_tile;
use super::step_home_crawler::{house_entry_interior_tile, HOME_CRAWLER_WAVE_MAX};
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
            if draft.cells[z][x] != DraftTile::Open
                && draft.floor_styles[z][x] == TileStyle::FLOOR_MARBLE
            {
                panic!("marble style on non-open tile ({x}, {z})");
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

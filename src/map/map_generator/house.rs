//! Houses: merged subseed room footprints (subseed data discarded after clustering).

use super::draft::{MapDraft, Room, RoomRecord};
use super::types::HouseEntrypoint;

/// One building footprint — possibly several merged axis-aligned rects.
#[derive(Debug, Clone)]
pub(crate) struct House {
    pub rects: Vec<Room>,
    pub x0: i32,
    pub z0: i32,
    pub x1: i32,
    pub z1: i32,
    pub entry: Option<HouseEntrypoint>,
}

impl House {
    pub fn contains(&self, x: i32, z: i32) -> bool {
        self.rects.iter().any(|r| r.contains(x, z))
    }

    pub fn center(&self) -> (i32, i32) {
        ((self.x0 + self.x1) / 2, (self.z0 + self.z1) / 2)
    }

    pub fn to_generated(&self) -> super::types::GeneratedHouse {
        let (center_x, center_z) = self.center();
        super::types::GeneratedHouse {
            x0: self.x0,
            z0: self.z0,
            x1: self.x1,
            z1: self.z1,
            center_x,
            center_z,
            entry: self.entry.clone().expect("house must have entry before metadata"),
        }
    }
}

/// Group touching / overlapping subseed rects into whole houses.
pub(crate) fn cluster_houses(room_records: &[RoomRecord]) -> Vec<House> {
    let n = room_records.len();
    if n == 0 {
        return Vec::new();
    }
    let mut parent: Vec<usize> = (0..n).collect();
    for i in 0..n {
        for j in (i + 1)..n {
            if rects_connect(room_records[i].bounds, room_records[j].bounds) {
                union_sets(&mut parent, i, j);
            }
        }
    }
    let mut groups: std::collections::HashMap<usize, Vec<Room>> = std::collections::HashMap::new();
    for i in 0..n {
        let root = find_set(&mut parent, i);
        groups
            .entry(root)
            .or_default()
            .push(room_records[i].bounds);
    }
    groups
        .into_values()
        .map(|rects| {
            let x0 = rects.iter().map(|r| r.x0).min().unwrap();
            let z0 = rects.iter().map(|r| r.z0).min().unwrap();
            let x1 = rects.iter().map(|r| r.x1).max().unwrap();
            let z1 = rects.iter().map(|r| r.z1).max().unwrap();
            House {
                rects,
                x0,
                z0,
                x1,
                z1,
                entry: None,
            }
        })
        .collect()
}

fn find_set(parent: &mut [usize], i: usize) -> usize {
    if parent[i] != i {
        let root = find_set(parent, parent[i]);
        parent[i] = root;
    }
    parent[i]
}

fn union_sets(parent: &mut [usize], a: usize, b: usize) {
    let ra = find_set(parent, a);
    let rb = find_set(parent, b);
    if ra != rb {
        parent[rb] = ra;
    }
}

/// True when rectangles overlap (touching-only rects stay separate houses).
fn rects_connect(a: Room, b: Room) -> bool {
    a.x0 <= b.x1 && b.x0 <= a.x1 && a.z0 <= b.z1 && b.z0 <= a.z1
}

pub(crate) fn house_contains(house: &House, x: i32, z: i32) -> bool {
    house.contains(x, z)
}

pub(crate) fn house_on_perimeter(house: &House, x: i32, z: i32) -> bool {
    if !house.contains(x, z) {
        return false;
    }
    !house.contains(x, z - 1)
        || !house.contains(x, z + 1)
        || !house.contains(x - 1, z)
        || !house.contains(x + 1, z)
}

pub(crate) fn all_house_rects(draft: &MapDraft) -> Vec<Room> {
    draft
        .houses
        .iter()
        .flat_map(|h| h.rects.iter().copied())
        .collect()
}

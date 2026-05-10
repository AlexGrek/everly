#!/usr/bin/env python3
"""
Handcrafted Everly center chunk map (64×64, 2-char cells).

Produces `world_map.txt`: void margin, road spine, varied building footprints
(rectangles, L-shapes, hollow frames, unions). Wall tokens use the same
bitmask rules as `docs/tilemap.md` / `src/world_map.rs`.

Each building gets at least one **exterior door**: an edge wall cell loses the
one wall bit that faces `__` (road), so there is a gap onto the street/courtyard.

Usage:
  python3 scripts/generate_world_map.py
  python3 scripts/generate_world_map.py --output path/to/world_map.txt
"""

from __future__ import annotations

import argparse
from pathlib import Path

SZ = 64
MARGIN = 2  # void cells around the edge

VOID = -1
ROAD = 0

MASK_N, MASK_S, MASK_E, MASK_W = 1, 2, 4, 8


def rect_cells(x0: int, y0: int, x1: int, y1: int) -> set[tuple[int, int]]:
    """Inclusive axis-aligned rectangle."""
    return {(x, y) for y in range(y0, y1 + 1) for x in range(x0, x1 + 1)}


def region_minus(outer: set[tuple[int, int]], inner: set[tuple[int, int]]) -> set[tuple[int, int]]:
    return outer - inner


def on_road_spine(x: int, y: int) -> bool:
    """Reserved two-cell cross (must stay `__`): vertical x=30–31, horizontal y=28–29."""
    if MARGIN <= y < SZ - MARGIN and 30 <= x <= 31:
        return True
    if MARGIN <= x < SZ - MARGIN and 28 <= y <= 29:
        return True
    return False


def cell_token(mask: int) -> str:
    if mask <= 0:
        return "__"
    return f"w{mask:x}"


def shortcut_token(mask: int) -> str:
    if mask <= 0:
        return "__"
    if mask in (1, 2, 4, 8):
        return {1: "wn", 2: "ws", 4: "we", 8: "ww"}[mask]
    return cell_token(mask)


def stamp_region_masks(occupied: set[tuple[int, int]]) -> dict[tuple[int, int], int]:
    """Wall bitmask per cell from orthogonal footprint (same rules as old stamp_region)."""
    masks: dict[tuple[int, int], int] = {}
    for (x, y) in occupied:
        m = 0
        if (x, y - 1) not in occupied:
            m |= MASK_N
        if (x, y + 1) not in occupied:
            m |= MASK_S
        if (x + 1, y) not in occupied:
            m |= MASK_E
        if (x - 1, y) not in occupied:
            m |= MASK_W
        masks[(x, y)] = m
    return masks


def init_int_grid() -> list[list[int]]:
    g = [[VOID for _ in range(SZ)] for _ in range(SZ)]
    for y in range(MARGIN, SZ - MARGIN):
        for x in range(MARGIN, SZ - MARGIN):
            g[y][x] = ROAD
    return g


def apply_masks_to_grid(grid: list[list[int]], masks: dict[tuple[int, int], int]) -> None:
    for (x, y), m in masks.items():
        grid[y][x] = m


def add_exterior_door_for_building(grid: list[list[int]], occupied: set[tuple[int, int]]) -> None:
    """
    Pick one perimeter cell whose wall faces `__` (ROAD) outside the footprint
    and clear that wall bit so there is an opening (door). If that was the
    last bit, the cell becomes open floor (`__`).
    """
    neighbors = (
        (0, -1, MASK_N),
        (0, 1, MASK_S),
        (1, 0, MASK_E),
        (-1, 0, MASK_W),
    )
    candidates: list[tuple[int, int, int]] = []
    for (x, y) in occupied:
        m = grid[y][x]
        if m <= 0:
            continue
        for dx, dy, wall_bit in neighbors:
            nx, ny = x + dx, y + dy
            if (nx, ny) in occupied:
                continue
            if not (0 <= nx < SZ and 0 <= ny < SZ):
                continue
            if grid[ny][nx] != ROAD:
                continue
            if m & wall_bit:
                candidates.append((x, y, wall_bit))

    if not candidates:
        return

    candidates.sort()
    x, y, bit = candidates[len(candidates) // 2]
    new_m = grid[y][x] & ~bit
    grid[y][x] = ROAD if new_m == 0 else new_m


def apply_spine(grid: list[list[int]]) -> None:
    for y in range(MARGIN, SZ - MARGIN):
        for x in range(MARGIN, SZ - MARGIN):
            if on_road_spine(x, y):
                grid[y][x] = ROAD


def grid_to_lines(grid: list[list[int]]) -> list[str]:
    lines = []
    for y in range(SZ):
        row = []
        for x in range(SZ):
            v = grid[y][x]
            if v == VOID:
                row.append("..")
            elif v == ROAD:
                row.append("__")
            else:
                row.append(shortcut_token(v))
        lines.append(" ".join(row))
    return lines


def collect_buildings() -> list[set[tuple[int, int]]]:
    """
    Footprints entirely inside one quadrant of the road cross (never on spine).

    NW: x 2–29, y 2–27 | NE: x 32–61, y 2–27
    SW: x 2–29, y 30–61 | SE: x 32–61, y 30–61
    """
    buildings: list[set[tuple[int, int]]] = []

    buildings.append(rect_cells(4, 4, 9, 14))
    buildings.append(rect_cells(12, 4, 27, 12))
    buildings.append(rect_cells(4, 16, 8, 27) | rect_cells(4, 22, 20, 27))

    buildings.append(rect_cells(34, 4, 52, 16) | rect_cells(44, 12, 58, 24))
    buildings.append(rect_cells(34, 18, 42, 26))

    outer = rect_cells(4, 32, 27, 56)
    inner = rect_cells(10, 38, 21, 50)
    buildings.append(region_minus(outer, inner))
    buildings.append(rect_cells(12, 41, 19, 48))

    buildings.append(rect_cells(34, 34, 50, 50) | rect_cells(48, 42, 58, 54))
    buildings.append(rect_cells(34, 54, 41, 60))
    buildings.append(rect_cells(44, 54, 47, 60))
    buildings.append(rect_cells(54, 55, 60, 61) | rect_cells(55, 57, 60, 61) | rect_cells(56, 55, 59, 58))

    return buildings


def validate_buildings(buildings: list[set[tuple[int, int]]]) -> None:
    all_c: set[tuple[int, int]] = set()
    for i, b in enumerate(buildings):
        if b & all_c:
            raise SystemExit(f"buildings overlap at step {i}")
        for x, y in b:
            if on_road_spine(x, y):
                raise SystemExit(f"building {i} uses spine cell ({x},{y})")
            if not (MARGIN <= x < SZ - MARGIN and MARGIN <= y < SZ - MARGIN):
                raise SystemExit(f"building {i} out of inner bounds at ({x},{y})")
        all_c |= b


def render_world_map() -> str:
    buildings = collect_buildings()
    validate_buildings(buildings)

    grid = init_int_grid()
    for b in buildings:
        masks = stamp_region_masks(b)
        apply_masks_to_grid(grid, masks)
        add_exterior_door_for_building(grid, b)

    apply_spine(grid)
    return "\n".join(grid_to_lines(grid)) + "\n"


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--output",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "world_map.txt",
        help="Destination tilemap (default: repo root world_map.txt)",
    )
    args = p.parse_args()
    text = render_world_map()
    args.output.write_text(text)
    print(f"Wrote {args.output} ({SZ}×{SZ} cells)")


if __name__ == "__main__":
    main()

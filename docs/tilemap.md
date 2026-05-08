# Tilemap Format

`world_map.txt` is parsed at game startup into a `WorldMapFloor`.

## Encoding

- Each map cell is exactly **2 characters**.
- A row is a sequence of 2-character tokens.
- All non-empty rows must have the same token count.
- Whitespace inside a line is ignored by the parser.

Supported tokens:

- `..` -> `VOID` (no floor mesh, impassable)
- `__` -> `ROAD` (1x1 floor mesh, passable)
- `wn` -> `WALL(North)` (road floor + north wall strip)
- `ws` -> `WALL(South)` (road floor + south wall strip)
- `we` -> `WALL(East)` (road floor + east wall strip)
- `ww` -> `WALL(West)` (road floor + west wall strip)

## Rendering Rules

- Every non-void cell renders a `1x1` road floor.
- Wall cells additionally render a wall cuboid on one side.
- Wall strip thickness is `1/5` of a cell.
- Void renders nothing so water below remains visible.

## Example

Current top-level map file:

```text
................
..wswswswsws....
..wn__we__wn....
..ws__ww__ws....
..wn__we__wn....
..wswswswsws....
................
```

## Common Parse Errors

- Odd number of characters in a row (cannot split into 2-char tokens).
- Non-rectangular map (row widths differ).
- Unknown token (not in the supported set above).

# Diorama Engine Development TODO

## Completed (R2 - Fog) - [fdcbea1](../commits/diagram-horizon-build.md)
- Terraced clouds over unexplored ground
- Ground shadow and halo
- Nether fog
- Cave dark bank
- Fog blur and sampling

## Tile Format v2 (R3) - [in progress]
- `store.rs`: DTL2 magic, flags + compressed light/spots planes
- `store.rs`: read/write_file updated for new format
- `store.rs`: pending Arc<TileImage> storage

## Right-click Menu (U1) - [in progress]
- `menu.rs`: Target, Action, MenuItem, Menu types
- `menu.rs`: make_menu, teleport_command helpers
- `menu.rs`: needs integration with runtime.rs for screen hooks

## Subsequent Features (per original plan)
1. **Settings/keys** (Wireframe mode, Bloom toggle, longpress durations)
2. **Minimap styling** (Classic/None/white frames)
3. **Compass** (N/E/S/W letters with rotation)
4. **Player arrow colour** (8 accent settings)
5. **Waypoint pin face** (headstone for deaths, caret for off-view)
6. **Zoom snap** (buttons 7, 8 per-dimension zoom)
7. **Area-weighted mip sampling** (shimmer prevention)
8. **Waypoint UI** (labels when zoomed out)
9. **Dimension selector** (per-dimension keys)
10. **World border** (underground world map blocks)
11. **Radar** (mob entities on map)
12. **Block looks** (hopper, ghost block, resources)
13. **Skylight decay** (caves under roofs)

## Glue Integration Required
- [runtime.rs](../crates/horizon-client/src/game.rs): settings expansion for buttons 16-21
- [horizon-mixin.toml](../horizon-mixin.toml): hook injections for each feature
- Type-check via horizon-mixins apply in granted directory
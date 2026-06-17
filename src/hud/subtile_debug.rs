//! Selected-bot subtile passability debug view.
//!
//! When a bot is selected **and** the "Subtile map" debug toggle (inspector
//! Debug tab) is on, this draws a small pixelated HUD image of the combined
//! passability grid for the **3×3 tiles** centered on the selected bot's tile —
//! `3 × SUBTILE_COUNT = 15` texels per axis, one texel per subtile. It is the
//! localized, bot-following counterpart to the full-chunk occupancy overlay
//! (F4): instead of painting the whole world it shows exactly what the collision
//! grid looks like immediately around one bot, so movement / collision behavior
//! can be read at a glance.
//!
//! Static geometry (walls, void, corners) and dynamic creature footprints live
//! in **separate** maps — `HypermapRuntime.static_subtile_cache` and the
//! [`DynamicPassabilityMap`] read buffer respectively — so this ORs both. Reading
//! only the dynamic map would show bot bodies but never walls.
//!
//! ## Colors
//!
//! | Subtile | Color |
//! |---|---|
//! | Selected bot's own center subtile | Green |
//! | `FLAG_CREATURE` (a bot body) | Red |
//! | `FLAG_BLOCKED` without creature (static wall) | Orange |
//! | `FLAG_VOID` (no floor) | Blue |
//! | passable | Dark gray |
//!
//! The image uses nearest-neighbor sampling so the 15×15 texture reads as sharp
//! pixels when scaled up in the UI. It is written every frame while visible (225
//! texels — trivially cheap) and hidden whenever the feature is off or no bot is
//! selected.
//!
//! On top of the pixel grid, a white **ring at display resolution** marks the
//! bot's *exact* float circle (`ActorState::center` and `radius_subtiles`), so
//! the continuous position is visible against the discretized texels the grid
//! quantizes it to. It is a UI `Node` (border + 50% radius), not a texel.

use bevy::asset::RenderAssetUsages;
use bevy::image::ImageSampler;
use bevy::picking::prelude::Pickable;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::actor::ActorObject;
use crate::hud::actor_inspector::SelectedActor;
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::passability::{
    DynamicPassabilityMap, FLAG_BLOCKED, FLAG_CREATURE, FLAG_VOID, SUBTILE_COUNT,
};
use crate::menu::main_menu::GameState;
use crate::scene::camera::StrategyCameraRig;

/// Tiles per axis shown around the bot (the bot's tile plus one ring).
const GRID_TILES: usize = 3;
/// Texels per axis: one texel per subtile across the `GRID_TILES × GRID_TILES`
/// region (`3 × 5 = 15`).
const GRID_SUBTILES: usize = GRID_TILES * SUBTILE_COUNT;
/// On-screen size of the pixelated image (square), in logical pixels.
const DISPLAY_PX: f32 = 196.0;

const PANEL_BG: Color = Color::srgba(0.06, 0.07, 0.1, 0.84);
const PANEL_BORDER: Color = Color::srgba(0.55, 0.62, 0.72, 0.40);
const LABEL: Color = Color::srgba(0.85, 0.90, 0.95, 0.88);
const LEGEND: Color = Color::srgba(0.70, 0.75, 0.82, 0.80);

// Texel colors (kept close to the occupancy overlay palette in `chunk_overlay`).
const COLOR_SELF: [u8; 4] = [70, 235, 110, 255];
const COLOR_CREATURE: [u8; 4] = [225, 70, 70, 255];
const COLOR_WALL: [u8; 4] = [220, 140, 55, 255];
const COLOR_VOID: [u8; 4] = [55, 95, 215, 255];
const COLOR_PASSABLE: [u8; 4] = [26, 28, 34, 235];

/// Exact-position float circle drawn over the pixel grid (a UI ring, not a texel).
const CIRCLE_COLOR: Color = Color::srgba(1.0, 1.0, 1.0, 0.95);

/// Toggles the selected-bot subtile passability view. Flipped by the "Subtile
/// map" button in the inspector Debug tab; read by [`update_subtile_debug`].
#[derive(Resource, Default)]
pub struct SubtilePassabilityDebugEnabled(pub bool);

/// Handle to the 15×15 RGBA image painted each frame.
#[derive(Resource)]
struct SubtileDebugImage(Handle<Image>);

/// Marks the docked debug panel root (whole subtree shows/hides with it).
#[derive(Component)]
struct SubtileDebugPanel;

/// The exact-position float circle overlay; its `Node` rect is set each frame.
#[derive(Component)]
struct SubtileDebugCircle;

pub struct SubtilePassabilityDebugPlugin;

impl Plugin for SubtilePassabilityDebugPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SubtilePassabilityDebugEnabled>()
            .add_systems(
                OnEnter(GameState::InGame),
                setup_subtile_debug.after(crate::scene::camera::spawn_camera),
            )
            .add_systems(
                Update,
                update_subtile_debug.run_if(in_state(GameState::InGame)),
            );
    }
}

fn new_debug_image() -> Image {
    let size = GRID_SUBTILES as u32;
    let mut image = Image::new(
        Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        TextureDimension::D2,
        vec![0u8; (size * size * 4) as usize],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    // Nearest sampling keeps each subtile a crisp square when scaled up.
    image.sampler = ImageSampler::nearest();
    image
}

fn setup_subtile_debug(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    camera: Query<Entity, With<StrategyCameraRig>>,
) {
    let Ok(cam) = camera.single() else {
        return;
    };

    let image = images.add(new_debug_image());
    commands.insert_resource(SubtileDebugImage(image.clone()));

    commands
        .spawn((
            Name::new("Subtile passability debug"),
            SubtileDebugPanel,
            UiTargetCamera(cam),
            Pickable::IGNORE,
            Visibility::Hidden,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(14.0),
                top: Val::Px(84.0),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(6.0),
                padding: UiRect::all(Val::Px(8.0)),
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(PANEL_BG),
            BorderColor::all(PANEL_BORDER),
            ZIndex(1450),
        ))
        .with_children(|root| {
            root.spawn((
                Text::new("Subtile passability (3x3)"),
                TextFont::from_font_size(12.0),
                TextColor(LABEL),
            ));
            root.spawn((
                Pickable::IGNORE,
                Node {
                    width: Val::Px(DISPLAY_PX),
                    height: Val::Px(DISPLAY_PX),
                    position_type: PositionType::Relative,
                    overflow: Overflow::clip(),
                    ..default()
                },
            ))
            .with_children(|frame| {
                frame.spawn((
                    Pickable::IGNORE,
                    ImageNode::new(image),
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(0.0),
                        top: Val::Px(0.0),
                        width: Val::Px(DISPLAY_PX),
                        height: Val::Px(DISPLAY_PX),
                        ..default()
                    },
                ));
                // Bot's true float circle, drawn at display resolution on top of
                // the discretized pixel grid. Rect is positioned each frame from
                // `state.center` / `radius_subtiles` in `update_subtile_debug`.
                frame.spawn((
                    SubtileDebugCircle,
                    Pickable::IGNORE,
                    Node {
                        position_type: PositionType::Absolute,
                        border: UiRect::all(Val::Px(2.0)),
                        border_radius: BorderRadius::all(Val::Percent(50.0)),
                        ..default()
                    },
                    BorderColor::all(CIRCLE_COLOR),
                ));
            });
            root.spawn((
                Text::new("white=exact circle  green=cell  red=bot"),
                TextFont::from_font_size(10.0),
                TextColor(LEGEND),
            ));
        });
}

#[inline]
fn flags_to_rgba(flags: u64) -> [u8; 4] {
    if flags & FLAG_CREATURE != 0 {
        COLOR_CREATURE
    } else if flags & FLAG_BLOCKED != 0 {
        COLOR_WALL
    } else if flags & FLAG_VOID != 0 {
        COLOR_VOID
    } else {
        COLOR_PASSABLE
    }
}

/// Repaints the debug image from the passability read buffer around the selected
/// bot and shows/hides the panel. Reads the **read** buffer (last flushed frame),
/// matching the occupancy overlay.
fn update_subtile_debug(
    enabled: Res<SubtilePassabilityDebugEnabled>,
    selection: Res<SelectedActor>,
    dyn_pass: Res<DynamicPassabilityMap>,
    runtime: Res<HypermapRuntime>,
    image_res: Res<SubtileDebugImage>,
    mut images: ResMut<Assets<Image>>,
    actors: Query<&ActorObject>,
    mut panel: Query<&mut Visibility, With<SubtileDebugPanel>>,
    mut circle: Query<&mut Node, With<SubtileDebugCircle>>,
) {
    let Ok(mut vis) = panel.single_mut() else {
        return;
    };

    let target = if enabled.0 {
        selection.entity.and_then(|e| actors.get(e).ok())
    } else {
        None
    };
    let Some(obj) = target else {
        if *vis != Visibility::Hidden {
            *vis = Visibility::Hidden;
        }
        return;
    };

    let state = obj.inner.state();
    let center_tile = state.center_tile_i32();

    let Some(image) = images.get_mut(&image_res.0) else {
        return;
    };
    let Some(data) = image.data.as_mut() else {
        return;
    };

    let res = GRID_SUBTILES;
    let sc = SUBTILE_COUNT;
    let dynamic = dyn_pass.inner();
    let static_cache = &runtime.static_subtile_cache;
    // Top-left texel maps to the (−1, −1) tile's (0, 0) subtile; +y is south
    // (down), matching the top-down camera so the panel reads north-up.
    for ty_off in 0..GRID_TILES {
        for tx_off in 0..GRID_TILES {
            let tile_x = center_tile.x + tx_off as i32 - 1;
            let tile_y = center_tile.y + ty_off as i32 - 1;
            // Static geometry (walls / void / corners) and dynamic creature
            // footprints live in separate maps — OR them so both show.
            let static_tile = static_cache.get(tile_x, tile_y);
            let dynamic_tile = dynamic.get(tile_x, tile_y);
            for sy in 0..sc {
                for sx in 0..sc {
                    let flags = static_tile.flags_at(sy, sx) | dynamic_tile.flags_at(sy, sx);
                    let px = tx_off * sc + sx;
                    let py = ty_off * sc + sy;
                    let idx = (py * res + px) * 4;
                    data[idx..idx + 4].copy_from_slice(&flags_to_rgba(flags));
                }
            }
        }
    }

    // Mark the bot's own center subtile so its position is unambiguous amid the
    // creature-red footprints (its own body included).
    let bot_sub = state
        .last_accepted_center_subtile
        .unwrap_or_else(|| state.center_subtile_i32());
    let origin_x = (center_tile.x - 1) * sc as i32;
    let origin_y = (center_tile.y - 1) * sc as i32;
    let mpx = bot_sub.x - origin_x;
    let mpy = bot_sub.y - origin_y;
    if (0..res as i32).contains(&mpx) && (0..res as i32).contains(&mpy) {
        let idx = (mpy as usize * res + mpx as usize) * 4;
        data[idx..idx + 4].copy_from_slice(&COLOR_SELF);
    }

    // Overlay the bot's exact float circle at display resolution: real `center`
    // and `radius_subtiles` mapped from subtile-space to panel pixels. This shows
    // the continuous position the grid texels above can only approximate.
    if let Ok(mut node) = circle.single_mut() {
        let subtile_px = DISPLAY_PX / GRID_SUBTILES as f32;
        let cx_px = (state.center.x * sc as f32 - origin_x as f32) * subtile_px;
        let cy_px = (state.center.y * sc as f32 - origin_y as f32) * subtile_px;
        let r_px = state.radius_subtiles.max(0) as f32 * subtile_px;
        node.left = Val::Px(cx_px - r_px);
        node.top = Val::Px(cy_px - r_px);
        node.width = Val::Px(2.0 * r_px);
        node.height = Val::Px(2.0 * r_px);
    }

    if *vis != Visibility::Inherited {
        *vis = Visibility::Inherited;
    }
}

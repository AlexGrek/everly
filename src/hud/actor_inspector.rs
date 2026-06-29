//! Actor hover label and click-to-select side panel (mesh picking).
//!
//! Selecting a bot (left-click on its mesh) opens a non-blocking, right-docked
//! properties panel that live-refreshes twice a second. The panel does not
//! capture the rest of the screen: the world stays interactive while it is open.
//! Closing the panel (X / Escape) or selecting a different bot updates the
//! [`SelectedActor`] resource, which also drives the world-space selection
//! overlay (glowing marker + waypoints) in `crate::actor::selection_overlay`.

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::picking::prelude::*;
use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::actor::actor_pick::ActorForceLogs;
use crate::actor::actor_pick::{ActorInspectable, ActorPickMesh};
use crate::actor::black_bot::{BlackBotVisual, BotSpecialization, Breakable};
use crate::actor::brain::Brain;
use crate::actor::charge::Charge;
use crate::actor::dispatch::BotInventory;
use crate::actor::genetics::Genome;
use crate::actor::inspect::{
    debug_rows, display_actor_name, inventory_rows, memory_rows, route_rows, status_rows,
    systems_rows,
};
use crate::actor::ActorObject;
use crate::edit::actor_spawn::{ActorSpawnState, ActorTool};
use crate::hud::subtile_debug::SubtilePassabilityDebugEnabled;
use crate::menu::main_menu::GameState;
use crate::scene::camera::StrategyCameraRig;

/// Bottom HUD + palette dead zone (see `map_edit::HUD_DEAD_ZONE_PX`).
const PLAYFIELD_BOTTOM_MARGIN_PX: f32 = 120.0;

/// Width of the docked properties panel.
const PANEL_WIDTH_PX: f32 = 320.0;

const ACCENT: Color = Color::srgb(0.48, 0.78, 0.96);
const TEXT_BRIGHT: Color = Color::srgba(0.97, 0.98, 1.0, 0.96);
const TEXT_MUTED: Color = Color::srgba(0.72, 0.76, 0.82, 0.88);
const TOOLTIP_BG: Color = Color::srgba(0.07, 0.09, 0.12, 0.9);
const TOOLTIP_BORDER: Color = Color::srgba(0.48, 0.78, 0.96, 0.55);
const CARD_BG: Color = Color::srgba(0.09, 0.11, 0.15, 0.97);
const CARD_BORDER: Color = Color::srgba(0.55, 0.62, 0.72, 0.35);
const ROW_DIVIDER: Color = Color::srgba(0.4, 0.45, 0.52, 0.25);

/// Minimalist tabs are text-only with a bottom accent rule on the active one.
const TAB_UNDERLINE_ACTIVE: Color = ACCENT;
const TAB_UNDERLINE_INACTIVE: Color = Color::NONE;

/// Logical pixels per mouse-wheel line (matches Bevy UI scroll example).
const SCROLL_LINE_HEIGHT: f32 = 21.0;

/// Panel content refresh cadence — the live properties update twice a second.
const PANEL_REFRESH_INTERVAL_S: f32 = 0.5;

/// Which tab is currently active in the actor inspector panel.
#[derive(Resource, Default, PartialEq, Clone, Copy, Debug)]
pub enum InspectorTab {
    #[default]
    Status,
    Systems,
    Route,
    Inventory,
    Memory,
    Debug,
}

#[derive(Resource, Default)]
struct HoveredActor {
    root: Option<Entity>,
    pick_mesh: Option<Entity>,
}

/// The currently selected actor (the one shown in the side panel). `None` means
/// nothing is selected and the panel is closed. Read by the world-space
/// selection overlay (marker + waypoints).
#[derive(Resource, Default)]
pub struct SelectedActor {
    pub entity: Option<Entity>,
    /// Bumped when the panel body (rows/actions) should rebuild while the same
    /// actor stays selected (tab switch, toggle press).
    content_stamp: u32,
}

impl SelectedActor {
    pub fn is_some(&self) -> bool {
        self.entity.is_some()
    }
}

/// `true` while the cursor is hovering the docked panel. The strategy camera
/// reads this to suppress zoom-on-scroll over the panel (the panel scrolls
/// instead), keeping the camera free everywhere else.
#[derive(Resource, Default)]
pub struct InspectorPointerOver(pub bool);

#[derive(Component)]
struct ActorInspectorUiRoot;

#[derive(Component)]
struct ActorHoverTooltip;

#[derive(Component)]
struct ActorHoverTooltipText;

/// The docked properties card.
#[derive(Component)]
struct ActorInspectorPanel;

#[derive(Component)]
struct ActorInspectorCloseButton;

#[derive(Component)]
struct ActorInspectorTitle;

#[derive(Component)]
struct ActorInspectorKindBadge;

#[derive(Component)]
struct ActorInspectorRowsHost;

#[derive(Component)]
struct ActorInspectorRow;

#[derive(Component)]
struct ActorInspectorActionsHost;

#[derive(Component)]
struct ActorInspectorActionBtn;

#[derive(Component)]
struct BlackBotResetButton;

#[derive(Component)]
struct ActorForceLogsToggleButton;

#[derive(Component)]
struct SubtileDebugToggleButton;

#[derive(Component)]
struct ActorDeleteButton;

/// Marker on each tab button; carries which tab it activates.
#[derive(Component, Clone, Copy)]
struct ActorInspectorTabBtn(InspectorTab);

pub struct ActorInspectorPlugin;

impl Plugin for ActorInspectorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HoveredActor>()
            .init_resource::<SelectedActor>()
            .init_resource::<InspectorPointerOver>()
            .init_resource::<InspectorTab>()
            .add_observer(on_actor_pointer_over)
            .add_observer(on_actor_pointer_out)
            .add_observer(on_actor_pointer_click)
            .add_systems(
                OnEnter(GameState::InGame),
                spawn_actor_inspector_ui.after(crate::scene::camera::spawn_camera),
            )
            .add_systems(
                Update,
                (
                    clear_dead_selection,
                    sync_actor_hover_tooltip,
                    sync_inspector_pointer_over,
                    sync_actor_inspector_panel,
                    animate_actor_inspector,
                    actor_inspector_close_input,
                    actor_inspector_wheel_scroll,
                )
                    .run_if(in_state(GameState::InGame)),
            )
            .add_systems(
                Update,
                (
                    actor_inspector_tab_buttons,
                    sync_tab_button_visuals,
                    black_bot_reset_button,
                    actor_force_logs_toggle_button,
                    subtile_debug_toggle_button,
                    actor_delete_button,
                )
                    .run_if(in_state(GameState::InGame)),
            );
    }
}

fn find_actor_root(
    mut entity: Entity,
    child_of: &Query<&ChildOf>,
    inspectable: &Query<(), With<ActorInspectable>>,
) -> Option<Entity> {
    loop {
        if inspectable.get(entity).is_ok() {
            return Some(entity);
        }
        let Ok(child) = child_of.get(entity) else {
            break;
        };
        entity = child.parent();
    }
    None
}

fn on_actor_pointer_over(
    trigger: On<Pointer<Over>>,
    pick_meshes: Query<(), With<ActorPickMesh>>,
    child_of: Query<&ChildOf>,
    inspectable: Query<(), With<ActorInspectable>>,
    mut hovered: ResMut<HoveredActor>,
) {
    if pick_meshes.get(trigger.entity).is_err() {
        return;
    }
    let Some(root) = find_actor_root(trigger.entity, &child_of, &inspectable) else {
        return;
    };
    hovered.root = Some(root);
    hovered.pick_mesh = Some(trigger.entity);
}

fn on_actor_pointer_out(
    trigger: On<Pointer<Out>>,
    pick_meshes: Query<(), With<ActorPickMesh>>,
    mut hovered: ResMut<HoveredActor>,
) {
    if pick_meshes.get(trigger.entity).is_err() {
        return;
    }
    if hovered.pick_mesh == Some(trigger.entity) {
        hovered.root = None;
        hovered.pick_mesh = None;
    }
}

fn on_actor_pointer_click(
    click: On<Pointer<Click>>,
    pick_meshes: Query<(), With<ActorPickMesh>>,
    child_of: Query<&ChildOf>,
    inspectable: Query<(), With<ActorInspectable>>,
    spawn_state: Res<ActorSpawnState>,
    mut commands: Commands,
    mut selection: ResMut<SelectedActor>,
) {
    if click.event().button != PointerButton::Primary {
        return;
    }
    if pick_meshes.get(click.entity).is_err() {
        return;
    }
    let Some(root) = find_actor_root(click.entity, &child_of, &inspectable) else {
        return;
    };
    if matches!(spawn_state.tool, Some(ActorTool::Kill)) {
        if selection.entity == Some(root) {
            selection.entity = None;
        }
        commands.entity(root).despawn();
        return;
    }
    // Selecting a different bot replaces the selection (the old one deselects).
    selection.entity = Some(root);
}

fn spawn_actor_inspector_ui(mut commands: Commands, camera: Query<Entity, With<StrategyCameraRig>>) {
    let Ok(cam) = camera.single() else {
        return;
    };

    commands
        .spawn((
            Name::new("Actor inspector UI"),
            ActorInspectorUiRoot,
            UiTargetCamera(cam),
            Pickable::IGNORE,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
            ZIndex(1500),
        ))
        .with_children(|root| {
            root.spawn((
                Name::new("Actor hover tooltip"),
                ActorHoverTooltip,
                Pickable::IGNORE,
                Visibility::Hidden,
                Node {
                    position_type: PositionType::Absolute,
                    padding: UiRect::axes(Val::Px(10.0), Val::Px(5.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    ..default()
                },
                BackgroundColor(TOOLTIP_BG),
                BorderColor::all(TOOLTIP_BORDER),
            ))
            .with_children(|tip| {
                tip.spawn((
                    ActorHoverTooltipText,
                    Text::new(""),
                    TextFont::from_font_size(14.0),
                    TextColor(TEXT_BRIGHT),
                ));
            });

            // Right-docked, non-blocking properties panel. Pickable so its
            // buttons work and the cursor-over check captures it, but it never
            // covers the rest of the screen, so the world stays interactive.
            root.spawn((
                Name::new("Actor inspector panel"),
                ActorInspectorPanel,
                Pickable::default(),
                Visibility::Hidden,
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(14.0),
                    top: Val::Px(72.0),
                    bottom: Val::Px(132.0),
                    width: Val::Px(PANEL_WIDTH_PX),
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::all(Val::Px(12.0)),
                    row_gap: Val::Px(9.0),
                    border: UiRect::all(Val::Px(1.0)),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor(CARD_BG),
                BorderColor::all(CARD_BORDER),
            ))
            .with_children(|card| {
                // Header: kind badge + title + close button.
                card.spawn(Node {
                    width: Val::Percent(100.0),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::SpaceBetween,
                    align_items: AlignItems::FlexStart,
                    column_gap: Val::Px(12.0),
                    ..default()
                })
                .with_children(|header| {
                    header
                        .spawn(Node {
                            flex_direction: FlexDirection::Column,
                            row_gap: Val::Px(3.0),
                            flex_grow: 1.0,
                            ..default()
                        })
                        .with_children(|titles| {
                            titles.spawn((
                                ActorInspectorKindBadge,
                                Text::new("ACTOR"),
                                TextFont::from_font_size(10.0),
                                TextColor(ACCENT),
                            ));
                            titles.spawn((
                                ActorInspectorTitle,
                                Text::new("-"),
                                TextFont::from_font_size(16.0),
                                TextColor(TEXT_BRIGHT),
                            ));
                        });

                    header
                        .spawn((
                            Name::new("Actor inspector close"),
                            ActorInspectorCloseButton,
                            Pickable::default(),
                            Button,
                            Node {
                                width: Val::Px(24.0),
                                height: Val::Px(24.0),
                                justify_content: JustifyContent::Center,
                                align_items: AlignItems::Center,
                                ..default()
                            },
                            BackgroundColor(Color::srgba(0.16, 0.18, 0.22, 0.6)),
                        ))
                        .with_children(|btn| {
                            btn.spawn((
                                Text::new("X"),
                                TextFont::from_font_size(13.0),
                                TextColor(TEXT_MUTED),
                            ));
                        });
                });

                // Accent divider.
                card.spawn((
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Px(1.0),
                        ..default()
                    },
                    BackgroundColor(ACCENT.with_alpha(0.3)),
                ));

                // Tab bar: Status | Systems | Route | Inventory | Memory | Debug
                card.spawn(Node {
                    width: Val::Percent(100.0),
                    flex_direction: FlexDirection::Row,
                    flex_wrap: FlexWrap::Wrap,
                    column_gap: Val::Px(12.0),
                    row_gap: Val::Px(2.0),
                    ..default()
                })
                .with_children(|tabs| {
                    spawn_tab_button(tabs, "Status", InspectorTab::Status, true);
                    spawn_tab_button(tabs, "Systems", InspectorTab::Systems, false);
                    spawn_tab_button(tabs, "Route", InspectorTab::Route, false);
                    spawn_tab_button(tabs, "Inventory", InspectorTab::Inventory, false);
                    spawn_tab_button(tabs, "Memory", InspectorTab::Memory, false);
                    spawn_tab_button(tabs, "Debug", InspectorTab::Debug, false);
                });

                // Actions host (Reset, Delete — always visible).
                card.spawn((
                    ActorInspectorActionsHost,
                    Node {
                        width: Val::Percent(100.0),
                        flex_direction: FlexDirection::Row,
                        column_gap: Val::Px(6.0),
                        ..default()
                    },
                ));

                // Rows host: content varies by active tab.
                card.spawn((
                    ActorInspectorRowsHost,
                    Node {
                        width: Val::Percent(100.0),
                        flex_direction: FlexDirection::Column,
                        flex_grow: 1.0,
                        flex_shrink: 1.0,
                        min_height: Val::Px(0.0),
                        row_gap: Val::Px(6.0),
                        overflow: Overflow::scroll_y(),
                        ..default()
                    },
                    ScrollPosition(Vec2::ZERO),
                ));
            });
        });
}

fn spawn_tab_button(parent: &mut ChildSpawnerCommands, label: &str, tab: InspectorTab, active: bool) {
    let underline = if active { TAB_UNDERLINE_ACTIVE } else { TAB_UNDERLINE_INACTIVE };
    parent
        .spawn((
            ActorInspectorTabBtn(tab),
            Pickable::default(),
            Button,
            Node {
                height: Val::Px(22.0),
                padding: UiRect::new(Val::Px(2.0), Val::Px(2.0), Val::Px(0.0), Val::Px(3.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::bottom(Val::Px(2.0)),
                ..default()
            },
            BorderColor::all(underline),
            BackgroundColor(Color::NONE),
        ))
        .with_children(|btn| {
            btn.spawn((
                Text::new(label.to_string()),
                TextFont::from_font_size(11.0),
                TextColor(if active { ACCENT } else { TEXT_MUTED }),
            ));
        });
}

/// Drops the selection when the selected actor entity no longer exists (killed
/// via the palette tool, the Delete action, or any other despawn).
fn clear_dead_selection(
    mut selection: ResMut<SelectedActor>,
    actors: Query<(), With<ActorInspectable>>,
) {
    if let Some(e) = selection.entity {
        if actors.get(e).is_err() {
            selection.entity = None;
        }
    }
}

/// Tracks whether the cursor is over the docked panel (used to suppress camera
/// zoom and route mouse-wheel scroll to the panel).
fn sync_inspector_pointer_over(
    selection: Res<SelectedActor>,
    window: Query<&Window>,
    camera: Query<&Camera, With<StrategyCameraRig>>,
    panel: Query<(&ComputedNode, &UiGlobalTransform), With<ActorInspectorPanel>>,
    mut over: ResMut<InspectorPointerOver>,
) {
    let mut result = false;
    if selection.is_some() {
        if let (Ok(window), Ok(camera), Ok((node, tf))) =
            (window.single(), camera.single(), panel.single())
        {
            if let (Some(cursor), Some(viewport)) =
                (window.physical_cursor_position(), camera.physical_viewport_rect())
            {
                result = node.contains_point(*tf, cursor - viewport.min.as_vec2());
            }
        }
    }
    if over.0 != result {
        over.0 = result;
    }
}

fn actor_inspector_wheel_scroll(
    selection: Res<SelectedActor>,
    over: Res<InspectorPointerOver>,
    mut wheel: MessageReader<MouseWheel>,
    mut rows: Query<(&mut ScrollPosition, &Node, &ComputedNode), With<ActorInspectorRowsHost>>,
) {
    if !selection.is_some() || !over.0 {
        return;
    }

    let Ok((mut scroll_position, node, computed)) = rows.single_mut() else {
        return;
    };
    if node.overflow.y != OverflowAxis::Scroll {
        return;
    }

    let max_offset =
        (computed.content_size() - computed.size()) * computed.inverse_scale_factor();
    if max_offset.y <= 0.0 {
        return;
    }

    for ev in wheel.read() {
        let mut delta = -Vec2::new(ev.x, ev.y);
        if ev.unit == MouseScrollUnit::Line {
            delta *= SCROLL_LINE_HEIGHT;
        }
        if delta.y == 0.0 {
            continue;
        }
        scroll_position.y = (scroll_position.y + delta.y).clamp(0.0, max_offset.y);
    }
}

fn actor_inspector_close_input(
    interactions: Query<&Interaction, (With<ActorInspectorCloseButton>, Changed<Interaction>)>,
    keys: Res<ButtonInput<KeyCode>>,
    mut selection: ResMut<SelectedActor>,
) {
    for interaction in &interactions {
        if *interaction == Interaction::Pressed {
            selection.entity = None;
        }
    }
    if keys.just_pressed(KeyCode::Escape) && selection.is_some() {
        selection.entity = None;
    }
}

/// Handles tab button presses; updates active tab and triggers a content rebuild.
fn actor_inspector_tab_buttons(
    interactions: Query<(&Interaction, &ActorInspectorTabBtn), Changed<Interaction>>,
    mut tab: ResMut<InspectorTab>,
    mut selection: ResMut<SelectedActor>,
) {
    for (interaction, btn) in &interactions {
        if *interaction == Interaction::Pressed && *tab != btn.0 {
            *tab = btn.0;
            selection.content_stamp = selection.content_stamp.wrapping_add(1);
        }
    }
}

/// Updates tab button appearance to reflect the active tab.
fn sync_tab_button_visuals(
    tab: Res<InspectorTab>,
    mut tab_buttons: Query<(&ActorInspectorTabBtn, &mut BackgroundColor, &mut BorderColor, &Children)>,
    mut tab_texts: Query<&mut TextColor>,
) {
    if !tab.is_changed() {
        return;
    }
    for (btn, mut bg, mut border, children) in &mut tab_buttons {
        let active = btn.0 == *tab;
        *bg = BackgroundColor(Color::NONE);
        *border = BorderColor::all(if active { TAB_UNDERLINE_ACTIVE } else { TAB_UNDERLINE_INACTIVE });
        for child in children.iter() {
            if let Ok(mut color) = tab_texts.get_mut(child) {
                *color = TextColor(if active { ACCENT } else { TEXT_MUTED });
            }
        }
    }
}

fn sync_actor_hover_tooltip(
    hovered: Res<HoveredActor>,
    window: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform), With<StrategyCameraRig>>,
    pick_meshes: Query<&GlobalTransform, With<ActorPickMesh>>,
    actor_names: Query<&Name, With<ActorInspectable>>,
    mut tooltip: Query<(&mut Visibility, &mut Node), With<ActorHoverTooltip>>,
    mut tooltip_text: Query<&mut Text, With<ActorHoverTooltipText>>,
) {
    let Ok((mut vis, mut node)) = tooltip.single_mut() else {
        return;
    };

    let Some(root) = hovered.root else {
        *vis = Visibility::Hidden;
        return;
    };
    if !cursor_in_playfield(&window) {
        *vis = Visibility::Hidden;
        return;
    }

    let Ok(name) = actor_names.get(root) else {
        *vis = Visibility::Hidden;
        return;
    };

    let pick_entity = hovered.pick_mesh.unwrap_or(root);
    let Ok(world_tf) = pick_meshes.get(pick_entity) else {
        *vis = Visibility::Hidden;
        return;
    };

    let Ok((camera, cam_gt)) = cameras.single() else {
        return;
    };

    let anchor = world_tf.translation() + Vec3::Y * 0.75;
    let Ok(screen) = camera.world_to_viewport(cam_gt, anchor) else {
        *vis = Visibility::Hidden;
        return;
    };

    for mut text in &mut tooltip_text {
        **text = display_actor_name(name.as_str());
    }

    node.left = Val::Px(screen.x - 60.0);
    node.top = Val::Px(screen.y - 44.0);
    *vis = Visibility::Inherited;
}

fn cursor_in_playfield(window: &Query<&Window>) -> bool {
    let Ok(window) = window.single() else {
        return false;
    };
    let Some(pos) = window.cursor_position() else {
        return false;
    };
    pos.y >= PLAYFIELD_BOTTOM_MARGIN_PX
}

const ANIM_DURATION_S: f32 = 0.18;

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Slides the panel in from the right edge when a bot is selected.
fn animate_actor_inspector(
    selection: Res<SelectedActor>,
    time: Res<Time>,
    mut progress: Local<f32>,
    mut was_open: Local<bool>,
    mut panel: Query<&mut Transform, With<ActorInspectorPanel>>,
) {
    let open = selection.is_some();
    if open && !*was_open {
        *progress = 0.0;
    }
    *was_open = open;

    if !open {
        return;
    }

    *progress = (*progress + time.delta_secs() / ANIM_DURATION_S).min(1.0);
    let t = ease_out_cubic(*progress);

    if let Ok(mut tf) = panel.single_mut() {
        tf.translation.x = (PANEL_WIDTH_PX + 28.0) * (1.0 - t);
    }
}

fn spawn_black_bot_reset_button(parent: &mut ChildSpawnerCommands) {
    parent
        .spawn((
            Name::new("BlackBot reset route"),
            ActorInspectorActionBtn,
            BlackBotResetButton,
            Pickable::default(),
            Button,
            Node {
                min_width: Val::Px(56.0),
                height: Val::Px(26.0),
                padding: UiRect::horizontal(Val::Px(10.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(CARD_BORDER),
            BackgroundColor(Color::srgba(0.14, 0.16, 0.22, 0.9)),
        ))
        .with_children(|btn| {
            btn.spawn((
                Text::new("Reset"),
                TextFont::from_font_size(12.0),
                TextColor(TEXT_BRIGHT),
            ));
        });
}

fn spawn_force_logs_toggle_button(parent: &mut ChildSpawnerCommands, enabled: bool) {
    let label = if enabled {
        "Force logs: ON"
    } else {
        "Force logs: OFF"
    };
    parent
        .spawn((
            Name::new("Actor force logs toggle"),
            ActorInspectorActionBtn,
            ActorForceLogsToggleButton,
            Pickable::default(),
            Button,
            Node {
                min_width: Val::Px(110.0),
                height: Val::Px(26.0),
                padding: UiRect::horizontal(Val::Px(12.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(if enabled { ACCENT } else { CARD_BORDER }),
            BackgroundColor(if enabled {
                Color::srgba(0.12, 0.18, 0.28, 0.95)
            } else {
                Color::srgba(0.14, 0.16, 0.22, 0.9)
            }),
        ))
        .with_children(|btn| {
            btn.spawn((
                Text::new(label),
                TextFont::from_font_size(12.0),
                TextColor(if enabled { ACCENT } else { TEXT_BRIGHT }),
            ));
        });
}

fn actor_force_logs_toggle_button(
    interactions: Query<&Interaction, (With<ActorForceLogsToggleButton>, Changed<Interaction>)>,
    mut selection: ResMut<SelectedActor>,
    mut force_logs: Query<&mut ActorForceLogs>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let Some(actor) = selection.entity else {
            continue;
        };
        let Ok(mut flag) = force_logs.get_mut(actor) else {
            continue;
        };
        flag.0 = !flag.0;
        selection.content_stamp = selection.content_stamp.wrapping_add(1);
    }
}

fn spawn_subtile_debug_toggle_button(parent: &mut ChildSpawnerCommands, enabled: bool) {
    let label = if enabled {
        "Subtile map: ON"
    } else {
        "Subtile map: OFF"
    };
    parent
        .spawn((
            Name::new("Subtile passability debug toggle"),
            ActorInspectorActionBtn,
            SubtileDebugToggleButton,
            Pickable::default(),
            Button,
            Node {
                min_width: Val::Px(120.0),
                height: Val::Px(26.0),
                padding: UiRect::horizontal(Val::Px(12.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(if enabled { ACCENT } else { CARD_BORDER }),
            BackgroundColor(if enabled {
                Color::srgba(0.12, 0.18, 0.28, 0.95)
            } else {
                Color::srgba(0.14, 0.16, 0.22, 0.9)
            }),
        ))
        .with_children(|btn| {
            btn.spawn((
                Text::new(label),
                TextFont::from_font_size(12.0),
                TextColor(if enabled { ACCENT } else { TEXT_BRIGHT }),
            ));
        });
}

/// Flips the global [`SubtilePassabilityDebugEnabled`] toggle (drives the
/// bottom-left selected-bot passability panel) and rebuilds the Debug tab so the
/// button label updates.
fn subtile_debug_toggle_button(
    interactions: Query<&Interaction, (With<SubtileDebugToggleButton>, Changed<Interaction>)>,
    mut selection: ResMut<SelectedActor>,
    mut enabled: ResMut<SubtilePassabilityDebugEnabled>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        enabled.0 = !enabled.0;
        selection.content_stamp = selection.content_stamp.wrapping_add(1);
    }
}

fn black_bot_reset_button(
    interactions: Query<&Interaction, (With<BlackBotResetButton>, Changed<Interaction>)>,
    mut selection: ResMut<SelectedActor>,
    mut brains: Query<&mut Brain, With<ActorInspectable>>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let Some(actor) = selection.entity else {
            continue;
        };
        let Ok(mut brain) = brains.get_mut(actor) else {
            continue;
        };
        brain.reset();
        selection.content_stamp = selection.content_stamp.wrapping_add(1);
    }
}

fn spawn_actor_delete_button(parent: &mut ChildSpawnerCommands) {
    const DELETE_BG: Color = Color::srgba(0.22, 0.10, 0.10, 0.9);
    const DELETE_BORDER: Color = Color::srgba(0.75, 0.28, 0.28, 0.6);
    const DELETE_TEXT: Color = Color::srgb(0.95, 0.55, 0.55);

    parent
        .spawn((
            Name::new("Actor delete"),
            ActorInspectorActionBtn,
            ActorDeleteButton,
            Pickable::default(),
            Button,
            Node {
                min_width: Val::Px(56.0),
                height: Val::Px(26.0),
                padding: UiRect::horizontal(Val::Px(10.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(DELETE_BORDER),
            BackgroundColor(DELETE_BG),
        ))
        .with_children(|btn| {
            btn.spawn((
                Text::new("Delete"),
                TextFont::from_font_size(12.0),
                TextColor(DELETE_TEXT),
            ));
        });
}

fn actor_delete_button(
    interactions: Query<&Interaction, (With<ActorDeleteButton>, Changed<Interaction>)>,
    mut commands: Commands,
    mut selection: ResMut<SelectedActor>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let Some(actor) = selection.entity else {
            continue;
        };
        commands.entity(actor).despawn();
        selection.entity = None;
    }
}

/// Persistent state for the panel rebuild logic.
struct PanelBuildState {
    last_stamp: u32,
    last_actor: Option<Entity>,
    refresh: Timer,
}

impl Default for PanelBuildState {
    fn default() -> Self {
        Self {
            last_stamp: 0,
            last_actor: None,
            refresh: Timer::from_seconds(PANEL_REFRESH_INTERVAL_S, TimerMode::Repeating),
        }
    }
}

fn sync_actor_inspector_panel(
    mut commands: Commands,
    time: Res<Time>,
    selection: Res<SelectedActor>,
    tab: Res<InspectorTab>,
    mut panel: Query<&mut Visibility, With<ActorInspectorPanel>>,
    mut title: Query<&mut Text, With<ActorInspectorTitle>>,
    mut badge: Query<&mut Text, (With<ActorInspectorKindBadge>, Without<ActorInspectorTitle>)>,
    actions_host: Query<Entity, With<ActorInspectorActionsHost>>,
    rows_host: Query<Entity, With<ActorInspectorRowsHost>>,
    existing_rows: Query<Entity, With<ActorInspectorRow>>,
    existing_actions: Query<Entity, With<ActorInspectorActionBtn>>,
    actor_data: Query<(&ActorObject, Option<&Name>), With<ActorInspectable>>,
    black: Query<(&Brain, &BlackBotVisual, Option<&BotSpecialization>, Option<&Genome>)>,
    actor_extras: Query<(
        Option<&Charge>,
        Option<&Breakable>,
        Option<&BotInventory>,
        Option<&ActorForceLogs>,
    )>,
    subtile_debug: Res<SubtilePassabilityDebugEnabled>,
    mut state: Local<PanelBuildState>,
) {
    let Ok(mut panel_vis) = panel.single_mut() else {
        return;
    };

    if !selection.is_some() {
        *panel_vis = Visibility::Hidden;
        state.last_stamp = 0;
        state.last_actor = None;
        for row in &existing_rows {
            commands.entity(row).despawn();
        }
        for action in &existing_actions {
            commands.entity(action).despawn();
        }
        return;
    }

    *panel_vis = Visibility::Inherited;

    // Twice-a-second live refresh keeps displayed values current while the same
    // bot stays selected, on top of the explicit change triggers below.
    let ticked = state.refresh.tick(time.delta()).just_finished();
    let content_changed = ticked
        || selection.is_changed()
        || tab.is_changed()
        || state.last_stamp != selection.content_stamp
        || state.last_actor != selection.entity;
    if !content_changed {
        return;
    }
    state.last_stamp = selection.content_stamp;
    state.last_actor = selection.entity;

    let Some(actor) = selection.entity else {
        return;
    };

    let (charge, breakable, inventory, force_logs_on) = actor_extras
        .get(actor)
        .map(|(c, b, i, f)| (c.map(|c| c.level), b, i, f.map(|f| f.0).unwrap_or(false)))
        .unwrap_or((None, None, None, false));
    let is_black_bot = black.get(actor).is_ok();

    let kind_label;
    let rows;
    if let Ok((brain, vis, spec, genome)) = black.get(actor) {
        kind_label = "BlackBot";
        let Ok((obj, _)) = actor_data.get(actor) else {
            return;
        };
        rows = match *tab {
            InspectorTab::Status => status_rows(
                obj,
                charge,
                Some(brain),
                spec.copied(),
                Some(vis.collision_pressure()),
                genome.map(|g| g.traits()),
            ),
            InspectorTab::Systems => breakable.map(|b| systems_rows(b)).unwrap_or_default(),
            InspectorTab::Route => route_rows(brain),
            InspectorTab::Inventory => {
                inventory_rows(inventory.and_then(|inv| inv.carried))
            }
            InspectorTab::Memory => memory_rows(brain.memory()),
            InspectorTab::Debug => debug_rows(force_logs_on),
        };
    } else {
        return;
    };

    let display_name = actor_data
        .get(actor)
        .ok()
        .and_then(|(_, name)| name)
        .map(|n| display_actor_name(n.as_str()))
        .unwrap_or_else(|| "(unnamed)".to_string());

    for mut text in &mut badge {
        **text = kind_label.to_string();
    }
    for mut text in &mut title {
        **text = display_name.clone();
    }

    let Ok(actions_entity) = actions_host.single() else {
        return;
    };
    let Ok(host) = rows_host.single() else {
        return;
    };
    for row in &existing_rows {
        commands.entity(row).despawn();
    }
    for action in &existing_actions {
        commands.entity(action).despawn();
    }

    if is_black_bot {
        commands
            .entity(actions_entity)
            .with_children(spawn_black_bot_reset_button);
    }
    commands
        .entity(actions_entity)
        .with_children(spawn_actor_delete_button);

    if *tab == InspectorTab::Debug {
        let subtile_on = subtile_debug.0;
        commands.entity(host).with_children(|parent| {
            spawn_force_logs_toggle_button(parent, force_logs_on);
            spawn_subtile_debug_toggle_button(parent, subtile_on);
        });
    }

    if rows.is_empty() {
        commands.entity(host).with_children(|parent| {
            parent
                .spawn((
                    ActorInspectorRow,
                    Node { padding: UiRect::vertical(Val::Px(8.0)), ..default() },
                ))
                .with_children(|block| {
                    block.spawn((
                        Text::new("No data for this tab."),
                        TextFont::from_font_size(12.0),
                        TextColor(TEXT_MUTED),
                    ));
                });
        });
        return;
    }

    for (i, row) in rows.iter().enumerate() {
        commands.entity(host).with_children(|parent| {
            parent
                .spawn((
                    ActorInspectorRow,
                    Node {
                        width: Val::Percent(100.0),
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(1.0),
                        padding: UiRect::vertical(Val::Px(3.0)),
                        ..default()
                    },
                ))
                .with_children(|block| {
                    block.spawn((
                        Text::new(row.label),
                        TextFont::from_font_size(10.0),
                        TextColor(TEXT_MUTED),
                    ));
                    block.spawn((
                        Text::new(row.value.clone()),
                        TextFont::from_font_size(13.0),
                        TextColor(TEXT_BRIGHT),
                    ));
                    if i + 1 < rows.len() {
                        block.spawn((
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(1.0),
                                margin: UiRect::top(Val::Px(5.0)),
                                ..default()
                            },
                            BackgroundColor(ROW_DIVIDER),
                        ));
                    }
                });
        });
    }
}

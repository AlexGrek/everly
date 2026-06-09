//! Actor hover label and click-to-inspect modal (mesh picking).

use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::picking::prelude::*;
use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::actor::actor_pick::{ActorInspectable, ActorPickMesh};
use crate::actor::black_bot::{BotSpecialization, Breakable};
use crate::actor::brain::Brain;
use crate::actor::charge::Charge;
use crate::actor::glitch_bot::GlitchBotVisual;
use crate::actor::inspect::{display_actor_name, route_rows, status_rows, systems_rows};
use crate::actor::ActorObject;
use crate::edit::actor_spawn::{ActorSpawnState, ActorTool};
use crate::menu::main_menu::GameState;
use crate::scene::camera::StrategyCameraRig;

/// Bottom HUD + palette dead zone (see `map_edit::HUD_DEAD_ZONE_PX`).
const PLAYFIELD_BOTTOM_MARGIN_PX: f32 = 120.0;

const ACCENT: Color = Color::srgb(0.48, 0.78, 0.96);
const TEXT_BRIGHT: Color = Color::srgba(0.97, 0.98, 1.0, 0.96);
const TEXT_MUTED: Color = Color::srgba(0.72, 0.76, 0.82, 0.88);
const TOOLTIP_BG: Color = Color::srgba(0.07, 0.09, 0.12, 0.9);
const TOOLTIP_BORDER: Color = Color::srgba(0.48, 0.78, 0.96, 0.55);
const SCRIM: Color = Color::srgba(0.02, 0.03, 0.06, 0.68);
const CARD_BG: Color = Color::srgba(0.09, 0.11, 0.15, 0.96);
const CARD_BORDER: Color = Color::srgba(0.55, 0.62, 0.72, 0.35);
const ROW_DIVIDER: Color = Color::srgba(0.4, 0.45, 0.52, 0.25);

const TAB_ACTIVE_BG: Color = Color::srgba(0.48, 0.78, 0.96, 0.15);
const TAB_ACTIVE_BORDER: Color = Color::srgba(0.48, 0.78, 0.96, 0.7);
const TAB_INACTIVE_BG: Color = Color::srgba(0.12, 0.14, 0.18, 0.7);

/// Logical pixels per mouse-wheel line (matches Bevy UI scroll example).
const SCROLL_LINE_HEIGHT: f32 = 21.0;

/// Which tab is currently active in the actor inspector modal.
#[derive(Resource, Default, PartialEq, Clone, Copy, Debug)]
pub enum InspectorTab {
    #[default]
    Status,
    Systems,
    Route,
}

#[derive(Resource, Default)]
struct HoveredActor {
    root: Option<Entity>,
    pick_mesh: Option<Entity>,
}

#[derive(Resource, Default)]
pub struct ActorInspectorModal {
    pub open: bool,
    actor: Option<Entity>,
    /// Bumped when modal body (rows/actions) should rebuild while staying open.
    content_stamp: u32,
}

#[derive(Component)]
struct ActorInspectorUiRoot;

#[derive(Component)]
struct ActorHoverTooltip;

#[derive(Component)]
struct ActorHoverTooltipText;

#[derive(Component)]
struct ActorInspectorOverlay;

#[derive(Component)]
struct ActorInspectorScrim;

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
struct ActorInspectorCard;

#[derive(Component)]
struct ActorInspectorActionsHost;

#[derive(Component)]
struct ActorInspectorActionBtn;

#[derive(Component)]
struct BlackBotResetButton;

#[derive(Component)]
struct ActorDeleteButton;

/// Marker on each tab button; carries which tab it activates.
#[derive(Component, Clone, Copy)]
struct ActorInspectorTabBtn(InspectorTab);

pub struct ActorInspectorPlugin;

impl Plugin for ActorInspectorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HoveredActor>()
            .init_resource::<ActorInspectorModal>()
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
                    sync_actor_hover_tooltip,
                    sync_actor_inspector_modal,
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
    mut modal: ResMut<ActorInspectorModal>,
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
        commands.entity(root).despawn();
        return;
    }
    modal.open = true;
    modal.actor = Some(root);
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
                    border_radius: BorderRadius::all(Val::Px(8.0)),
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

            root.spawn((
                Name::new("Actor inspector overlay"),
                ActorInspectorOverlay,
                Pickable::IGNORE,
                Visibility::Hidden,
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    ..default()
                },
            ))
            .with_children(|overlay| {
                overlay
                    .spawn((
                        Name::new("Actor inspector scrim"),
                        ActorInspectorScrim,
                        Pickable::default(),
                        Button,
                        Node {
                            position_type: PositionType::Absolute,
                            width: Val::Percent(100.0),
                            height: Val::Percent(100.0),
                            ..default()
                        },
                        BackgroundColor(SCRIM),
                        ZIndex(0),
                    ))
                    .observe(close_modal_on_scrim_click);

                overlay
                    .spawn((
                        Name::new("Actor inspector card"),
                        ActorInspectorCard,
                        Pickable::default(),
                        Node {
                            width: Val::Px(520.0),
                            max_height: Val::Percent(80.0),
                            flex_direction: FlexDirection::Column,
                            padding: UiRect::all(Val::Px(20.0)),
                            row_gap: Val::Px(14.0),
                            border: UiRect::all(Val::Px(1.0)),
                            border_radius: BorderRadius::all(Val::Px(12.0)),
                            overflow: Overflow::clip(),
                            ..default()
                        },
                        BackgroundColor(CARD_BG),
                        BorderColor::all(CARD_BORDER),
                        ZIndex(1),
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
                                    row_gap: Val::Px(6.0),
                                    flex_grow: 1.0,
                                    ..default()
                                })
                                .with_children(|titles| {
                                    titles.spawn((
                                        ActorInspectorKindBadge,
                                        Text::new("Actor"),
                                        TextFont::from_font_size(12.0),
                                        TextColor(ACCENT),
                                    ));
                                    titles.spawn((
                                        ActorInspectorTitle,
                                        Text::new("—"),
                                        TextFont::from_font_size(24.0),
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
                                        width: Val::Px(32.0),
                                        height: Val::Px(32.0),
                                        justify_content: JustifyContent::Center,
                                        align_items: AlignItems::Center,
                                        border: UiRect::all(Val::Px(1.0)),
                                        border_radius: BorderRadius::all(Val::Px(8.0)),
                                        ..default()
                                    },
                                    BorderColor::all(CARD_BORDER),
                                    BackgroundColor(Color::srgba(0.14, 0.16, 0.2, 0.8)),
                                ))
                                .with_children(|btn| {
                                    btn.spawn((
                                        Text::new("×"),
                                        TextFont::from_font_size(20.0),
                                        TextColor(TEXT_MUTED),
                                    ));
                                });
                        });

                        // Accent divider.
                        card.spawn((
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(2.0),
                                ..default()
                            },
                            BackgroundColor(ACCENT.with_alpha(0.45)),
                        ));

                        // Tab bar: Status | Systems
                        card.spawn(Node {
                            width: Val::Percent(100.0),
                            flex_direction: FlexDirection::Row,
                            column_gap: Val::Px(6.0),
                            ..default()
                        })
                        .with_children(|tabs| {
                            spawn_tab_button(tabs, "Status", InspectorTab::Status, true);
                            spawn_tab_button(tabs, "Systems", InspectorTab::Systems, false);
                            spawn_tab_button(tabs, "Route", InspectorTab::Route, false);
                        });

                        // Actions host (Reset, Delete — always visible).
                        card.spawn((
                            ActorInspectorActionsHost,
                            Node {
                                width: Val::Percent(100.0),
                                flex_direction: FlexDirection::Row,
                                column_gap: Val::Px(8.0),
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
                                row_gap: Val::Px(8.0),
                                overflow: Overflow::scroll_y(),
                                ..default()
                            },
                            ScrollPosition(Vec2::ZERO),
                        ));
                    });
            });
        });
}

fn spawn_tab_button(parent: &mut ChildSpawnerCommands, label: &str, tab: InspectorTab, active: bool) {
    let bg = if active { TAB_ACTIVE_BG } else { TAB_INACTIVE_BG };
    let border = if active { TAB_ACTIVE_BORDER } else { CARD_BORDER };
    parent
        .spawn((
            ActorInspectorTabBtn(tab),
            Pickable::default(),
            Button,
            Node {
                height: Val::Px(28.0),
                padding: UiRect::horizontal(Val::Px(14.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BorderColor::all(border),
            BackgroundColor(bg),
        ))
        .with_children(|btn| {
            btn.spawn((
                Text::new(label.to_string()),
                TextFont::from_font_size(13.0),
                TextColor(if active { ACCENT } else { TEXT_MUTED }),
            ));
        });
}

fn close_modal_on_scrim_click(
    _click: On<Pointer<Click>>,
    mut modal: ResMut<ActorInspectorModal>,
) {
    modal.open = false;
    modal.actor = None;
}

fn actor_inspector_wheel_scroll(
    modal: Res<ActorInspectorModal>,
    mut wheel: MessageReader<MouseWheel>,
    window: Query<&Window>,
    camera: Query<&Camera, With<StrategyCameraRig>>,
    card: Query<(&ComputedNode, &UiGlobalTransform), With<ActorInspectorCard>>,
    mut rows: Query<(&mut ScrollPosition, &Node, &ComputedNode), With<ActorInspectorRowsHost>>,
) {
    if !modal.open {
        return;
    }

    let Ok(window) = window.single() else {
        return;
    };
    let Some(cursor) = window.physical_cursor_position() else {
        return;
    };

    let Ok(camera) = camera.single() else {
        return;
    };
    let Ok((card_node, card_tf)) = card.single() else {
        return;
    };

    let Some(viewport) = camera.physical_viewport_rect() else {
        return;
    };
    let cursor_in_card = card_node.contains_point(*card_tf, cursor - viewport.min.as_vec2());
    if !cursor_in_card {
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
    mut modal: ResMut<ActorInspectorModal>,
) {
    for interaction in &interactions {
        if *interaction == Interaction::Pressed {
            modal.open = false;
            modal.actor = None;
        }
    }
    if keys.just_pressed(KeyCode::Escape) && modal.open {
        modal.open = false;
        modal.actor = None;
    }
}

/// Handles tab button presses; updates active tab and triggers a content rebuild.
fn actor_inspector_tab_buttons(
    interactions: Query<(&Interaction, &ActorInspectorTabBtn), Changed<Interaction>>,
    mut tab: ResMut<InspectorTab>,
    mut modal: ResMut<ActorInspectorModal>,
) {
    for (interaction, btn) in &interactions {
        if *interaction == Interaction::Pressed && *tab != btn.0 {
            *tab = btn.0;
            modal.content_stamp = modal.content_stamp.wrapping_add(1);
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
        *bg = BackgroundColor(if active { TAB_ACTIVE_BG } else { TAB_INACTIVE_BG });
        *border = BorderColor::all(if active { TAB_ACTIVE_BORDER } else { CARD_BORDER });
        for child in children.iter() {
            if let Ok(mut color) = tab_texts.get_mut(child) {
                *color = TextColor(if active { ACCENT } else { TEXT_MUTED });
            }
        }
    }
}

fn sync_actor_hover_tooltip(
    hovered: Res<HoveredActor>,
    modal: Res<ActorInspectorModal>,
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

    let show = hovered.root.is_some() && !modal.open && cursor_in_playfield(&window);

    let Some(root) = hovered.root else {
        *vis = Visibility::Hidden;
        return;
    };
    if !show {
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

fn animate_actor_inspector(
    modal: Res<ActorInspectorModal>,
    time: Res<Time>,
    mut progress: Local<f32>,
    mut was_open: Local<bool>,
    mut card: Query<&mut Transform, With<ActorInspectorCard>>,
    mut scrim: Query<&mut BackgroundColor, With<ActorInspectorScrim>>,
) {
    if modal.open && !*was_open {
        *progress = 0.0;
    }
    *was_open = modal.open;

    if !modal.open {
        return;
    }

    *progress = (*progress + time.delta_secs() / ANIM_DURATION_S).min(1.0);
    let t = ease_out_cubic(*progress);

    if let Ok(mut tf) = card.single_mut() {
        let s = 0.94 + 0.06 * t;
        tf.scale = Vec3::new(s, s, 1.0);
        tf.translation.y = 10.0 * (1.0 - t);
    }
    if let Ok(mut bg) = scrim.single_mut() {
        bg.0 = SCRIM.with_alpha(SCRIM.alpha() * t);
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
                min_width: Val::Px(72.0),
                height: Val::Px(32.0),
                padding: UiRect::horizontal(Val::Px(14.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BorderColor::all(CARD_BORDER),
            BackgroundColor(Color::srgba(0.14, 0.16, 0.22, 0.9)),
        ))
        .with_children(|btn| {
            btn.spawn((
                Text::new("Reset"),
                TextFont::from_font_size(14.0),
                TextColor(TEXT_BRIGHT),
            ));
        });
}

fn black_bot_reset_button(
    interactions: Query<&Interaction, (With<BlackBotResetButton>, Changed<Interaction>)>,
    mut modal: ResMut<ActorInspectorModal>,
    mut brains: Query<&mut Brain, With<ActorInspectable>>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let Some(actor) = modal.actor else {
            continue;
        };
        let Ok(mut brain) = brains.get_mut(actor) else {
            continue;
        };
        brain.reset();
        modal.content_stamp = modal.content_stamp.wrapping_add(1);
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
                min_width: Val::Px(72.0),
                height: Val::Px(32.0),
                padding: UiRect::horizontal(Val::Px(14.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BorderColor::all(DELETE_BORDER),
            BackgroundColor(DELETE_BG),
        ))
        .with_children(|btn| {
            btn.spawn((
                Text::new("Delete"),
                TextFont::from_font_size(14.0),
                TextColor(DELETE_TEXT),
            ));
        });
}

fn actor_delete_button(
    interactions: Query<&Interaction, (With<ActorDeleteButton>, Changed<Interaction>)>,
    mut commands: Commands,
    mut modal: ResMut<ActorInspectorModal>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let Some(actor) = modal.actor else {
            continue;
        };
        commands.entity(actor).despawn();
        modal.open = false;
        modal.actor = None;
    }
}

/// Persistent state for the inspector rebuild logic.
#[derive(Default)]
struct InspectorBuildState {
    last_stamp: u32,
    last_actor: Option<Entity>,
}

fn sync_actor_inspector_modal(
    mut commands: Commands,
    modal: Res<ActorInspectorModal>,
    tab: Res<InspectorTab>,
    mut overlay: Query<&mut Visibility, With<ActorInspectorOverlay>>,
    mut title: Query<&mut Text, With<ActorInspectorTitle>>,
    mut badge: Query<&mut Text, (With<ActorInspectorKindBadge>, Without<ActorInspectorTitle>)>,
    actions_host: Query<Entity, With<ActorInspectorActionsHost>>,
    rows_host: Query<Entity, With<ActorInspectorRowsHost>>,
    existing_rows: Query<Entity, With<ActorInspectorRow>>,
    existing_actions: Query<Entity, With<ActorInspectorActionBtn>>,
    actor_data: Query<(&ActorObject, Option<&Name>), With<ActorInspectable>>,
    black: Query<(&Brain, Option<&BotSpecialization>)>,
    glitch: Query<&GlitchBotVisual>,
    actor_extras: Query<(Option<&Charge>, Option<&Breakable>)>,
    mut state: Local<InspectorBuildState>,
) {
    let Ok(mut overlay_vis) = overlay.single_mut() else {
        return;
    };

    if !modal.open {
        *overlay_vis = Visibility::Hidden;
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

    *overlay_vis = Visibility::Inherited;

    let content_changed = modal.is_changed()
        || tab.is_changed()
        || state.last_stamp != modal.content_stamp
        || state.last_actor != modal.actor;
    if !content_changed {
        return;
    }
    state.last_stamp = modal.content_stamp;
    state.last_actor = modal.actor;

    let Some(actor) = modal.actor else {
        return;
    };

    let (charge, breakable) = actor_extras
        .get(actor)
        .map(|(c, b)| (c.map(|c| c.level), b))
        .unwrap_or((None, None));
    let is_black_bot = black.get(actor).is_ok();

    let kind_label;
    let rows;
    if let Ok((brain, spec)) = black.get(actor) {
        kind_label = "BlackBot";
        let Ok((obj, _)) = actor_data.get(actor) else { return };
        rows = match *tab {
            InspectorTab::Status => status_rows(obj, charge, Some(brain), None, spec.copied()),
            InspectorTab::Systems => breakable.map(|b| systems_rows(b)).unwrap_or_default(),
            InspectorTab::Route => route_rows(brain),
        };
    } else if let Ok(vis) = glitch.get(actor) {
        kind_label = "GlitchBot";
        let Ok((obj, _)) = actor_data.get(actor) else { return };
        rows = match *tab {
            InspectorTab::Status => status_rows(obj, charge, None, Some(vis), None),
            InspectorTab::Systems | InspectorTab::Route => Vec::new(),
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

    let Ok(actions_entity) = actions_host.single() else { return };
    let Ok(host) = rows_host.single() else { return };
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

    if rows.is_empty() {
        commands.entity(host).with_children(|parent| {
            parent.spawn((
                ActorInspectorRow,
                Node { padding: UiRect::vertical(Val::Px(8.0)), ..default() },
            ))
            .with_children(|block| {
                block.spawn((
                    Text::new("No data for this tab."),
                    TextFont::from_font_size(14.0),
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
                        row_gap: Val::Px(2.0),
                        padding: UiRect::vertical(Val::Px(4.0)),
                        ..default()
                    },
                ))
                .with_children(|block| {
                    block.spawn((
                        Text::new(row.label),
                        TextFont::from_font_size(12.0),
                        TextColor(TEXT_MUTED),
                    ));
                    block.spawn((
                        Text::new(row.value.clone()),
                        TextFont::from_font_size(15.0),
                        TextColor(TEXT_BRIGHT),
                    ));
                    if i + 1 < rows.len() {
                        block.spawn((
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(1.0),
                                margin: UiRect::top(Val::Px(6.0)),
                                ..default()
                            },
                            BackgroundColor(ROW_DIVIDER),
                        ));
                    }
                });
        });
    }
}

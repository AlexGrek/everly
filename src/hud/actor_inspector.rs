//! Actor hover label and click-to-inspect modal (mesh picking).

use bevy::picking::prelude::*;
use bevy::prelude::*;
use bevy::ui::widget::Button;

use crate::actor::actor_pick::{ActorInspectable, ActorPickMesh};
use crate::actor::black_bot::BlackBotVisual;
use crate::actor::charge::Charge;
use crate::actor::glitch_bot::GlitchBotVisual;
use crate::map::hypermap_world::HypermapRuntime;
use crate::actor::inspect::{collect_inspect_rows, display_actor_name};
use crate::actor::ActorObject;
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

#[derive(Resource, Default)]
struct HoveredActor {
    root: Option<Entity>,
    pick_mesh: Option<Entity>,
}

#[derive(Resource, Default)]
struct ActorInspectorModal {
    open: bool,
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

pub struct ActorInspectorPlugin;

impl Plugin for ActorInspectorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HoveredActor>()
            .init_resource::<ActorInspectorModal>()
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
            // Full-screen layout container; must not block mesh picking (floor select, actors).
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
                            width: Val::Px(400.0),
                            max_height: Val::Percent(72.0),
                            flex_direction: FlexDirection::Column,
                            padding: UiRect::all(Val::Px(20.0)),
                            row_gap: Val::Px(14.0),
                            border: UiRect::all(Val::Px(1.0)),
                            border_radius: BorderRadius::all(Val::Px(12.0)),
                            ..default()
                        },
                        BackgroundColor(CARD_BG),
                        BorderColor::all(CARD_BORDER),
                        ZIndex(1),
                    ))
                    .with_children(|card| {
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

                        card.spawn((
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(2.0),
                                ..default()
                            },
                            BackgroundColor(ACCENT.with_alpha(0.45)),
                        ));

                        card.spawn((
                            ActorInspectorActionsHost,
                            Node {
                                width: Val::Percent(100.0),
                                flex_direction: FlexDirection::Row,
                                column_gap: Val::Px(8.0),
                                ..default()
                            },
                        ));

                        card.spawn((
                            ActorInspectorRowsHost,
                            Node {
                                width: Val::Percent(100.0),
                                flex_direction: FlexDirection::Column,
                                row_gap: Val::Px(8.0),
                                overflow: Overflow::scroll_y(),
                                ..default()
                            },
                        ));
                    });
            });
        });
}

fn close_modal_on_scrim_click(
    _click: On<Pointer<Click>>,
    mut modal: ResMut<ActorInspectorModal>,
) {
    modal.open = false;
    modal.actor = None;
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
    // `world_to_viewport` already returns top-left viewport pixels (same as UI `top`).

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
    hypermap: Res<HypermapRuntime>,
    mut actors: Query<(&mut ActorObject, &mut BlackBotVisual), With<ActorInspectable>>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let Some(actor) = modal.actor else {
            continue;
        };
        let Ok((mut obj, mut vis)) = actors.get_mut(actor) else {
            continue;
        };
        vis.reset_route(
            obj.inner.state_mut(),
            &hypermap.static_passability_map,
        );
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

fn sync_actor_inspector_modal(
    mut commands: Commands,
    modal: Res<ActorInspectorModal>,
    mut overlay: Query<&mut Visibility, With<ActorInspectorOverlay>>,
    mut title: Query<&mut Text, With<ActorInspectorTitle>>,
    mut badge: Query<&mut Text, (With<ActorInspectorKindBadge>, Without<ActorInspectorTitle>)>,
    actions_host: Query<Entity, With<ActorInspectorActionsHost>>,
    rows_host: Query<Entity, With<ActorInspectorRowsHost>>,
    existing_rows: Query<Entity, With<ActorInspectorRow>>,
    existing_actions: Query<Entity, With<ActorInspectorActionBtn>>,
    actor_names: Query<&Name, With<ActorInspectable>>,
    actors: Query<&ActorObject, With<ActorInspectable>>,
    black: Query<&BlackBotVisual>,
    glitch: Query<&GlitchBotVisual>,
    charges: Query<&Charge>,
    mut last_content_stamp: Local<u32>,
) {
    let Ok(mut overlay_vis) = overlay.single_mut() else {
        return;
    };

    if !modal.open {
        *overlay_vis = Visibility::Hidden;
        *last_content_stamp = 0;
        for row in &existing_rows {
            commands.entity(row).despawn();
        }
        for action in &existing_actions {
            commands.entity(action).despawn();
        }
        return;
    }

    *overlay_vis = Visibility::Inherited;

    if !modal.is_changed() && *last_content_stamp == modal.content_stamp {
        return;
    }
    *last_content_stamp = modal.content_stamp;

    let Some(actor) = modal.actor else {
        return;
    };

    let kind_label;
    let rows;
    let charge = charges.get(actor).ok().map(|c| c.level);
    let is_black_bot = black.get(actor).is_ok();
    if let Ok(vis) = black.get(actor) {
        kind_label = "BlackBot";
        let Ok(obj) = actors.get(actor) else {
            return;
        };
        rows = collect_inspect_rows(obj, charge, Some(vis), None);
    } else if let Ok(vis) = glitch.get(actor) {
        kind_label = "GlitchBot";
        let Ok(obj) = actors.get(actor) else {
            return;
        };
        rows = collect_inspect_rows(obj, charge, None, Some(vis));
    } else {
        return;
    };

    let display_name = actor_names
        .get(actor)
        .map(|n| display_actor_name(n.as_str()))
        .unwrap_or_else(|_| "(unnamed)".to_string());

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

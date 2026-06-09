//! Human-readable actor state for the HUD inspector modal.

use bevy::math::IVec2;

use crate::actor::black_bot::{BotSpecialization, Breakable, BreakablePartState};
use crate::actor::brain::Brain;
use crate::actor::glitch_bot::GlitchBotVisual;
use crate::actor::{actor_main_tile, ActorObject};

/// One label/value row in the inspector modal.
#[derive(Clone)]
pub struct InspectRow {
    pub label: &'static str,
    pub value: String,
}

/// Rows shared by every actor type.
pub fn common_actor_rows(state: &crate::actor::ActorState) -> Vec<InspectRow> {
    let center = format!("({:.3}, {:.3})", state.center.x, state.center.y);
    let last_sub = match state.last_accepted_center_subtile {
        Some(s) => format!("({}, {})", s.x, s.y),
        None => "-".to_string(),
    };
    vec![
        InspectRow { label: "center", value: center },
        InspectRow { label: "last_accepted_center_subtile", value: last_sub },
    ]
}

/// Battery charge row, shown as a whole-percent value.
pub fn charge_row(level: f32) -> InspectRow {
    let pct = (level * 100.0).round() as i32;
    let value = if level <= 0.0 {
        format!("{pct}% (depleted)")
    } else {
        format!("{pct}%")
    };
    InspectRow { label: "charge", value }
}

pub fn black_bot_rows(
    brain: &Brain,
    main_tile: IVec2,
    spec: Option<BotSpecialization>,
    collision_pressure: Option<u32>,
) -> Vec<InspectRow> {
    let priority = brain
        .current_priority()
        .map(|p| format!("{:?} ({:.0})", p.kind, p.value))
        .unwrap_or_else(|| "-".to_string());
    let specialization = spec.map(|s| s.label()).unwrap_or("-");
    vec![
        InspectRow { label: "specialization", value: specialization.to_string() },
        InspectRow { label: "main_tile", value: format!("({}, {})", main_tile.x, main_tile.y) },
        InspectRow { label: "stuck", value: brain.is_stuck().to_string() },
        InspectRow { label: "priority", value: priority },
        InspectRow { label: "high_level", value: brain.high_level_label() },
        InspectRow { label: "low_level", value: brain.low_level_label() },
        InspectRow { label: "has_target", value: brain.has_target().to_string() },
        InspectRow {
            label: "collision_pressure",
            value: collision_pressure
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string()),
        },
    ]
}

pub fn glitch_bot_rows(vis: &GlitchBotVisual) -> Vec<InspectRow> {
    vec![
        InspectRow {
            label: "direction",
            value: format!("({:.3}, {:.3})", vis.direction().x, vis.direction().y),
        },
        InspectRow {
            label: "collision_streak",
            value: vis.collision_streak().to_string(),
        },
    ]
}

fn format_part(p: &BreakablePartState) -> String {
    if p.broken {
        format!("{:.3} (BROKEN)", p.wear)
    } else {
        format!("{:.3}", p.wear)
    }
}

/// Status-tab rows: position, charge, and actor-specific movement info.
pub fn status_rows(
    obj: &ActorObject,
    charge: Option<f32>,
    black: Option<&Brain>,
    glitch: Option<&GlitchBotVisual>,
    spec: Option<BotSpecialization>,
    collision_pressure: Option<u32>,
) -> Vec<InspectRow> {
    let mut rows = common_actor_rows(obj.inner.state());
    if let Some(level) = charge {
        rows.push(charge_row(level));
    }
    if let Some(brain) = black {
        let main_tile = actor_main_tile(obj.inner.state().center);
        rows.extend(black_bot_rows(brain, main_tile, spec, collision_pressure));
    }
    if let Some(vis) = glitch {
        rows.extend(glitch_bot_rows(vis));
    }
    rows
}

/// Route-tab rows: pathfinding state for BlackBot.
pub fn route_rows(brain: &Brain) -> Vec<InspectRow> {
    let target = brain
        .target_tile()
        .map(|(x, y)| format!("({x}, {y})"))
        .unwrap_or_else(|| "-".to_string());
    let vel = brain.velocity();
    vec![
        InspectRow { label: "target", value: target },
        InspectRow { label: "waypoints_left", value: brain.remaining_waypoints().to_string() },
        InspectRow { label: "velocity", value: format!("({:.3}, {:.3})", vel.x, vel.y) },
        InspectRow { label: "stuck_timer", value: format!("{:.2}s", brain.stuck_timer()) },
    ]
}

/// Debug-tab rows: per-actor tooling flags.
pub fn debug_rows(force_logs: bool) -> Vec<InspectRow> {
    vec![InspectRow {
        label: "force_logs",
        value: force_logs.to_string(),
    }]
}

/// Systems-tab rows: wear and breakage status for each sub-component.
pub fn systems_rows(b: &Breakable) -> Vec<InspectRow> {
    vec![
        InspectRow { label: "MOVEMENT_ENGINE", value: format_part(&b.movement_engine) },
        InspectRow { label: "CONTROL_PLANE", value: format_part(&b.control_plane) },
        InspectRow { label: "SENSORY_SYSTEM", value: format_part(&b.sensory_system) },
    ]
}

pub fn display_actor_name(name: &str) -> String {
    if name.is_empty() {
        "(unnamed)".to_string()
    } else {
        name.to_string()
    }
}

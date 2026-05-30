//! Human-readable actor state for the HUD inspector modal.

use crate::actor::black_bot::{Breakable, BreakablePartState, BlackBotVisual};
use crate::actor::glitch_bot::GlitchBotVisual;
use crate::actor::ActorObject;

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
        None => "—".to_string(),
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

pub fn black_bot_rows(vis: &BlackBotVisual) -> Vec<InspectRow> {
    let main_tile = vis
        .main_tile()
        .map(|t| format!("({}, {})", t.x, t.y))
        .unwrap_or_else(|| "—".to_string());
    vec![
        InspectRow { label: "main_tile", value: main_tile },
        InspectRow { label: "has_target", value: vis.has_target().to_string() },
        InspectRow { label: "movement_state", value: vis.movement_state_label() },
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
    black: Option<&BlackBotVisual>,
    glitch: Option<&GlitchBotVisual>,
) -> Vec<InspectRow> {
    let mut rows = common_actor_rows(obj.inner.state());
    if let Some(level) = charge {
        rows.push(charge_row(level));
    }
    if let Some(vis) = black {
        rows.extend(black_bot_rows(vis));
    }
    if let Some(vis) = glitch {
        rows.extend(glitch_bot_rows(vis));
    }
    rows
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

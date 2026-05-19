//! Human-readable actor state for the HUD inspector modal.

use crate::actor::black_bot::BlackBotVisual;
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
        InspectRow {
            label: "center",
            value: center,
        },
        InspectRow {
            label: "last_accepted_center_subtile",
            value: last_sub,
        },
    ]
}

pub fn black_bot_rows(vis: &BlackBotVisual) -> Vec<InspectRow> {
    let main_tile = vis
        .main_tile()
        .map(|t| format!("({}, {})", t.x, t.y))
        .unwrap_or_else(|| "—".to_string());
    vec![
        InspectRow {
            label: "main_tile",
            value: main_tile,
        },
        InspectRow {
            label: "has_target",
            value: vis.has_target().to_string(),
        },
        InspectRow {
            label: "movement_state",
            value: vis.movement_state_label(),
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

pub fn collect_inspect_rows(
    obj: &ActorObject,
    black: Option<&BlackBotVisual>,
    glitch: Option<&GlitchBotVisual>,
) -> Vec<InspectRow> {
    let mut rows = common_actor_rows(obj.inner.state());
    if let Some(vis) = black {
        rows.extend(black_bot_rows(vis));
    }
    if let Some(vis) = glitch {
        rows.extend(glitch_bot_rows(vis));
    }
    rows
}

pub fn display_actor_name(name: &str) -> String {
    if name.is_empty() {
        "(unnamed)".to_string()
    } else {
        name.to_string()
    }
}

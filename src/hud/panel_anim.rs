//! Slide-in / slide-out animation for editor palette panels.
//!
//! Attach [`PanelAnim`] to any panel whose `Node::bottom` should animate
//! between a hidden position (tucked below `open_bottom`) and an open
//! position (`open_bottom`). The system drives the CSS `bottom` offset and
//! toggles `Visibility` so nothing is rendered while fully closed.

use bevy::prelude::*;

/// Drives a CSS `bottom` slide animation on a UI panel.
#[derive(Component)]
pub struct PanelAnim {
    /// 0.0 = fully closed (panel's top flush with the HUD top),
    /// 1.0 = fully open (sitting at `open_bottom`).
    pub progress: f32,
    /// Target: 0.0 to close, 1.0 to open.
    pub target: f32,
    /// `Node::bottom` in px when the panel is fully open.
    pub open_bottom: f32,
    /// Panel height in px; the slide travel distance.
    pub panel_height: f32,
}

impl PanelAnim {
    pub fn closed(open_bottom: f32, panel_height: f32) -> Self {
        Self { progress: 0.0, target: 0.0, open_bottom, panel_height }
    }
}

pub struct PanelAnimPlugin;

impl Plugin for PanelAnimPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, animate_editor_panels);
    }
}

fn animate_editor_panels(
    time: Res<Time>,
    mut panels: Query<(&mut PanelAnim, &mut Node, &mut Visibility)>,
) {
    const RATE: f32 = 16.0;
    let factor = 1.0 - (-RATE * time.delta_secs()).exp();
    for (mut anim, mut node, mut vis) in &mut panels {
        if anim.target > 0.0 && *vis == Visibility::Hidden {
            *vis = Visibility::Inherited;
        }
        anim.progress += (anim.target - anim.progress) * factor;
        if (anim.progress - anim.target).abs() < 0.004 {
            anim.progress = anim.target;
        }
        node.bottom = Val::Px(anim.open_bottom - anim.panel_height * (1.0 - anim.progress));
        if anim.target == 0.0 && anim.progress == 0.0 && *vis != Visibility::Hidden {
            *vis = Visibility::Hidden;
        }
    }
}

//! Random display names for editor-spawned actors ([`names`] adjective–noun pairs).

use names::Generator;

/// Picks a new random name (e.g. `rusty-nail`).
pub fn random_actor_name() -> String {
    Generator::default().next().unwrap_or_default()
}

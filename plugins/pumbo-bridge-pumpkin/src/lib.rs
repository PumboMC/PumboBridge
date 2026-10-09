//! PumboBridge for Pumpkin: the server half of PumboProx (docs/pumbo-bridge-spec.md).
//!
//! The plugin connects to the proxy over TCP from a task that runs every tick
//! and never blocks, runs the proxy's commands and queries with the Pumpkin
//! API, reports events and writes the proxy's permission decisions as
//! attachments (unless a local PumboPerms rules the server). The protocol and
//! the permission bookkeeping live in `pumbo-bridge-core`; this crate only
//! calls Pumpkin and the socket.
//!
//! [`SUBSCRIBED`] and [`PUMPING`] are checked natively: the bridge never
//! subscribes to an event that one of its own calls can fire (a nested call
//! into the same plugin hangs Pumpkin, spec §2 G2).

use pumbo_bridge_core::proto::method;

/// Plugin name on Pumpkin (also its data folder).
pub const PLUGIN_NAME: &str = "pumbobridge";

/// Pumpkin events the bridge subscribes to.
/// `player-command-send` asks `has-permission`, which fires only
/// `player-permission-check` (never subscribed here).
pub const SUBSCRIBED: &[&str] =
    &["server-list-ping", "player-join", "player-leave", "player-death", "player-respawn", "player-command-send"];

/// Pumpkin calls of the bridge that can run other plugins' handlers inside
/// them (spec §2 G3), with the events they may fire.
pub const PUMPING: &[(&str, &[&str])] = &[
    // Pairs as in docs/architektura.md ("bez zdarzeń od własnych wywołań").
    ("teleport", &["player-teleport", "player-move"]),
    ("teleport-world", &["player-teleport", "player-change-world", "player-changed-world", "player-move"]),
    ("set-gamemode", &["player-gamemode-change"]),
    ("set-food-level", &["food-level-change"]),
    ("add-effect", &["entity-potion-effect", "entity-regain-health"]),
    ("set-permission-level", &["player-permission-check"]),
    ("open-gui", &["inventory-open"]),
    // `ipc` re-enters through `pump_reentry` (safe); other plugins' handlers
    // never call the bridge back there (PumboPerms calls from its own task).
    ("ipc", &[]),
];

/// Methods this bridge has (`info.caps`). Not here: `kill` and `console`
/// (spec §4.2), `guard-rules` (PumboGuard).
pub const CAPS: &[&str] = &[
    method::TELEPORT,
    method::SET_GAMEMODE,
    method::HEAL,
    method::EFFECT,
    method::FLY,
    method::INV_GET,
    method::INV_SET,
    method::INV_GIVE,
    method::INV_CLEAR,
    method::SHOW_ITEMS,
    method::PERM_SET,
    method::PERMS_EXPORT,
    method::Q_PLAYER,
    method::Q_SERVER,
    method::Q_SPAWN,
    method::Q_ENTITIES,
];

/// `minecraft:speed` → `speed`.
pub fn bare(id: &str) -> &str {
    id.strip_prefix("minecraft:").unwrap_or(id)
}

/// `stone` → `minecraft:stone`.
pub fn namespaced(id: &str) -> String {
    if id.contains(':') { id.to_string() } else { format!("minecraft:{id}") }
}

/// The UUID of an `ops.json` entry with this player's name but another UUID,
/// when none has the player's own. Pumpkin matches operators by UUID only,
/// and behind a proxy the UUID follows the proxy's online mode, so such an
/// entry silently does not count.
pub fn stale_op<U: PartialEq>(name: &str, id: &U, ops: Vec<(String, U)>) -> Option<U> {
    if ops.iter().any(|(_, u)| u == id) {
        return None;
    }
    ops.into_iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, u)| u)
}

#[cfg(all(target_arch = "wasm32", feature = "mc263", feature = "mc262"))]
compile_error!("enable only one of the features `mc263` and `mc262`");
#[cfg(all(target_arch = "wasm32", not(any(feature = "mc263", feature = "mc262"))))]
compile_error!("enable one of the features `mc263` (Pumpkin 0.2.0) or `mc262` (Pumpkin 0.1.0-dev)");

#[cfg(all(target_arch = "wasm32", feature = "mc262", not(feature = "mc263")))]
extern crate api262 as papi;
#[cfg(all(target_arch = "wasm32", feature = "mc263", not(feature = "mc262")))]
extern crate api263 as papi;

#[cfg(target_arch = "wasm32")]
mod plugin;

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §3.6: no event fired by one of our own pumping calls is
    /// subscribed (that nesting hangs Pumpkin).
    #[test]
    fn no_subscription_to_events_of_our_own_calls() {
        for (call, fired) in PUMPING {
            for e in *fired {
                assert!(!SUBSCRIBED.contains(e), "{call} fires {e}, which the bridge subscribes to");
            }
        }
        for never in
            ["player-move", "player-teleport", "player-change-world", "inventory-open", "player-permission-check"]
        {
            assert!(!SUBSCRIBED.contains(&never), "{never}");
        }
        assert!(!CAPS.contains(&"kill") && !CAPS.contains(&"console") && !CAPS.contains(&method::GUARD_RULES));
        assert_eq!((bare("minecraft:speed"), namespaced("stone").as_str()), ("speed", "minecraft:stone"));
    }

    #[test]
    fn an_operator_entry_under_another_uuid_is_named() {
        let ops = |v: &[(&str, u8)]| v.iter().map(|(n, u)| (n.to_string(), *u)).collect::<Vec<_>>();
        assert_eq!(stale_op("Qhash", &1, ops(&[("qhash", 2)])), Some(2));
        assert_eq!(stale_op("Qhash", &1, ops(&[("Qhash", 2), ("Qhash", 1)])), None, "the right entry is there too");
        assert_eq!(stale_op("Qhash", &1, ops(&[("Other", 2)])), None);
        assert_eq!(stale_op("Qhash", &1, ops(&[])), None);
    }
}

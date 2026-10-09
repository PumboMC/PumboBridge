//! Permission attachments from the proxy (spec §5.2) and the precedence of a
//! local PumboPerms (§5.3).
//!
//! Attachments are one map per player shared by every plugin, so only one
//! plugin may write them. A local PumboPerms wins: when it answers `hello`
//! or asks `release-permissions`, the bridge takes back what it set and
//! stops writing (mode `local`); when it is gone, the proxy sends fresh sets
//! (mode `bridge`). PumboPerms imports the proxy table through
//! `export-permissions`.
//!
//! When the proxy's PumboPerms rules the network (its export says
//! `proxy-rules`), a local PumboPerms steps back: it answers `hello` with
//! `passive` and hands its attachments over (`take-permissions`); the bridge
//! writes again and clears those, for players offline now at their join
//! (PumboPerms spec §16).

use std::collections::{BTreeMap, HashMap};

use pumbo_bridge_proto::api::{PermSet, PermsExport, PermsMode};
use pumbo_bridge_proto::uuid::Uuid;
use pumbo_bridge_proto::wire::perm_diff;
use serde::Deserialize;

/// A `perm-set` waits this long for its player to join.
pub const PENDING_MS: u64 = 30_000;

/// Attachments to change for one player. `refresh`: resend the player's
/// command list afterwards (not on leave or unload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub player: Uuid,
    pub set: Vec<(String, bool)>,
    pub unset: Vec<String>,
    pub refresh: bool,
}

#[derive(Debug, Default)]
pub struct Perms {
    mode: PermsMode,
    applied: HashMap<Uuid, BTreeMap<String, bool>>,
    pending: HashMap<Uuid, (BTreeMap<String, bool>, u64)>,
    export: Option<PermsExport>,
    /// Attachments a local PumboPerms left when it stepped back.
    leftovers: HashMap<Uuid, Vec<String>>,
}

impl Perms {
    pub fn mode(&self) -> PermsMode {
        self.mode
    }

    /// A `perm-set` from the proxy; `online`: the player is on this server.
    pub fn perm_set(&mut self, s: PermSet, online: bool, now_ms: u64) -> Option<Change> {
        if self.mode == PermsMode::Local {
            return None;
        }
        if !online {
            self.pending.retain(|_, (_, until)| *until > now_ms);
            self.pending.insert(s.player, (s.nodes, now_ms + PENDING_MS));
            return None;
        }
        self.apply(s.player, s.nodes)
    }

    fn apply(&mut self, player: Uuid, nodes: BTreeMap<String, bool>) -> Option<Change> {
        let old = self.applied.get(&player).cloned().unwrap_or_default();
        let (set, unset) = perm_diff(&old, &nodes);
        self.applied.insert(player, nodes);
        (!set.is_empty() || !unset.is_empty()).then_some(Change { player, set, unset, refresh: true })
    }

    /// The player joined: what a local PumboPerms left goes, then the set the
    /// proxy sent before.
    pub fn joined(&mut self, player: Uuid, now_ms: u64) -> Option<Change> {
        let set = match self.pending.remove(&player) {
            Some((nodes, until)) if self.mode == PermsMode::Bridge && until > now_ms => self.apply(player, nodes),
            _ => None,
        };
        let Some(mut left) = self.leftover(player) else { return set };
        if let Some(c) = set {
            left.unset.retain(|n| !c.set.iter().any(|(s, _)| s == n));
            left.unset.extend(c.unset);
            left.set = c.set;
        }
        Some(left)
    }

    /// Unsets what a local PumboPerms left on this player (when it is online).
    pub fn leftover(&mut self, player: Uuid) -> Option<Change> {
        let unset = self.leftovers.remove(&player)?;
        Some(Change { player, set: Vec::new(), unset, refresh: true })
    }

    /// Players with leftovers (to clear the online ones at once).
    pub fn leftover_players(&self) -> Vec<Uuid> {
        self.leftovers.keys().copied().collect()
    }

    /// The player left: everything the bridge set goes.
    pub fn left(&mut self, player: Uuid) -> Option<Change> {
        self.pending.remove(&player);
        let old = self.applied.remove(&player)?;
        Some(Change { player, set: Vec::new(), unset: old.into_keys().collect(), refresh: false })
    }

    /// Everything the bridge set, to take back (unload, or local mode).
    pub fn release_all(&mut self, refresh: bool) -> Vec<Change> {
        self.pending.clear();
        self.applied
            .drain()
            .map(|(player, nodes)| Change { player, set: Vec::new(), unset: nodes.into_keys().collect(), refresh })
            .collect()
    }

    /// Switches the mode; going local takes back every attachment.
    /// Returns the changes and whether the mode changed.
    pub fn set_mode(&mut self, mode: PermsMode) -> (Vec<Change>, bool) {
        if self.mode == mode {
            return (Vec::new(), false);
        }
        self.mode = mode;
        let changes = if mode == PermsMode::Local { self.release_all(true) } else { Vec::new() };
        (changes, true)
    }

    pub fn set_export(&mut self, e: PermsExport) {
        self.export = Some(e);
    }

    pub fn export(&self) -> Option<&PermsExport> {
        self.export.as_ref()
    }

    /// An `ipc` message from another plugin (JSON). Returns the answer and
    /// the attachment changes it causes (release).
    pub fn ipc(&mut self, msg: &[u8], version: &str) -> (Vec<u8>, Vec<Change>) {
        #[derive(Deserialize)]
        struct Req {
            op: String,
        }
        let mut changes = Vec::new();
        let reply = match serde_json::from_slice::<Req>(msg).map(|r| r.op) {
            Ok(op) if op == "hello" => serde_json::json!({
                "ok": true, "plugin": "PumboBridge", "version": version,
                "mode": match self.mode { PermsMode::Bridge => "bridge", PermsMode::Local => "local" },
            }),
            Ok(op) if op == "release-permissions" => {
                changes = self.set_mode(PermsMode::Local).0;
                serde_json::json!({"ok": true})
            }
            Ok(op) if op == "take-permissions" => {
                #[derive(Deserialize)]
                struct Take {
                    #[serde(default)]
                    nodes: BTreeMap<String, Vec<String>>,
                }
                if let Ok(t) = serde_json::from_slice::<Take>(msg) {
                    for (u, nodes) in t.nodes {
                        if let Ok(u) = Uuid::parse_str(&u) {
                            self.leftovers.entry(u).or_default().extend(nodes);
                        }
                    }
                }
                self.set_mode(PermsMode::Bridge);
                serde_json::json!({"ok": true})
            }
            Ok(op) if op == "export-permissions" => match &self.export {
                Some(e) => serde_json::json!({
                    "ok": true,
                    "fingerprint": e.fingerprint,
                    "data": serde_json::from_str::<serde_json::Value>(&e.data).unwrap_or(serde_json::Value::Null),
                }),
                None => serde_json::json!({"ok": false, "pending": true}),
            },
            Ok(op) => serde_json::json!({"ok": false, "error": format!("unknown op {op}")}),
            Err(e) => serde_json::json!({"ok": false, "error": format!("bad request: {e}")}),
        };
        (serde_json::to_vec(&reply).unwrap_or_default(), changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(p: u128, nodes: &[(&str, bool)]) -> PermSet {
        PermSet {
            player: Uuid::from_u128(p),
            nodes: nodes.iter().map(|(n, v)| (n.to_string(), *v)).collect(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn sets_wait_for_the_join_and_apply_differences() {
        let mut p = Perms::default();
        let u = Uuid::from_u128(1);
        assert_eq!(p.perm_set(set(1, &[("a", true)]), false, 0), None, "kept until the join");
        let c = p.joined(u, 1000).unwrap();
        assert_eq!(c.set, vec![("a".to_string(), true)]);
        assert!(c.refresh);
        assert_eq!(p.perm_set(set(1, &[("a", true)]), true, 2000), None, "no change, no refresh");
        let c = p.perm_set(set(1, &[("b", false)]), true, 3000).unwrap();
        assert_eq!((c.set, c.unset), (vec![("b".to_string(), false)], vec!["a".to_string()]));
        let c = p.left(u).unwrap();
        assert_eq!(c.unset, vec!["b".to_string()]);
        assert!(!c.refresh);
        // Too late for the join.
        p.perm_set(set(2, &[("a", true)]), false, 0);
        assert_eq!(p.joined(Uuid::from_u128(2), PENDING_MS + 1), None);
    }

    #[test]
    fn local_pumboperms_takes_over_and_imports() {
        let mut p = Perms::default();
        p.perm_set(set(1, &[("a", true), ("b", true)]), true, 0);
        let (reply, changes) = p.ipc(br#"{"op":"export-permissions"}"#, "0.1.0");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&reply).unwrap()["pending"], true);
        assert!(changes.is_empty());
        p.set_export(PermsExport { fingerprint: "f".into(), data: r#"{"format":"pumboperms"}"#.into() });
        let (reply, _) = p.ipc(br#"{"op":"export-permissions"}"#, "0.1.0");
        let v: serde_json::Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!((v["fingerprint"].as_str(), v["data"]["format"].as_str()), (Some("f"), Some("pumboperms")));
        let (reply, changes) = p.ipc(br#"{"op":"release-permissions"}"#, "0.1.0");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&reply).unwrap()["ok"], true);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].unset.len(), 2);
        assert_eq!(p.mode(), PermsMode::Local);
        assert_eq!(p.perm_set(set(1, &[("c", true)]), true, 1), None, "local: no attachments");
        assert_eq!(p.set_mode(PermsMode::Bridge), (vec![], true));
        assert!(p.perm_set(set(1, &[("c", true)]), true, 2).is_some());
        let (reply, _) = p.ipc(b"{", "0.1.0");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&reply).unwrap()["ok"], false);
    }

    #[test]
    fn local_pumboperms_hands_back_to_the_proxy() {
        let mut p = Perms::default();
        p.ipc(br#"{"op":"release-permissions"}"#, "0.1.0");
        assert_eq!(p.mode(), PermsMode::Local);
        let a = "00000000-0000-0000-0000-000000000001";
        let b = "00000000-0000-0000-0000-000000000002";
        let msg = format!(r#"{{"op":"take-permissions","nodes":{{"{a}":["x","y"],"{b}":["z"]}}}}"#);
        let (reply, changes) = p.ipc(msg.as_bytes(), "0.1.0");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&reply).unwrap()["ok"], true);
        assert!(changes.is_empty());
        assert_eq!(p.mode(), PermsMode::Bridge, "the bridge writes again");
        // An online player: its leftovers go at once.
        let c = p.leftover(Uuid::from_u128(1)).unwrap();
        assert_eq!(c.unset, vec!["x".to_string(), "y".to_string()]);
        assert_eq!(p.leftover(Uuid::from_u128(1)), None);
        // An offline one: at the join, before the proxy's set (which wins where it overlaps).
        p.perm_set(set(2, &[("z", true), ("w", false)]), false, 0);
        let c = p.joined(Uuid::from_u128(2), 1).unwrap();
        assert!(c.unset.is_empty(), "z is set by the proxy anyway: {c:?}");
        assert_eq!(c.set.len(), 2);
        assert!(p.leftover_players().is_empty());
    }
}

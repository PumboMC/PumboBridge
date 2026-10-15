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
//! (PumboPerms spec §16). Only PumboPerms may switch the mode
//! (`release-permissions`, `take-permissions`).
//!
//! `perms-mode` tells PumboPerms whether the proxy rules without the whole
//! export, and whether the export is `live` (received in the current session
//! with the proxy) or a copy from disk or an earlier session. `player-info`
//! hands it what the proxy's provider shows for a player (`perm-set.info`)
//! and the nodes the bridge applied for that player.

use std::collections::{BTreeMap, HashMap};

use pumbo_bridge_proto::api::{PermSet, PermsExport, PermsMode};
use pumbo_bridge_proto::uuid::Uuid;
use pumbo_bridge_proto::wire::perm_diff;
use serde::Deserialize;

/// A `perm-set` waits this long for its player to join.
pub const PENDING_MS: u64 = 30_000;

/// The only plugin that may switch the mode.
pub const PUMBOPERMS: &str = "pumboperms";

/// Whether the answer to `{"op":"hello"}` sent to PumboPerms comes from a
/// running PumboPerms: an answer with `"ok": true`. A trapped plugin answers
/// with an error, which must not keep the bridge out.
pub fn local_perms_alive<A, B>(reply: &Result<Result<Vec<u8>, A>, B>) -> bool {
    matches!(reply, Ok(Ok(b)) if serde_json::from_slice::<serde_json::Value>(b).ok().and_then(|v| v.get("ok")?.as_bool()) == Some(true))
}

/// The proxy's export as the bridge keeps it, also in `perms-export.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct StoredExport {
    #[serde(flatten)]
    pub export: PermsExport,
    /// When it was received (Unix ms; 0 in files of older bridges).
    #[serde(default)]
    pub saved: u64,
}

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
    export: Option<StoredExport>,
    /// The export came in the current session with the proxy.
    live: bool,
    /// The export says that the proxy's PumboPerms rules.
    proxy_rules: bool,
    /// The bridge connects to a proxy (it has a key).
    connected: bool,
    /// `perm-set.info` per player.
    info: HashMap<Uuid, String>,
    /// Attachments a local PumboPerms left when it stepped back.
    leftovers: HashMap<Uuid, Vec<String>>,
}

impl Perms {
    pub fn mode(&self) -> PermsMode {
        self.mode
    }

    /// A `perm-set` from the proxy; `online`: the player is on this server.
    pub fn perm_set(&mut self, s: PermSet, online: bool, now_ms: u64) -> Option<Change> {
        match s.info {
            Some(info) => self.info.insert(s.player, info),
            None => self.info.remove(&s.player),
        };
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
        self.info.remove(&player);
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

    /// The export from the proxy in this session (`saved`: now, Unix ms).
    pub fn set_export(&mut self, export: PermsExport, saved: u64) -> &StoredExport {
        self.keep(StoredExport { export, saved }, true)
    }

    /// The export kept on disk: not live until the proxy sends one.
    pub fn load_export(&mut self, stored: StoredExport) {
        self.keep(stored, false);
    }

    fn keep(&mut self, stored: StoredExport, live: bool) -> &StoredExport {
        // Read the flag alone: an export of a newer format still says it.
        self.proxy_rules = serde_json::from_str::<serde_json::Value>(&stored.export.data)
            .ok()
            .and_then(|v| v.get("proxy-rules")?.as_bool())
            .unwrap_or(false);
        self.live = live;
        self.export.insert(stored)
    }

    /// A new session with the proxy: the export kept so far is from before it.
    pub fn session_started(&mut self) {
        self.live = false;
    }

    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    pub fn export(&self) -> Option<&StoredExport> {
        self.export.as_ref()
    }

    fn mode_name(&self) -> &'static str {
        match self.mode {
            PermsMode::Bridge => "bridge",
            PermsMode::Local => "local",
        }
    }

    /// An `ipc` message from plugin `sender` (JSON). Returns the answer and
    /// the attachment changes it causes (release).
    pub fn ipc(&mut self, sender: &str, msg: &[u8], version: &str) -> (Vec<u8>, Vec<Change>) {
        #[derive(Deserialize)]
        struct Req {
            op: String,
        }
        let mut changes = Vec::new();
        let reply = match serde_json::from_slice::<Req>(msg).map(|r| r.op) {
            Ok(op) if op == "hello" => serde_json::json!({
                "ok": true, "plugin": "PumboBridge", "version": version, "mode": self.mode_name(),
            }),
            Ok(op) if (op == "release-permissions" || op == "take-permissions") && sender != PUMBOPERMS => {
                serde_json::json!({"ok": false, "error": format!("{op} is for {PUMBOPERMS} only")})
            }
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
                    "fingerprint": e.export.fingerprint,
                    "live": self.live,
                    "saved": e.saved,
                    "data": serde_json::from_str::<serde_json::Value>(&e.export.data).unwrap_or(serde_json::Value::Null),
                }),
                None => serde_json::json!({"ok": false, "pending": true}),
            },
            Ok(op) if op == "perms-mode" => serde_json::json!({
                "ok": true,
                "proxy-rules": self.export.is_some() && self.proxy_rules,
                "fingerprint": self.export.as_ref().map(|e| e.export.fingerprint.as_str()).unwrap_or_default(),
                "saved": self.export.as_ref().map_or(0, |e| e.saved),
                "pending": self.export.is_none(),
                "mode": self.mode_name(),
                "live": self.export.is_some() && self.live,
                "connected": self.connected,
            }),
            Ok(op) if op == "player-info" => {
                #[derive(Deserialize)]
                struct Who {
                    uuid: String,
                }
                match serde_json::from_slice::<Who>(msg).ok().and_then(|w| Uuid::parse_str(&w.uuid).ok()) {
                    Some(u) => {
                        serde_json::json!({"ok": true, "data": self.info.get(&u), "nodes": self.applied.get(&u)})
                    }
                    None => serde_json::json!({"ok": false, "error": "bad uuid"}),
                }
            }
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
            info: None,
        }
    }

    const PP: &str = PUMBOPERMS;

    fn json(reply: &[u8]) -> serde_json::Value {
        serde_json::from_slice(reply).unwrap()
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
        let (reply, changes) = p.ipc(PP, br#"{"op":"export-permissions"}"#, "0.1.0");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&reply).unwrap()["pending"], true);
        assert!(changes.is_empty());
        p.set_export(PermsExport { fingerprint: "f".into(), data: r#"{"format":"pumboperms"}"#.into() }, 1);
        let (reply, _) = p.ipc(PP, br#"{"op":"export-permissions"}"#, "0.1.0");
        let v: serde_json::Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!((v["fingerprint"].as_str(), v["data"]["format"].as_str()), (Some("f"), Some("pumboperms")));
        let (reply, changes) = p.ipc(PP, br#"{"op":"release-permissions"}"#, "0.1.0");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&reply).unwrap()["ok"], true);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].unset.len(), 2);
        assert_eq!(p.mode(), PermsMode::Local);
        assert_eq!(p.perm_set(set(1, &[("c", true)]), true, 1), None, "local: no attachments");
        assert_eq!(p.set_mode(PermsMode::Bridge), (vec![], true));
        assert!(p.perm_set(set(1, &[("c", true)]), true, 2).is_some());
        let (reply, _) = p.ipc(PP, b"{", "0.1.0");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&reply).unwrap()["ok"], false);
    }

    #[test]
    fn local_pumboperms_hands_back_to_the_proxy() {
        let mut p = Perms::default();
        p.ipc(PP, br#"{"op":"release-permissions"}"#, "0.1.0");
        assert_eq!(p.mode(), PermsMode::Local);
        let a = "00000000-0000-0000-0000-000000000001";
        let b = "00000000-0000-0000-0000-000000000002";
        let msg = format!(r#"{{"op":"take-permissions","nodes":{{"{a}":["x","y"],"{b}":["z"]}}}}"#);
        let (reply, changes) = p.ipc(PP, msg.as_bytes(), "0.1.0");
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

    #[test]
    fn perms_mode_answers_flag_fingerprint_and_live() {
        let mut p = Perms::default();
        let mode = |p: &mut Perms| json(&p.ipc("someplugin", br#"{"op":"perms-mode"}"#, "0.1.0").0);
        let v = mode(&mut p);
        assert_eq!(
            (v["ok"].clone(), v["pending"].clone(), v["proxy-rules"].clone()),
            (true.into(), true.into(), false.into())
        );
        p.set_connected(true);
        // A newer export format still carries the flag.
        let data = r#"{"format":"pumboperms","version":99,"proxy-rules":true,"groups":"later"}"#;
        p.set_export(PermsExport { fingerprint: "abc".into(), data: data.into() }, 7);
        let v = mode(&mut p);
        assert_eq!(v["proxy-rules"], true);
        assert_eq!((v["fingerprint"].as_str(), v["saved"].as_u64()), (Some("abc"), Some(7)));
        assert_eq!(
            (v["live"].clone(), v["connected"].clone(), v["mode"].clone()),
            (true.into(), true.into(), "bridge".into())
        );
        assert_eq!(json(&p.ipc(PP, br#"{"op":"export-permissions"}"#, "0.1.0").0)["live"], true);
        p.session_started();
        assert_eq!(mode(&mut p)["live"], false, "an export from an earlier session");
        p.set_export(PermsExport { fingerprint: "def".into(), data: r#"{"format":"pumboperms"}"#.into() }, 8);
        let v = mode(&mut p);
        assert_eq!((v["proxy-rules"].clone(), v["live"].clone()), (false.into(), true.into()));
    }

    #[test]
    fn disk_export_is_not_live() {
        let mut p = Perms::default();
        let stored = StoredExport {
            export: PermsExport {
                fingerprint: "f".into(),
                data: r#"{"format":"pumboperms","proxy-rules":true}"#.into(),
            },
            saved: 5,
        };
        // The file keeps the time it was received; files of older bridges have none.
        let text = serde_json::to_string(&stored).unwrap();
        assert_eq!(serde_json::from_str::<StoredExport>(&text).unwrap(), stored);
        let old: StoredExport = serde_json::from_str(r#"{"fingerprint":"f","data":"{}"}"#).unwrap();
        assert_eq!(old.saved, 0);
        p.load_export(stored);
        let v = json(&p.ipc(PP, br#"{"op":"perms-mode"}"#, "0.1.0").0);
        assert_eq!(
            (v["live"].clone(), v["proxy-rules"].clone(), v["saved"].clone()),
            (false.into(), true.into(), 5.into())
        );
        let v = json(&p.ipc(PP, br#"{"op":"export-permissions"}"#, "0.1.0").0);
        assert_eq!((v["ok"].clone(), v["live"].clone()), (true.into(), false.into()));
    }

    #[test]
    fn release_only_from_pumboperms() {
        let mut p = Perms::default();
        p.perm_set(set(1, &[("a", true)]), true, 0);
        let (reply, changes) = p.ipc("evil", br#"{"op":"release-permissions"}"#, "0.1.0");
        assert_eq!(json(&reply)["ok"], false);
        assert!(changes.is_empty());
        assert_eq!(p.mode(), PermsMode::Bridge, "another plugin cannot stop the bridge");
        p.ipc(PP, br#"{"op":"release-permissions"}"#, "0.1.0");
        assert_eq!(p.mode(), PermsMode::Local);
        let a = "00000000-0000-0000-0000-000000000001";
        let msg = format!(r#"{{"op":"take-permissions","nodes":{{"{a}":["x"]}}}}"#);
        assert_eq!(json(&p.ipc("evil", msg.as_bytes(), "0.1.0").0)["ok"], false);
        assert_eq!(p.mode(), PermsMode::Local);
        assert!(p.leftover_players().is_empty(), "nothing taken from another plugin");
    }

    #[test]
    fn local_perms_needs_ok_hello() {
        type Reply = Result<Result<Vec<u8>, String>, ()>;
        let alive = |r: Reply| local_perms_alive(&r);
        assert!(alive(Ok(Ok(br#"{"ok":true,"plugin":"PumboPerms"}"#.to_vec()))));
        assert!(!alive(Ok(Err("wasm trap: unreachable".into()))), "a trapped PumboPerms");
        assert!(!alive(Ok(Ok(br#"{"ok":false,"error":"busy"}"#.to_vec()))));
        assert!(!alive(Ok(Ok(b"not json".to_vec()))));
        assert!(!alive(Err(())), "no PumboPerms");
    }

    #[test]
    fn set_after_expired_pending_applies_on_join() {
        let mut p = Perms::default();
        let u = Uuid::from_u128(1);
        p.perm_set(set(1, &[("a", true)]), false, 0);
        assert_eq!(p.joined(u, PENDING_MS + 1_000), None, "the set waited too long");
        // The proxy sends the set again at the join.
        let c = p.perm_set(set(1, &[("a", true)]), true, PENDING_MS + 1_100).unwrap();
        assert_eq!(c.set, vec![("a".to_string(), true)]);
    }

    #[test]
    fn player_info_comes_with_the_set() {
        let mut p = Perms::default();
        let ask = |p: &mut Perms| {
            let msg = br#"{"op":"player-info","uuid":"00000000-0000-0000-0000-000000000001"}"#;
            json(&p.ipc("anyone", msg, "0.1.0").0)
        };
        assert!(ask(&mut p)["data"].is_null());
        p.perm_set(PermSet { info: Some(r#"{"rank":"vip"}"#.into()), ..set(1, &[]) }, false, 0);
        assert_eq!(ask(&mut p)["data"], r#"{"rank":"vip"}"#);
        assert!(ask(&mut p)["nodes"].is_null(), "no set applied while the player is away");
        p.joined(Uuid::from_u128(1), 1);
        assert_eq!(ask(&mut p)["nodes"], serde_json::json!({}));
        p.perm_set(set(1, &[("a:b", true), ("a:c", false)]), true, 2);
        assert_eq!(ask(&mut p)["nodes"], serde_json::json!({"a:b": true, "a:c": false}));
        p.left(Uuid::from_u128(1));
        assert!(ask(&mut p)["data"].is_null());
        assert!(ask(&mut p)["nodes"].is_null());
        assert_eq!(json(&p.ipc("anyone", br#"{"op":"player-info","uuid":"x"}"#, "0.1.0").0)["ok"], false);
    }
}

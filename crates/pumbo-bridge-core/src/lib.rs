//! PumboBridge core (docs/pumbo-bridge-spec.md): everything of the bridge
//! that does not touch Pumpkin or the socket, so it is tested natively.
//!
//! - [`session::Session`]: the bridge's side of the protocol (handshake with
//!   the proxy proof checked first, frames, pings, pairing nonces, limits).
//! - [`perms::Perms`]: permission attachments from `perm-set`, their
//!   difference with what is set, and the precedence of a local PumboPerms.
//! - [`Backoff`], [`TpsMeter`], [`Changes`]: reconnect delays, the tick rate
//!   and "what changed since last time" for worlds and player stats.
//!
//! The Pumpkin layer (`plugins/pumbo-bridge-pumpkin`) runs one task per tick
//! that pumps the socket and calls into this crate; it never subscribes to an
//! event that its own calls fire (spec §3.6).

pub mod perms;
pub mod session;

use std::collections::HashMap;
use std::hash::Hash;

pub use pumbo_bridge_proto as proto;

/// The bridge's config (`config.yml`, the same on every server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// IPv4 address and port of `bridge.listen` of the proxy.
    pub proxy: ([u8; 4], u16),
    /// The bridge key the proxy prints (`pumbo bridge key`); `None` = idle.
    pub key: Option<proto::wire::Key>,
}

/// The default `config.yml`.
pub const DEFAULT_CONFIG: &str = include_str!("../config.yml");

/// `config.yml` as written (see [`DEFAULT_CONFIG`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Settings {
    pub proxy: String,
    pub key: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { proxy: "127.0.0.1:25578".into(), key: String::new() }
    }
}

impl pumbo_common::config::Settings for Settings {}

/// Reads `config.yml` with the shared YAML loader: the config, or why the
/// bridge stays idle, and the loader's warnings (unknown options, ...).
pub fn parse_config(text: &str) -> (Result<Config, String>, Vec<String>) {
    let (s, warnings) = pumbo_common::config::load::<Settings>(text);
    let warnings: Vec<String> = warnings.iter().map(|w| w.to_string()).collect();
    let cfg = (|| {
        let proxy = parse_addr(&s.proxy).ok_or("proxy must be ip:port, e.g. 127.0.0.1:25578")?;
        let key = match s.key.trim() {
            "" => None,
            k => Some(
                proto::wire::parse_key(k)
                    .ok_or("key must be the 64 hex digits of `pumbo bridge key` in the proxy console")?,
            ),
        };
        Ok(Config { proxy, key })
    })();
    (cfg, warnings)
}

fn parse_addr(s: &str) -> Option<([u8; 4], u16)> {
    let (ip, port) = s.rsplit_once(':')?;
    let ip: std::net::Ipv4Addr = ip.parse().ok()?;
    Some((ip.octets(), port.parse().ok()?))
}

/// Whether an address is loopback or private (spec §3.3: warn otherwise).
pub fn is_private(ip: [u8; 4]) -> bool {
    let ip = std::net::Ipv4Addr::from(ip);
    // 100.64.0.0/10: Tailscale and other CGNAT tunnels.
    let [a, b, ..] = ip.octets();
    ip.is_loopback() || ip.is_private() || (a == 100 && (64..128).contains(&b))
}

/// Reconnect delays: 1, 2, 5, 10, 30 s, then 30 s forever, plus up to 20%.
#[derive(Debug, Default, Clone)]
pub struct Backoff {
    step: usize,
}

const STEPS_MS: [u64; 5] = [1000, 2000, 5000, 10_000, 30_000];

impl Backoff {
    /// The next delay; `random` is any number (the jitter source).
    pub fn next_ms(&mut self, random: u32) -> u64 {
        let base = STEPS_MS.get(self.step).copied().unwrap_or(30_000);
        self.step = (self.step + 1).min(STEPS_MS.len() - 1);
        base + u64::from(random) % (base / 5 + 1)
    }

    pub fn reset(&mut self) {
        self.step = 0;
    }
}

/// Task runs per second of wall clock (Pumpkin's own TPS is not usable).
#[derive(Debug, Default, Clone)]
pub struct TpsMeter {
    start_ms: u64,
    ticks: u32,
    last: f32,
}

impl TpsMeter {
    pub fn tick(&mut self, now_ms: u64) {
        if self.start_ms == 0 {
            self.start_ms = now_ms;
        }
        self.ticks += 1;
        let span = now_ms.saturating_sub(self.start_ms);
        if span >= 5000 {
            self.last = self.ticks as f32 * 1000.0 / span as f32;
            self.start_ms = now_ms;
            self.ticks = 0;
        }
    }

    /// Over the last full five seconds (20 before the first).
    pub fn tps(&self) -> f32 {
        if self.last == 0.0 { 20.0 } else { self.last }
    }
}

/// Last known values per key; [`Changes::update`] says which changed.
#[derive(Debug, Clone)]
pub struct Changes<K, V> {
    last: HashMap<K, V>,
}

impl<K, V> Default for Changes<K, V> {
    fn default() -> Self {
        Changes { last: HashMap::new() }
    }
}

impl<K: Eq + Hash + Clone, V: PartialEq + Clone> Changes<K, V> {
    /// Takes the current values of all keys; returns `(key, old, new)` for
    /// those that changed (`old` is `None` for new keys). Keys not given are
    /// forgotten.
    pub fn update(&mut self, now: impl IntoIterator<Item = (K, V)>) -> Vec<(K, Option<V>, V)> {
        let mut next = HashMap::new();
        let mut out = Vec::new();
        for (k, v) in now {
            let old = self.last.remove(&k);
            if old.as_ref() != Some(&v) {
                out.push((k.clone(), old, v.clone()));
            }
            next.insert(k, v);
        }
        self.last = next;
        out
    }

    pub fn forget(&mut self, k: &K) {
        self.last.remove(k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file() {
        let (c, w) = parse_config(DEFAULT_CONFIG);
        assert_eq!((c.unwrap(), w.len()), (Config { proxy: ([127, 0, 0, 1], 25578), key: None }, 0));
        let k = "ab".repeat(32);
        let c = parse_config(&format!("proxy: \"10.0.0.2:25578\"  # LAN\nkey: {k}\n")).0.unwrap();
        assert_eq!(c.proxy, ([10, 0, 0, 2], 25578));
        assert_eq!(c.key, Some([0xab; 32]));
        assert!(parse_config("proxy: localhost:1\n").0.unwrap_err().contains("ip:port"));
        assert!(parse_config("proxy: 1.2.3.4:5\nkey: nope\n").0.unwrap_err().contains("64 hex"));
        let (c, w) = parse_config("proxy: 1.2.3.4:5\nsecret: x\n");
        assert!(c.is_ok() && w.len() == 1 && w[0].contains("secret"), "{w:?}");
        assert!(is_private([127, 0, 0, 1]) && is_private([100, 101, 1, 2]) && !is_private([8, 8, 8, 8]));
    }

    #[test]
    fn backoff_steps_with_jitter() {
        let mut b = Backoff::default();
        let d: Vec<u64> = (0..7).map(|_| b.next_ms(0)).collect();
        assert_eq!(d, [1000, 2000, 5000, 10_000, 30_000, 30_000, 30_000]);
        b.reset();
        let j = b.next_ms(u32::MAX);
        assert!((1000..=1200).contains(&j), "{j}");
    }

    #[test]
    fn tps_and_changes() {
        let mut m = TpsMeter::default();
        assert_eq!(m.tps(), 20.0);
        for i in 0..=100u64 {
            m.tick(1 + i * 50);
        }
        assert!((m.tps() - 20.0).abs() < 0.5, "{}", m.tps());
        let mut c: Changes<u8, &str> = Changes::default();
        assert_eq!(c.update([(1, "a"), (2, "b")]).len(), 2);
        assert_eq!(c.update([(1, "a"), (2, "c")]), vec![(2, Some("b"), "c")]);
        assert_eq!(c.update([(1, "a")]), vec![]);
        assert_eq!(c.update([(1, "a"), (2, "c")]), vec![(2, None, "c")], "a returning key is new");
    }
}

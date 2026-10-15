//! PumboBridge core (docs/pumbo-bridge-spec.md): everything of the bridge
//! that does not touch Pumpkin or the socket, so it is tested natively.
//!
//! - [`session::Session`]: the bridge's side of the protocol (handshake with
//!   the proxy proof checked first, frames, pings, pairing nonces, limits).
//! - [`perms::Perms`]: permission attachments from `perm-set`, their
//!   difference with what is set, and the precedence of a local PumboPerms.
//! - [`Backoff`], [`Reconnect`], [`TaskWatch`], [`too_old`], [`TpsMeter`],
//!   [`Changes`]: reconnect delays and what to log about them, frozen ticks
//!   and the commands that waited them out, the tick rate and "what changed
//!   since last time" for worlds and player stats.
//!
//! The Pumpkin layer (`plugins/pumbo-bridge-pumpkin`) runs one task per tick
//! that pumps the socket and calls into this crate (its event handlers do the
//! socket part while ticks are frozen); it never subscribes to an event that
//! its own calls fire (spec §3.6).

// Panics trap the host and stdout is lost; these stay errors outside tests.
#![deny(
    clippy::string_slice,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::await_holding_lock,
    clippy::await_holding_refcell_ref,
    clippy::integer_division,
    clippy::modulo_arithmetic,
    clippy::unchecked_time_subtraction,
    clippy::get_unwrap,
    clippy::exit,
    clippy::large_stack_arrays,
    clippy::large_stack_frames,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn,
    clippy::significant_drop_in_scrutinee
)]
#![cfg_attr(
    test,
    allow(
        clippy::string_slice,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::await_holding_lock,
        clippy::await_holding_refcell_ref,
        clippy::integer_division,
        clippy::modulo_arithmetic,
        clippy::unchecked_time_subtraction,
        clippy::get_unwrap,
        clippy::exit,
        clippy::large_stack_arrays,
        clippy::large_stack_frames,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn,
        clippy::significant_drop_in_scrutinee
    )
)]

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
        base + u64::from(random) % (base.checked_div(5).unwrap_or(0) + 1)
    }

    pub fn reset(&mut self) {
        self.step = 0;
    }
}

/// Reconnecting to the proxy: the wait before the next attempt and the line
/// to log. A paired session that ends is logged every time and retried after
/// the shortest wait; failed attempts log a reason once, not every retry.
#[derive(Debug, Default)]
pub struct Reconnect {
    backoff: Backoff,
    said: String,
}

impl Reconnect {
    /// A session or an attempt ended (`paired`: the session had a server).
    /// `wait` replaces the backoff; `random` is the jitter source.
    pub fn down(&mut self, why: &str, paired: bool, wait: Option<u64>, random: u32) -> (Option<String>, u64) {
        let line = if paired {
            self.backoff.reset();
            format!("lost the proxy connection: {why}, reconnecting")
        } else {
            format!("not connected to the proxy: {why}; retrying")
        };
        let wait = wait.unwrap_or_else(|| self.backoff.next_ms(random));
        if line == self.said {
            return (None, wait);
        }
        self.said.clone_from(&line);
        (Some(line), wait)
    }

    /// Paired with the proxy (again): the line to log.
    pub fn paired(&mut self, server: &str) -> String {
        self.backoff.reset();
        self.said.clear();
        format!("connected to PumboProx as server {server}")
    }
}

/// Longer than this without a run of the per-tick task counts as stopped ticks.
pub const STOPPED_MS: u64 = 2000;

/// Whether the per-tick task still runs. `/tick freeze` stops Pumpkin's
/// plugin scheduler but not its events, so event handlers keep the
/// connection then; the stop and the end of it get one line each.
#[derive(Debug, Default)]
pub struct TaskWatch {
    last_ms: u64,
    noticed: bool,
}

impl TaskWatch {
    /// The per-tick task runs: after a stop an event handler noticed, the
    /// line to log. A long tick nobody noticed says nothing.
    pub fn ran(&mut self, now_ms: u64) -> Option<String> {
        let gap = now_ms.saturating_sub(self.last_ms);
        self.last_ms = now_ms;
        let secs = std::time::Duration::from_millis(gap).as_secs();
        std::mem::take(&mut self.noticed)
            .then(|| format!("the server's ticks were frozen or stalled for {secs} s; the bridge works again"))
    }

    /// An event handler runs: whether the task stopped, and the line to log
    /// the first time this stop is seen.
    pub fn stopped(&mut self, now_ms: u64) -> (bool, Option<&'static str>) {
        let stopped = self.last_ms != 0 && now_ms.saturating_sub(self.last_ms) > STOPPED_MS;
        let first = stopped && !self.noticed;
        self.noticed |= stopped;
        (stopped, first.then_some(FROZEN))
    }
}

/// A proxy command that waited longer than this for the per-tick task (frozen
/// ticks) is answered `timeout` instead of run: the proxy gave up on it long ago.
pub const COMMAND_MAX_AGE_MS: u64 = 30_000;

/// Whether a command that came in at `arrived_ms` is too old to run.
pub fn too_old(arrived_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(arrived_ms) > COMMAND_MAX_AGE_MS
}

const FROZEN: &str = "the server's ticks are frozen (/tick freeze): until /tick unfreeze the bridge answers the proxy only when something happens on the server, and the proxy drops it after 15 s without an answer";

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
    fn lost_session_is_logged_and_retried_from_the_shortest_wait() {
        let mut r = Reconnect::default();
        let lost = "lost the proxy connection: closed by the proxy, reconnecting".to_string();
        assert_eq!(r.down("closed by the proxy", true, None, 0), (Some(lost.clone()), 1000));
        // The proxy is down: one line for the reason, then quiet retries up to 30 s apart.
        let refused = "not connected to the proxy: the proxy is not listening; retrying".to_string();
        let waits: Vec<_> = (0..6).map(|_| r.down("the proxy is not listening", false, None, 0)).collect();
        assert_eq!(
            waits,
            [(Some(refused), 2000), (None, 5000), (None, 10_000), (None, 30_000), (None, 30_000), (None, 30_000)]
        );
        assert_eq!(r.down("x", false, Some(30_000), 0).1, 30_000, "a fixed wait");
        // Paired again: said every time, and the next loss starts from 1 s again.
        assert_eq!(r.paired("gp"), "connected to PumboProx as server gp");
        assert_eq!(r.down("closed by the proxy", true, None, 0), (Some(lost), 1000));
        assert_eq!(r.paired("gp"), "connected to PumboProx as server gp");
    }

    #[test]
    fn stopped_ticks_are_noticed_once_and_their_end_is_said() {
        let mut w = TaskWatch::default();
        assert_eq!(w.stopped(10_000), (false, None), "before the first run nothing is known");
        assert_eq!(w.ran(100), None);
        assert_eq!(w.ran(150), None);
        assert_eq!(w.stopped(150 + STOPPED_MS), (false, None), "a slow tick is no stop");
        let (stopped, line) = w.stopped(151 + STOPPED_MS);
        assert!(stopped && line.is_some_and(|l| l.contains("/tick freeze")));
        assert_eq!(w.stopped(9000), (true, None), "said once per stop");
        let line = w.ran(37_150).unwrap();
        assert!(line.contains("frozen or stalled for 37 s"), "{line}");
        assert_eq!(w.stopped(37_200), (false, None));
        assert_eq!(w.stopped(40_000).1, Some(FROZEN), "the next stop is said again");
    }

    /// A long tick no event handler ran during (a stall, not `/tick freeze`)
    /// gives no warning when the task runs again.
    #[test]
    fn a_stall_nobody_noticed_says_nothing() {
        let mut w = TaskWatch::default();
        assert_eq!(w.ran(100), None);
        assert_eq!(w.ran(100 + 10 * STOPPED_MS), None);
        assert_eq!(w.stopped(30_000), (true, Some(FROZEN)));
        assert!(w.ran(30_050).is_some(), "a stop a handler saw is said");
        assert_eq!(w.ran(30_050 + 10 * STOPPED_MS), None, "and only that one");
    }

    #[test]
    fn commands_older_than_30_s_are_answered_not_run() {
        assert!(!too_old(1000, 1000 + COMMAND_MAX_AGE_MS));
        assert!(too_old(1000, 1001 + COMMAND_MAX_AGE_MS));
        assert!(!too_old(5000, 1000), "a clock going back drops nothing");
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

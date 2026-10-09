//! The bridge's side of the protocol (spec §3.3–3.5). No I/O: the Pumpkin
//! layer feeds what it read into [`Session::feed`] and writes what
//! [`Session::out`] holds, never blocking.
//!
//! The proxy's proof is checked before the bridge sends anything but
//! `hello`, so a fake proxy (a port taken while PumboProx is down) learns
//! nothing and gets no command executed.

use pumbo_bridge_proto::PROTO;
use pumbo_bridge_proto::ciborium::Value;
use pumbo_bridge_proto::wire::{self, Codec, Event, Instance, Key, Msg, Nonce, Side};

pub const PING_MS: u64 = 5000;
pub const SILENCE_MS: u64 = 15_000;
pub const HANDSHAKE_MS: u64 = 5000;
/// Output queue limit; more ends the session (state returns on reconnect).
pub const MAX_OUT: usize = 4 * 1024 * 1024;
/// Pairing nonces reported per 10 s.
pub const SEEN_PER_10S: u32 = 8;
/// After a wrong proxy proof the bridge waits this long.
pub const FAKE_PROXY_WAIT_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// The other end does not have our key: not our proxy, or the key in
    /// `config.yml` is not the proxy's.
    FakeProxy,
    Closed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Paired: this server's name; send `sync` now.
    Welcome {
        server: String,
        stats_ms: u32,
        player_stats_ms: u32,
    },
    Cmd {
        id: u64,
        method: String,
        args: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Challenge,
    Up,
    Paired(String),
}

/// One connection to the proxy.
pub struct Session {
    key: Key,
    nb: Nonce,
    instance: Instance,
    codec: Codec,
    phase: Phase,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    started_ms: u64,
    last_rx_ms: u64,
    last_ping_ms: u64,
    seen_window: (u64, u32),
    caps: Vec<String>,
    catalog: Vec<String>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("phase", &self.phase).finish_non_exhaustive()
    }
}

/// `(bridge, pumpkin, mc)` versions for `hello`.
pub type Versions = (String, String, String);

impl Session {
    /// A new connection: `hello` is queued. `nb` must be fresh random bytes;
    /// `instance` stays the same until the server restarts.
    pub fn new(
        key: Key,
        nb: Nonce,
        instance: Instance,
        versions: Versions,
        caps: Vec<String>,
        catalog: Vec<String>,
        now_ms: u64,
    ) -> Session {
        let mut s = Session {
            key,
            nb,
            instance,
            codec: Codec::new(Side::Bridge),
            phase: Phase::Challenge,
            inbuf: Vec::new(),
            out: Vec::new(),
            started_ms: now_ms,
            last_rx_ms: now_ms,
            last_ping_ms: now_ms,
            seen_window: (now_ms, 0),
            caps,
            catalog,
        };
        let (bridge, pumpkin, mc) = versions;
        let hello = Msg::Hello { proto: PROTO, bridge, pumpkin, mc, instance, nb };
        if let Ok(f) = s.codec.encode(&hello) {
            s.out.extend(f);
        }
        s
    }

    /// The server this session is paired with.
    pub fn server(&self) -> Option<&str> {
        match &self.phase {
            Phase::Paired(s) => Some(s),
            _ => None,
        }
    }

    /// Past the handshake (pairing may still be missing).
    pub fn authenticated(&self) -> bool {
        self.phase != Phase::Challenge
    }

    /// Bytes to write; the caller drains what it wrote.
    pub fn out(&mut self) -> &mut Vec<u8> {
        &mut self.out
    }

    /// Bytes read from the socket.
    pub fn feed(&mut self, data: &[u8], now_ms: u64) -> Result<Vec<Incoming>, End> {
        self.inbuf.extend_from_slice(data);
        let mut got = Vec::new();
        let mut used = 0;
        loop {
            let rest = self.inbuf.get(used..).unwrap_or_default();
            match self.codec.decode(rest) {
                Ok(Some((m, n))) => {
                    used += n;
                    self.last_rx_ms = now_ms;
                    self.on(m, &mut got)?;
                }
                Ok(None) => break,
                Err(e) => return Err(End::Closed(e.to_string())),
            }
        }
        self.inbuf.drain(..used);
        Ok(got)
    }

    fn on(&mut self, m: Msg, got: &mut Vec<Incoming>) -> Result<(), End> {
        match (&self.phase, m) {
            (Phase::Challenge, Msg::Challenge { np, proof }) => {
                if !wire::check_proxy_proof(&self.key, &self.nb, &np, &self.instance, &proof) {
                    return Err(End::FakeProxy);
                }
                let auth = Msg::Auth { proof: wire::bridge_proof(&self.key, &np, &self.nb, &self.instance) };
                let f = self.codec.encode(&auth).map_err(|e| End::Closed(e.to_string()))?;
                self.out.extend(f);
                self.codec.set_key(wire::session_key(&self.key, &self.nb, &np));
                self.phase = Phase::Up;
                let info =
                    Msg::Info { caps: std::mem::take(&mut self.caps), catalog: std::mem::take(&mut self.catalog) };
                self.send(&info)
            }
            (Phase::Challenge, _) => Err(End::Closed("expected a challenge".into())),
            (_, Msg::Welcome { server, stats_ms, player_stats_ms, .. }) => {
                self.phase = Phase::Paired(server.clone());
                got.push(Incoming::Welcome { server, stats_ms, player_stats_ms });
                Ok(())
            }
            (_, Msg::Cmd { id, method, args }) => {
                got.push(Incoming::Cmd { id, method, args });
                Ok(())
            }
            (_, Msg::Ping) => self.send(&Msg::Pong),
            _ => Ok(()),
        }
    }

    /// Pings and the silence limit; call every tick.
    pub fn tick(&mut self, now_ms: u64) -> Result<(), End> {
        if self.phase == Phase::Challenge {
            if now_ms.saturating_sub(self.started_ms) > HANDSHAKE_MS {
                return Err(End::Closed("no challenge within 5 s".into()));
            }
            return Ok(());
        }
        if now_ms.saturating_sub(self.last_rx_ms) > SILENCE_MS {
            return Err(End::Closed("the proxy was silent for 15 s".into()));
        }
        if now_ms.saturating_sub(self.last_ping_ms) >= PING_MS {
            self.last_ping_ms = now_ms;
            self.send(&Msg::Ping)?;
        }
        Ok(())
    }

    /// Queues a message (dropped before the handshake).
    pub fn send(&mut self, m: &Msg) -> Result<(), End> {
        if !self.codec.keyed() {
            return Ok(());
        }
        let f = self.codec.encode(m).map_err(|e| End::Closed(e.to_string()))?;
        self.out.extend(f);
        if self.out.len() > MAX_OUT {
            return Err(End::Closed("output queue over 4 MiB".into()));
        }
        Ok(())
    }

    /// The answer to command `id`.
    pub fn reply(&mut self, id: u64, r: Result<Value, (&str, Option<String>)>) -> Result<(), End> {
        let m = match r {
            Ok(v) => Msg::Res { id, ok: Some(v), err: None, detail: None },
            Err((code, detail)) => Msg::Res { id, ok: None, err: Some(code.to_string()), detail },
        };
        self.send(&m)
    }

    pub fn event(&mut self, ev: Event) -> Result<(), End> {
        self.send(&Msg::Ev { ev })
    }

    /// A pairing ping arrived (from the handler, through a queue): reported
    /// while unpaired, at most 8 per 10 s.
    pub fn seen(&mut self, nonce: &str, now_ms: u64) -> Result<(), End> {
        if self.phase != Phase::Up {
            return Ok(());
        }
        if now_ms.saturating_sub(self.seen_window.0) > 10_000 {
            self.seen_window = (now_ms, 0);
        }
        if self.seen_window.1 >= SEEN_PER_10S {
            return Ok(());
        }
        self.seen_window.1 += 1;
        self.send(&Msg::Seen { nonce: nonce.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: Key = [3; 32];
    const NB: Nonce = [4; 32];
    const INST: Instance = [5; 16];

    fn session() -> Session {
        Session::new(
            K,
            NB,
            INST,
            ("0.1.0".into(), "0.2.0".into(), "26.3".into()),
            vec!["heal".into()],
            vec!["minecraft:command.gamemode".into()],
            0,
        )
    }

    /// The proxy end of a test: decodes what the bridge sent.
    struct Proxy {
        codec: Codec,
        key: Key,
        buf: Vec<u8>,
    }

    impl Proxy {
        fn read(&mut self, s: &mut Session) -> Vec<Msg> {
            self.buf.append(s.out());
            let mut got = Vec::new();
            while let Some((m, n)) = self.codec.decode(&self.buf).unwrap() {
                self.buf.drain(..n);
                got.push(m);
            }
            got
        }

        fn frame(&mut self, m: &Msg) -> Vec<u8> {
            self.codec.encode(m).unwrap()
        }

        /// hello → challenge → auth: returns the bridge's messages after auth.
        fn handshake(&mut self, s: &mut Session) -> Vec<Msg> {
            let Msg::Hello { nb, instance, .. } = self.read(s).remove(0) else { panic!("hello first") };
            let np = [6; 32];
            let ch = self.frame(&Msg::Challenge { np, proof: wire::proxy_proof(&self.key, &nb, &np, &instance) });
            assert_eq!(s.feed(&ch, 1).unwrap(), vec![]);
            let mut buf = std::mem::take(s.out());
            let (Msg::Auth { proof }, n) = self.codec.decode(&buf).unwrap().unwrap() else { panic!("auth") };
            assert!(wire::check_bridge_proof(&self.key, &np, &nb, &instance, &proof));
            buf.drain(..n);
            self.buf = buf;
            self.codec.set_key(wire::session_key(&self.key, &nb, &np));
            let mut after = Vec::new();
            while let Some((m, n)) = self.codec.decode(&self.buf).unwrap() {
                self.buf.drain(..n);
                after.push(m);
            }
            after
        }
    }

    fn proxy(key: Key) -> Proxy {
        Proxy { codec: Codec::new(Side::Proxy), key, buf: Vec::new() }
    }

    #[test]
    fn handshake_pairing_and_commands() {
        let mut s = session();
        let mut p = proxy(K);
        let after = p.handshake(&mut s);
        assert!(matches!(&after[..], [Msg::Info { caps, .. }] if caps == &["heal"]));
        assert!(s.authenticated() && s.server().is_none());
        s.seen("ab", 2).unwrap();
        assert_eq!(p.read(&mut s), vec![Msg::Seen { nonce: "ab".into() }]);
        let w =
            p.frame(&Msg::Welcome { server: "lobby".into(), groups: vec![], stats_ms: 5000, player_stats_ms: 2000 });
        let c = p.frame(&Msg::Cmd { id: 7, method: "heal".into(), args: Value::Null });
        let mut both = w;
        both.extend(c);
        let got = s.feed(&both, 3).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(s.server(), Some("lobby"));
        s.seen("cd", 4).unwrap();
        assert!(p.read(&mut s).is_empty(), "no pairing reports once paired");
        s.reply(7, Err(("no-player", None))).unwrap();
        assert!(matches!(&p.read(&mut s)[..], [Msg::Res { id: 7, err: Some(e), .. }] if e == "no-player"));
        // Pings every 5 s, and the session ends after 15 s of silence.
        s.tick(5004).unwrap();
        assert_eq!(p.read(&mut s), vec![Msg::Ping]);
        assert!(matches!(s.tick(15_004), Err(End::Closed(_))));
    }

    #[test]
    fn a_fake_proxy_gets_nothing() {
        let mut s = session();
        let mut fake = proxy([9; 32]);
        let Msg::Hello { nb, instance, .. } = fake.read(&mut s).remove(0) else { panic!() };
        let np = [1; 32];
        let ch = fake.frame(&Msg::Challenge { np, proof: wire::proxy_proof(&[9; 32], &nb, &np, &instance) });
        assert_eq!(s.feed(&ch, 1), Err(End::FakeProxy));
        assert!(s.out().is_empty(), "no auth to a fake proxy");
        // Commands before the handshake are not commands.
        let mut s = session();
        let mut p = proxy(K);
        p.read(&mut s);
        let cmd = p.frame(&Msg::Cmd { id: 1, method: "heal".into(), args: Value::Null });
        assert!(matches!(s.feed(&cmd, 1), Err(End::Closed(_))));
    }

    #[test]
    fn replayed_frames_and_limits() {
        let mut s = session();
        let mut p = proxy(K);
        p.handshake(&mut s);
        let ping = p.frame(&Msg::Ping);
        s.feed(&ping, 2).unwrap();
        assert!(matches!(s.feed(&ping, 3), Err(End::Closed(_))), "replay ends the session");
        let mut s = session();
        let mut p = proxy(K);
        p.handshake(&mut s);
        for i in 0..SEEN_PER_10S + 3 {
            s.seen(&format!("{i}"), 10).unwrap();
        }
        assert_eq!(p.read(&mut s).len(), SEEN_PER_10S as usize);
        let big = Value::Bytes(vec![0; 1024 * 1024]);
        let r = (0..5).try_for_each(|i| s.reply(i, Ok(big.clone())));
        assert!(matches!(r, Err(End::Closed(_))), "output over 4 MiB ends the session");
        assert!(matches!(session().tick(HANDSHAKE_MS + 1), Err(End::Closed(_))));
    }
}

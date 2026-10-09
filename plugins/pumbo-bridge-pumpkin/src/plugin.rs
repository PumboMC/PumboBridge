//! The WebAssembly plugin. Rules (spec §3.6):
//!
//! - only the task that runs every tick touches the socket, never blocking
//!   (`start-connect`/`finish-connect`, non-blocking reads, `check-write`);
//! - handlers only record into their own cells, and no cell stays borrowed
//!   across a Pumpkin call (a call may run other handlers of this plugin);
//! - no subscription to an event our own calls fire (`crate::SUBSCRIBED`);
//! - commands in a 2 ms or 64-command budget, at most one `teleport-world`
//!   per tick.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use pumbo_bridge_core::perms::{Change, Perms};
use pumbo_bridge_core::proto::api::{self, GameMode as Mode, PermsMode, Pos};
use pumbo_bridge_core::proto::ciborium::Value;
use pumbo_bridge_core::proto::uuid::Uuid as Id;
use pumbo_bridge_core::proto::wire::{self, Event, Msg};
use pumbo_bridge_core::proto::{err, method};
use pumbo_bridge_core::session::{End, FAKE_PROXY_WAIT_MS, Incoming, Session};
use pumbo_bridge_core::{Backoff, Changes, Config, TpsMeter};
use wasip2::io::streams::{InputStream, OutputStream, StreamError};
use wasip2::sockets::instance_network::instance_network;
use wasip2::sockets::network::{ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress};
use wasip2::sockets::tcp::TcpSocket;
use wasip2::sockets::tcp_create_socket::create_tcp_socket;

use crate::papi::common::{GameMode, NamedColor};
use crate::papi::events::{
    EventData, EventHandler, EventPriority, PlayerCommandSendEvent, PlayerDeathEvent, PlayerJoinEvent,
    PlayerLeaveEvent, PlayerRespawnEvent, ServerListPingEvent,
};
use crate::papi::logging::{LogLevel, log};
use crate::papi::player::{StatusEffectInstance, StatusEffectType};
use crate::papi::scheduler::SchedulerExt;
use crate::papi::text::TextComponent;
use crate::papi::uuid::Uuid;
use crate::papi::{Context, ItemStack, Player, Plugin, PluginMetadata, Screen, Server, permissions};

#[cfg(feature = "mc263")]
const PUMPKIN: (&str, &str) = ("0.2.0", "26.3");
#[cfg(feature = "mc262")]
const PUMPKIN: (&str, &str) = ("0.1.0-dev", "26.2");

const BUDGET: Duration = Duration::from_millis(2);
const MAX_PER_TICK: usize = 64;
const MAX_QUEUED: usize = 1024;
const READ_MAX: u64 = 256 * 1024;
/// How often a local PumboPerms is looked for (spec §5.3).
const PERMS_CHECK_MS: u64 = 10_000;
/// Worlds of online players are compared every this many ticks.
const WORLD_POLL_TICKS: u64 = 10;
/// The default world of spawn teleports and queries.
const OVERWORLD: &str = "minecraft:overworld";

enum Net {
    Idle { retry_at_ms: u64 },
    Connecting { sock: TcpSocket },
    // Streams are children of the socket: declared (and dropped) first.
    Up { rx: InputStream, tx: OutputStream, _sock: TcpSocket, session: Box<Session> },
}

struct Cmd {
    id: u64,
    method: String,
    args: Value,
}

/// State of the per-tick task (never borrowed by handlers).
struct Task {
    net: Net,
    backoff: Backoff,
    queue: VecDeque<Cmd>,
    tps: TpsMeter,
    ticks: u64,
    stats_ms: (u64, u64),
    next_stats: (u64, u64),
    worlds: Changes<Id, String>,
    player_stats: Changes<Id, api::PlayerInfo>,
    next_perms_check: u64,
    last_entities_ms: u64,
    /// Logged once per state, not once per retry.
    last_log: String,
}

thread_local! {
    static CFG: RefCell<Option<Config>> = const { RefCell::new(None) };
    static DIR: RefCell<String> = const { RefCell::new(String::new()) };
    static INSTANCE: RefCell<[u8; 16]> = const { RefCell::new([0; 16]) };
    static TASK: RefCell<Option<Task>> = const { RefCell::new(None) };
    /// Filled by handlers, emptied by the task.
    static SEEN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
    static REFRESH: RefCell<Vec<Id>> = const { RefCell::new(Vec::new()) };
    /// Shared by the task, the join and leave handlers and ipc.
    static PERMS: RefCell<Perms> = RefCell::new(Perms::default());
    /// For `ipc` and unload, which get no server handle.
    static SERVER: RefCell<Option<Server>> = const { RefCell::new(None) };
}

fn started() -> Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    u64::try_from(started().elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn info(msg: &str) {
    log(LogLevel::Info, &format!("[PumboBridge] {msg}"));
}

fn warn(msg: &str) {
    log(LogLevel::Warn, &format!("[PumboBridge] {msg}"));
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    for (o, b) in out.iter_mut().zip(wasip2::random::random::get_random_bytes(N as u64)) {
        *o = b;
    }
    out
}

fn id_of(p: &Player) -> Id {
    let u = p.get_id();
    Id::from_u64_pair(u.high, u.low)
}

fn uuid(id: Id) -> Uuid {
    let (high, low) = id.as_u64_pair();
    Uuid { high, low }
}

fn pos_of(p: &Player) -> Pos {
    let (x, y, z) = p.get_position();
    Pos { world: p.get_world().get_dimension(), x, y, z, yaw: Some(p.get_yaw()), pitch: Some(p.get_pitch()) }
}

fn mode_of(m: GameMode) -> Mode {
    match m {
        GameMode::Survival => Mode::Survival,
        GameMode::Creative => Mode::Creative,
        GameMode::Adventure => Mode::Adventure,
        GameMode::Spectator => Mode::Spectator,
    }
}

fn mode_to(m: Mode) -> GameMode {
    match m {
        Mode::Survival => GameMode::Survival,
        Mode::Creative => GameMode::Creative,
        Mode::Adventure => GameMode::Adventure,
        Mode::Spectator => GameMode::Spectator,
    }
}

fn queue_event(ev: Event) {
    EVENTS.with(|e| {
        let mut e = e.borrow_mut();
        // Without a session events are not kept (state returns with `sync`).
        if e.len() < 4096 {
            e.push(ev);
        }
    });
}

/// Applies attachment changes; the command list is refreshed by the task
/// (a pumping call, never from inside a handler).
fn apply(server: Option<&Server>, changes: Vec<Change>) {
    for c in changes {
        let Some(p) = server.and_then(|s| s.get_player_by_uuid(uuid(c.player))) else {
            continue;
        };
        for n in &c.unset {
            p.unset_permission(n);
        }
        for (n, v) in &c.set {
            p.set_permission(n, *v);
        }
        if c.refresh {
            REFRESH.with(|r| r.borrow_mut().push(c.player));
        }
    }
}

// ---------------------------------------------------------------- network

fn start_connect(cfg: &Config) -> Result<TcpSocket, ErrorCode> {
    let ([a, b, c, d], port) = cfg.proxy;
    let sock = create_tcp_socket(IpAddressFamily::Ipv4)?;
    let addr = IpSocketAddress::Ipv4(Ipv4SocketAddress { port, address: (a, b, c, d) });
    sock.start_connect(&instance_network(), addr)?;
    Ok(sock)
}

fn say_once(t: &mut Task, msg: String) {
    if t.last_log != msg {
        info(&msg);
        t.last_log = msg;
    }
}

fn idle(t: &mut Task, why: &str, wait_ms: Option<u64>) -> Net {
    let wait = wait_ms.unwrap_or_else(|| t.backoff.next_ms(u32::from_le_bytes(random_bytes())));
    say_once(t, format!("not connected to the proxy: {why}; retrying"));
    Net::Idle { retry_at_ms: now_ms() + wait }
}

/// One step of the connection: connect, read, write. Never blocks.
fn pump(t: &mut Task, cfg: &Config, server: &Server) -> Vec<Incoming> {
    let Some(key) = cfg.key else { return Vec::new() };
    let now = now_ms();
    let mut got = Vec::new();
    let state = std::mem::replace(&mut t.net, Net::Idle { retry_at_ms: now });
    t.net = match state {
        Net::Idle { retry_at_ms } if now >= retry_at_ms => match start_connect(cfg) {
            Ok(sock) => Net::Connecting { sock },
            Err(ErrorCode::AccessDenied) => idle(
                t,
                "network blocked by loopback_only; set [plugins.overrides.pumbobridge] loopback_only = false in config.toml of Pumpkin",
                Some(30_000),
            ),
            Err(e) => idle(t, &format!("{e:?}"), None),
        },
        s @ Net::Idle { .. } => s,
        Net::Connecting { sock } => match sock.finish_connect() {
            Ok((rx, tx)) => {
                let versions = (env!("CARGO_PKG_VERSION").to_string(), PUMPKIN.0.to_string(), PUMPKIN.1.to_string());
                let caps = crate::CAPS.iter().map(|s| s.to_string()).collect();
                let catalog = pumbo_common::pumpkin::COMMAND_NODES.iter().map(|s| s.to_string()).collect();
                let instance = INSTANCE.with(|i| *i.borrow());
                let session = Box::new(Session::new(key, random_bytes(), instance, versions, caps, catalog, now));
                Net::Up { rx, tx, _sock: sock, session }
            }
            Err(ErrorCode::WouldBlock) => Net::Connecting { sock },
            Err(ErrorCode::ConnectionRefused) => idle(t, "the proxy is not listening", None),
            Err(e) => idle(t, &format!("{e:?}"), None),
        },
        Net::Up { rx, tx, _sock, mut session } => match io(&rx, &tx, &mut session, now, &mut got) {
            Ok(()) => Net::Up { rx, tx, _sock, session },
            Err(end) => {
                drop((rx, tx));
                got.clear();
                t.queue.clear();
                match end {
                    End::FakeProxy => idle(
                        t,
                        "the proxy did not prove the bridge key (a wrong key in config.yml, or not PumboProx)",
                        Some(FAKE_PROXY_WAIT_MS),
                    ),
                    End::Closed(why) => {
                        if session.server().is_some() {
                            t.backoff.reset();
                        }
                        idle(t, &why, None)
                    }
                }
            }
        },
    };
    let _ = server;
    got
}

fn io(rx: &InputStream, tx: &OutputStream, s: &mut Session, now: u64, got: &mut Vec<Incoming>) -> Result<(), End> {
    match rx.read(READ_MAX) {
        Ok(data) if !data.is_empty() => got.extend(s.feed(&data, now)?),
        Ok(_) => {}
        Err(StreamError::Closed) => return Err(End::Closed("closed by the proxy".into())),
        Err(StreamError::LastOperationFailed(e)) => return Err(End::Closed(e.to_debug_string())),
    }
    s.tick(now)?;
    flush(tx, s)
}

fn flush(tx: &OutputStream, s: &mut Session) -> Result<(), End> {
    let out = s.out();
    if out.is_empty() {
        return Ok(());
    }
    let permit = tx.check_write().map_err(|e| End::Closed(format!("check-write: {e:?}")))?;
    let n = usize::try_from(permit).unwrap_or(usize::MAX).min(out.len());
    if n > 0 {
        let chunk = out.get(..n).unwrap_or_default();
        tx.write(chunk).map_err(|e| End::Closed(format!("write: {e:?}")))?;
        tx.flush().map_err(|e| End::Closed(format!("flush: {e:?}")))?;
        out.drain(..n);
    }
    Ok(())
}

fn session(t: &mut Task) -> Option<&mut Session> {
    match &mut t.net {
        Net::Up { session, .. } => Some(session),
        _ => None,
    }
}

/// Sends through the session; an error ends it at the next pump.
fn send(t: &mut Task, m: &Msg) {
    if let Some(s) = session(t)
        && let Err(End::Closed(why)) = s.send(m)
    {
        warn(&format!("dropping the session: {why}"));
        t.net = Net::Idle { retry_at_ms: now_ms() + 1000 };
    }
}

// ---------------------------------------------------------------- the tick

fn tick(server: &Server) {
    let Some(cfg) = CFG.with(|c| c.borrow().clone()) else { return };
    let Some(mut t) = TASK.with(|c| c.borrow_mut().take()) else { return };
    let t0 = Instant::now();
    t.ticks += 1;
    t.tps.tick(now_ms());
    let incoming = pump(&mut t, &cfg, server);
    for i in incoming {
        match i {
            Incoming::Welcome { server: name, stats_ms, player_stats_ms } => {
                say_once(&mut t, format!("connected to PumboProx as server {name}"));
                t.backoff.reset();
                t.stats_ms = (u64::from(stats_ms.max(1000)), u64::from(player_stats_ms.max(500)));
                t.worlds = Changes::default();
                t.player_stats = Changes::default();
                let sync = Msg::Sync {
                    players: server
                        .get_all_players()
                        .iter()
                        .map(|p| wire::PlayerState {
                            player: id_of(p),
                            name: p.get_name(),
                            pos: pos_of(p),
                            gamemode: mode_of(p.get_gamemode()),
                        })
                        .collect(),
                    worlds: worlds(server),
                };
                send(&mut t, &sync);
                let mode = PERMS.with(|p| p.borrow().mode());
                send(&mut t, &Msg::Ev { ev: Event::PermsMode { mode } });
            }
            Incoming::Cmd { id, method, args } => {
                if t.queue.len() >= MAX_QUEUED {
                    warn("over 1024 commands waiting, dropping the session");
                    t.net = Net::Idle { retry_at_ms: now_ms() + 1000 };
                    t.queue.clear();
                    break;
                }
                t.queue.push_back(Cmd { id, method, args });
            }
        }
    }
    let paired = matches!(&t.net, Net::Up { session, .. } if session.server().is_some());
    // Pairing pings seen by the handler.
    let seen = SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()));
    if let Some(s) = session(&mut t) {
        for n in seen {
            let _ = s.seen(&n, now_ms());
        }
    }
    let events = EVENTS.with(|e| std::mem::take(&mut *e.borrow_mut()));
    if paired {
        for ev in events {
            send(&mut t, &Msg::Ev { ev });
        }
    }
    let refresh = REFRESH.with(|r| std::mem::take(&mut *r.borrow_mut()));
    for id in refresh {
        if let Some(p) = server.get_player_by_uuid(uuid(id)) {
            p.set_permission_level(p.get_permission_level());
        }
    }
    if paired {
        periodic(&mut t, server);
    }
    if now_ms() >= t.next_perms_check {
        t.next_perms_check = now_ms() + PERMS_CHECK_MS;
        perms_check(&mut t, server);
    }
    let mut done = 0;
    while done < MAX_PER_TICK && t0.elapsed() < BUDGET {
        let Some(c) = t.queue.pop_front() else { break };
        done += 1;
        let (r, world_change) = run(&mut t, server, &c);
        if let Some(r) = r {
            let reply = Msg::Res {
                id: c.id,
                ok: r.as_ref().ok().cloned(),
                err: r.as_ref().err().map(|(e, _)| e.to_string()),
                detail: r.err().and_then(|(_, d)| d),
            };
            send(&mut t, &reply);
        }
        if world_change {
            break; // one teleport-world per tick (spec §3.6)
        }
    }
    // Answers of this tick go out now, not a tick later.
    if let Net::Up { tx, session, .. } = &mut t.net
        && let Err(End::Closed(why)) = flush(tx, session)
    {
        warn(&format!("write failed: {why}"));
        t.net = Net::Idle { retry_at_ms: now_ms() + 1000 };
    }
    TASK.with(|c| *c.borrow_mut() = Some(t));
}

fn worlds(server: &Server) -> Vec<api::WorldInfo> {
    server
        .get_all_worlds()
        .into_iter()
        .map(|w| api::WorldInfo {
            name: w.get_name(),
            dimension: w.get_dimension(),
            players: server.get_player_count_in_world(w),
        })
        .collect()
}

/// World changes by polling, and the statistics for placeholders.
fn periodic(t: &mut Task, server: &Server) {
    let now = now_ms();
    if t.ticks.is_multiple_of(WORLD_POLL_TICKS) {
        let players = server.get_all_players();
        let current = players.iter().map(|p| (id_of(p), p.get_world().get_dimension()));
        for (id, from, to) in t.worlds.update(current) {
            let (Some(from), Some(p)) = (from, players.iter().find(|p| id_of(p) == id)) else { continue };
            send(t, &Msg::Ev { ev: Event::World { player: id, from, to, pos: pos_of(p) } });
        }
    }
    if now >= t.next_stats.0 {
        t.next_stats.0 = now + t.stats_ms.0;
        let m = Msg::StatsServer { tps: t.tps.tps(), mspt: server.get_mspt() as f32, worlds: worlds(server) };
        send(t, &m);
    }
    if now >= t.next_stats.1 {
        t.next_stats.1 = now + t.stats_ms.1;
        let infos: Vec<(Id, api::PlayerInfo)> =
            server.get_all_players().iter().map(|p| (id_of(p), player_info(p))).collect();
        let players: Vec<wire::PlayerStats> = t
            .player_stats
            .update(infos)
            .into_iter()
            .map(|(player, old, new)| {
                let changed = |f: &dyn Fn(&api::PlayerInfo) -> String| old.as_ref().is_none_or(|o| f(o) != f(&new));
                wire::PlayerStats {
                    player,
                    health: changed(&|i| format!("{:.0}", i.health)).then_some(new.health),
                    food: changed(&|i| i.food.to_string()).then_some(new.food),
                    level: changed(&|i| i.level.to_string()).then_some(new.level),
                    gamemode: changed(&|i| format!("{:?}", i.gamemode)).then_some(new.gamemode),
                    world: changed(&|i| i.pos.world.clone()).then_some(new.pos.world.clone()),
                }
            })
            .filter(|s| {
                s.health.is_some() || s.food.is_some() || s.level.is_some() || s.gamemode.is_some() || s.world.is_some()
            })
            .collect();
        if !players.is_empty() {
            send(t, &Msg::StatsPlayer { players });
        }
    }
}

/// A local PumboPerms has precedence (spec §5.3), unless it stepped back
/// because the proxy's PumboPerms rules (`passive`, PumboPerms spec §16).
fn perms_check(t: &mut Task, server: &Server) {
    let reply = crate::papi::ipc::send_ipc_message("pumboperms", br#"{"op":"hello"}"#);
    let passive = matches!(&reply, Ok(Ok(b)) if serde_json::from_slice::<serde_json::Value>(b).ok().and_then(|v| v.get("passive")?.as_bool()) == Some(true));
    let want = if reply.is_ok() && !passive { PermsMode::Local } else { PermsMode::Bridge };
    let (changes, switched) = PERMS.with(|p| p.borrow_mut().set_mode(want));
    if !switched {
        return;
    }
    apply(Some(server), changes);
    info(match want {
        PermsMode::Local => "a local PumboPerms writes the permissions here; the bridge stopped",
        PermsMode::Bridge => "no local PumboPerms: permissions come from the proxy",
    });
    send(t, &Msg::Ev { ev: Event::PermsMode { mode: want } });
}

// ---------------------------------------------------------------- commands

type Reply = Result<Value, (&'static str, Option<String>)>;

fn ok<T: serde::Serialize>(v: &T) -> Reply {
    wire::value(v).map_err(|e| (err::FAILED, Some(e)))
}

fn args<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, (&'static str, Option<String>)> {
    wire::from_value(v).map_err(|e| (err::BAD_ARGS, Some(e)))
}

fn player(server: &Server, id: Id) -> Result<Player, (&'static str, Option<String>)> {
    server.get_player_by_uuid(uuid(id)).ok_or((err::NO_PLAYER, None))
}

/// Runs one command; `None` = no answer (none is due). The bool says a
/// `teleport-world` ran (the tick ends after it).
fn run(t: &mut Task, server: &Server, c: &Cmd) -> (Option<Reply>, bool) {
    let mut world_change = false;
    let r = match c.method.as_str() {
        method::TELEPORT => args(&c.args).and_then(|a| teleport(server, a, &mut world_change)),
        method::SET_GAMEMODE => args::<api::SetGamemode>(&c.args).and_then(|a| {
            player(server, a.player)?.set_gamemode(mode_to(a.mode));
            ok(&())
        }),
        method::HEAL => args(&c.args).and_then(|a| heal(server, a)),
        method::EFFECT => args(&c.args).and_then(|a| effect(server, a)),
        method::FLY => args::<api::Fly>(&c.args).and_then(|a| {
            let p = player(server, a.player)?;
            p.set_allow_flight(a.allow);
            p.set_flying(a.allow && a.flying.unwrap_or(p.is_flying()));
            if let Some(s) = a.speed {
                p.set_fly_speed(s);
            }
            ok(&())
        }),
        method::INV_GET => args::<api::InvGet>(&c.args).and_then(|a| {
            let p = player(server, a.player)?;
            let items: Vec<api::SlotItem> = (0..a.part.size())
                .filter_map(|slot| Some(api::SlotItem { slot, item: item_of(&get_slot(&p, a.part, slot)?) }))
                .collect();
            ok(&items)
        }),
        method::INV_SET => args::<api::InvSet>(&c.args).and_then(|a| {
            let p = player(server, a.player)?;
            if a.slot >= a.part.size() {
                return Err((err::BAD_ARGS, Some(format!("slot {} out of range", a.slot))));
            }
            set_slot(&p, a.part, a.slot, a.item.as_ref().map(stack_of));
            ok(&())
        }),
        method::INV_GIVE => args::<api::InvGive>(&c.args).and_then(|a| {
            let p = player(server, a.player)?;
            ok(&api::Given { leftover: give(&p, &a.item) })
        }),
        method::INV_CLEAR => args::<api::InvClear>(&c.args).and_then(|a| {
            let p = player(server, a.player)?;
            let inv = p.get_inventory();
            match a.part {
                None => {
                    inv.clear_all();
                    p.clear_ender_chest();
                }
                Some(api::Part::Main) => inv.clear_main(),
                Some(api::Part::Armor) => inv.clear_armor(),
                Some(api::Part::Offhand) => inv.set_off_hand(None),
                Some(api::Part::Ender) => p.clear_ender_chest(),
            }
            ok(&())
        }),
        method::SHOW_ITEMS => args(&c.args).and_then(|a| show_items(server, a)),
        method::PERM_SET => {
            if let Ok(a) = args::<api::PermSet>(&c.args) {
                let online = server.get_player_by_uuid(uuid(a.player)).is_some();
                let change = PERMS.with(|p| p.borrow_mut().perm_set(a, online, now_ms()));
                apply(Some(server), change.into_iter().collect());
            }
            return (None, false);
        }
        method::PERMS_EXPORT => {
            if let Ok(a) = args::<api::PermsExport>(&c.args) {
                save_export(&a);
                PERMS.with(|p| p.borrow_mut().set_export(a));
            }
            return (None, false);
        }
        method::Q_PLAYER => args::<api::QPlayer>(&c.args).and_then(|a| ok(&player_info(&player(server, a.player)?))),
        method::Q_SERVER => ok(&api::ServerInfo {
            tps: t.tps.tps(),
            mspt: server.get_mspt() as f32,
            players: server.get_player_count(),
            worlds: worlds(server),
        }),
        method::Q_SPAWN => {
            args::<api::QSpawn>(&c.args).and_then(|a| ok(&spawn_of(server, a.world.as_deref().unwrap_or(OVERWORLD))?))
        }
        method::Q_ENTITIES => args::<api::QEntities>(&c.args).and_then(|a| {
            if now_ms().saturating_sub(t.last_entities_ms) < 1000 {
                return Err((err::BUSY, None));
            }
            t.last_entities_ms = now_ms();
            let counts: BTreeMap<String, u32> = server
                .get_all_worlds()
                .iter()
                .filter(|w| a.world.as_ref().is_none_or(|x| *x == w.get_dimension()))
                .map(|w| (w.get_dimension(), u32::try_from(w.get_entities().len()).unwrap_or(u32::MAX)))
                .collect();
            ok(&counts)
        }),
        _ => Err((err::UNSUPPORTED, None)),
    };
    (Some(r), world_change)
}

fn teleport(server: &Server, a: api::Teleport, world_change: &mut bool) -> Reply {
    let p = player(server, a.player)?;
    let to = match a.to {
        api::Target::Pos(pos) => pos,
        api::Target::Player(other) => pos_of(&player(server, other)?),
        api::Target::Spawn(w) => spawn_of(server, w.as_deref().unwrap_or(OVERWORLD))?,
    };
    let here = p.get_world();
    let xyz = (to.x, to.y, to.z);
    if to.world == here.get_dimension() {
        p.teleport(xyz, to.yaw, to.pitch, here);
    } else {
        let target = server
            .get_world_by_name(&to.world)
            .ok_or_else(|| (err::BAD_ARGS, Some(format!("no world {}", to.world))))?;
        *world_change = true;
        p.teleport_world(target, xyz, to.yaw, to.pitch);
    }
    ok(&())
}

fn spawn_of(server: &Server, world: &str) -> Result<Pos, (&'static str, Option<String>)> {
    let w = server.get_world_by_name(world).ok_or_else(|| (err::BAD_ARGS, Some(format!("no world {world}"))))?;
    let s = w.get_spawn_location();
    // Pumpkin keeps a spawn high in the air until a player spawned there;
    // stand on the highest block instead.
    let top = w.get_top_block_y(s.pos.x, s.pos.z) + 1;
    Ok(Pos {
        world: w.get_dimension(),
        x: f64::from(s.pos.x) + 0.5,
        y: f64::from(s.pos.y.min(top)),
        z: f64::from(s.pos.z) + 0.5,
        yaw: Some(s.yaw),
        pitch: Some(s.pitch),
    })
}

fn heal(server: &Server, a: api::Heal) -> Reply {
    let p = player(server, a.player)?;
    // Never to 0: a death inside our call would run handlers of this plugin.
    let max = p.get_max_health();
    p.set_health(a.health.unwrap_or(max).clamp(0.5, max));
    p.set_food_level(a.food.unwrap_or(20).min(20));
    p.set_saturation(a.saturation.unwrap_or(5.0));
    if a.extinguish {
        p.as_entity().set_fire_ticks(0);
        p.set_freeze_ticks(0);
    }
    ok(&())
}

fn effect(server: &Server, a: api::Effect) -> Reply {
    let p = player(server, a.player)?;
    let kind = || {
        let id = a.id.as_deref().ok_or((err::BAD_ARGS, Some("id needed".into())))?;
        effect_type(id).ok_or_else(|| (err::BAD_ARGS, Some(format!("unknown effect {id}"))))
    };
    match a.op {
        api::EffectOp::Add => p.add_effect(StatusEffectInstance {
            effect_type: kind()?,
            duration: a.seconds.unwrap_or(30).saturating_mul(20),
            amplifier: a.amplifier.unwrap_or(0),
            ambient: false,
            show_particles: a.particles.unwrap_or(true),
            show_icon: true,
        }),
        api::EffectOp::Remove => p.remove_effect(kind()?),
        api::EffectOp::Clear => p.clear_effects(),
    }
    ok(&())
}

macro_rules! effects {
    ($($v:ident = $n:literal),* $(,)?) => {
        fn effect_type(id: &str) -> Option<StatusEffectType> {
            Some(match crate::bare(id) {
                $($n => StatusEffectType::$v,)*
                _ => return None,
            })
        }
    };
}

effects!(
    Speed = "speed",
    Slowness = "slowness",
    Haste = "haste",
    MiningFatigue = "mining_fatigue",
    Strength = "strength",
    InstantHealth = "instant_health",
    InstantDamage = "instant_damage",
    JumpBoost = "jump_boost",
    Nausea = "nausea",
    Regeneration = "regeneration",
    Resistance = "resistance",
    FireResistance = "fire_resistance",
    WaterBreathing = "water_breathing",
    Invisibility = "invisibility",
    Blindness = "blindness",
    NightVision = "night_vision",
    Hunger = "hunger",
    Weakness = "weakness",
    Poison = "poison",
    Wither = "wither",
    HealthBoost = "health_boost",
    Absorption = "absorption",
    Saturation = "saturation",
    Glowing = "glowing",
    Levitation = "levitation",
    Luck = "luck",
    Unluck = "unluck",
    SlowFalling = "slow_falling",
    ConduitPower = "conduit_power",
    DolphinsGrace = "dolphins_grace",
    BadOmen = "bad_omen",
    HeroOfTheVillage = "hero_of_the_village",
    Darkness = "darkness",
    TrialOmen = "trial_omen",
    RaidOmen = "raid_omen",
    WindCharged = "wind_charged",
    Weaving = "weaving",
    Oozing = "oozing",
    Infested = "infested",
);

fn player_info(p: &Player) -> api::PlayerInfo {
    api::PlayerInfo {
        pos: pos_of(p),
        gamemode: mode_of(p.get_gamemode()),
        health: p.get_health(),
        max_health: p.get_max_health(),
        food: p.get_food_level(),
        saturation: p.get_saturation(),
        level: p.get_experience_level(),
        xp: p.get_experience_progress(),
        flying: p.is_flying(),
    }
}

// ponytail: id, count, name and lore only; enchantments and `raw`
// components wait for a Pumpkin API that names them (spec §4.1).
fn item_of(s: &ItemStack) -> api::Item {
    api::Item {
        id: crate::namespaced(&s.get_registry_key()),
        count: s.get_count(),
        name: s.get_custom_name().map(|n| n.to_json()),
        lore: s.get_lore().iter().map(|l| l.to_json()).collect(),
        ench: Vec::new(),
        dmg: None,
        raw: None,
    }
}

fn stack_of(i: &api::Item) -> ItemStack {
    let s = ItemStack::new(&crate::namespaced(&i.id), i.count);
    if let Some(n) = i.name.as_deref().and_then(|j| TextComponent::from_json(j).ok()) {
        s.set_custom_name(Some(n));
    }
    if !i.lore.is_empty() {
        s.set_lore(i.lore.iter().filter_map(|j| TextComponent::from_json(j).ok()).collect());
    }
    s
}

fn get_slot(p: &Player, part: api::Part, slot: u8) -> Option<ItemStack> {
    let inv = p.get_inventory();
    match (part, slot) {
        (api::Part::Main, s) => p.get_inventory_item(s),
        (api::Part::Armor, 0) => inv.get_helmet(),
        (api::Part::Armor, 1) => inv.get_chestplate(),
        (api::Part::Armor, 2) => inv.get_leggings(),
        (api::Part::Armor, _) => inv.get_boots(),
        (api::Part::Offhand, _) => inv.get_off_hand(),
        (api::Part::Ender, s) => p.get_ender_chest_item(s),
    }
}

fn set_slot(p: &Player, part: api::Part, slot: u8, item: Option<ItemStack>) {
    let inv = p.get_inventory();
    match (part, slot) {
        (api::Part::Main, s) => p.set_inventory_item(s, item),
        (api::Part::Armor, 0) => inv.set_helmet(item),
        (api::Part::Armor, 1) => inv.set_chestplate(item),
        (api::Part::Armor, 2) => inv.set_leggings(item),
        (api::Part::Armor, _) => inv.set_boots(item),
        (api::Part::Offhand, _) => inv.set_off_hand(item),
        (api::Part::Ender, s) => p.set_ender_chest_item(s, item),
    }
}

/// Fills matching stacks first, then empty main slots; returns the rest.
fn give(p: &Player, item: &api::Item) -> u8 {
    let id = crate::namespaced(&item.id);
    let mut left = item.count;
    let plain = item.name.is_none() && item.lore.is_empty();
    for slot in 0..36u8 {
        if left == 0 {
            break;
        }
        if let Some(s) = p.get_inventory_item(slot)
            && plain
            && crate::namespaced(&s.get_registry_key()) == id
            && s.get_custom_name().is_none()
        {
            let room = s.get_max_count().saturating_sub(s.get_count()).min(left);
            if room > 0 {
                s.set_count(s.get_count() + room);
                p.set_inventory_item(slot, Some(s));
                left -= room;
            }
        }
    }
    for slot in 0..36u8 {
        if left == 0 {
            break;
        }
        if p.get_inventory_item(slot).is_none() {
            let s = stack_of(item);
            let n = left.min(s.get_max_count().max(1));
            s.set_count(n);
            p.set_inventory_item(slot, Some(s));
            left -= n;
        }
    }
    left
}

fn show_items(server: &Server, a: api::ShowItems) -> Reply {
    let p = player(server, a.viewer)?;
    let screen = match a.rows {
        0 | 1 => Screen::Generic9x1,
        2 => Screen::Generic9x2,
        3 => Screen::Generic9x3,
        4 => Screen::Generic9x4,
        5 => Screen::Generic9x5,
        _ => Screen::Generic9x6,
    };
    let title = TextComponent::from_json(&a.title).unwrap_or_else(|_| TextComponent::text(&a.title));
    let gui = crate::papi::gui::Gui::new(screen, title);
    // Copies only, nothing can be taken or put in.
    gui.set_allow_grab_items(false);
    gui.set_allow_put_items(false);
    for s in &a.items {
        gui.set_item(u32::from(s.slot), stack_of(&s.item));
    }
    p.open_gui(gui);
    ok(&())
}

fn save_export(e: &api::PermsExport) {
    let dir = DIR.with(|d| d.borrow().clone());
    if let Ok(json) = serde_json::to_string(e) {
        let _ = std::fs::write(format!("{dir}/perms-export.json"), json);
    }
}

fn load_export(dir: &str) -> Option<api::PermsExport> {
    serde_json::from_str(&std::fs::read_to_string(format!("{dir}/perms-export.json")).ok()?).ok()
}

// ---------------------------------------------------------------- events

struct Ping;
impl EventHandler<ServerListPingEvent> for Ping {
    fn handle(&self, _s: Server, e: EventData<ServerListPingEvent>) -> EventData<ServerListPingEvent> {
        if let Some(n) = wire::ping_nonce(&e.hostname) {
            SEEN.with(|s| {
                let mut s = s.borrow_mut();
                if s.len() < 16 {
                    s.push(n.to_string());
                }
            });
        }
        e
    }
}

struct Join;
impl EventHandler<PlayerJoinEvent> for Join {
    fn handle(&self, server: Server, e: EventData<PlayerJoinEvent>) -> EventData<PlayerJoinEvent> {
        let p = &e.player;
        let id = id_of(p);
        // The proxy's set arrived before the player (spec §5.2).
        let change = PERMS.with(|x| x.borrow_mut().joined(id, now_ms()));
        apply(Some(&server), change.into_iter().collect());
        let ops = server
            .get_op_manager()
            .list_ops()
            .into_iter()
            .map(|o| (o.name, Id::from_u64_pair(o.uuid.high, o.uuid.low)))
            .collect();
        let name = p.get_name();
        if let Some(old) = crate::stale_op(&name, &id, ops) {
            warn(&format!(
                "{name} joined with UUID {id} (given by the proxy, by its online mode), but ops.json lists {name} under {old}: Pumpkin matches operators by UUID, so {name} is no operator here; run `op {name}` in this console"
            ));
        }
        queue_event(Event::Join { player: id, name, pos: pos_of(p) });
        e
    }
}

struct Leave;
impl EventHandler<PlayerLeaveEvent> for Leave {
    fn handle(&self, server: Server, e: EventData<PlayerLeaveEvent>) -> EventData<PlayerLeaveEvent> {
        let id = id_of(&e.player);
        let change = PERMS.with(|x| x.borrow_mut().left(id));
        if let Some(c) = change {
            for n in &c.unset {
                e.player.unset_permission(n);
            }
        }
        let _ = server;
        queue_event(Event::Leave { player: id, pos: pos_of(&e.player) });
        e
    }
}

struct Death;
impl EventHandler<PlayerDeathEvent> for Death {
    fn handle(&self, _s: Server, e: EventData<PlayerDeathEvent>) -> EventData<PlayerDeathEvent> {
        queue_event(Event::Death {
            player: id_of(&e.player),
            pos: pos_of(&e.player),
            message: e.death_message.to_json(),
            killer: None,
        });
        e
    }
}

struct Respawn;
impl EventHandler<PlayerRespawnEvent> for Respawn {
    fn handle(&self, _s: Server, e: EventData<PlayerRespawnEvent>) -> EventData<PlayerRespawnEvent> {
        let (x, y, z) = e.position;
        let pos = Pos { world: e.respawned_world.get_dimension(), x, y, z, yaw: Some(e.yaw), pitch: Some(e.pitch) };
        queue_event(Event::Respawn { player: id_of(&e.player), pos });
        e
    }
}

/// `/tp`, `/xp`, `/banip`, `/pardonip` with the permission of their command,
/// which Pumpkin does not check (`pumbo_common::pumpkin::UNGUARDED_ALIASES`).
struct AliasGuard;
impl EventHandler<PlayerCommandSendEvent> for AliasGuard {
    fn handle(&self, _s: Server, mut e: EventData<PlayerCommandSendEvent>) -> EventData<PlayerCommandSendEvent> {
        if !e.cancelled
            && let Some(node) = pumbo_common::pumpkin::unguarded_alias(&e.command)
            && !e.player.has_permission(node)
        {
            e.cancelled = true;
            let t = TextComponent::translate("command.unknown.command", Vec::new()).color_named(NamedColor::Red);
            e.player.send_system_message(t, false);
        }
        e
    }
}

// ---------------------------------------------------------------- plugin

pub struct PumboBridge;

impl Plugin for PumboBridge {
    fn new() -> Self {
        PumboBridge
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: crate::PLUGIN_NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: vec!["Patryk Skoczylas".into()],
            description: "PumboBridge: the server half of PumboProx".into(),
            dependencies: vec![],
            permissions: vec![
                permissions::NETWORK_TCP_CONNECT.into(),
                permissions::FS_READ_DATA.into(),
                permissions::FS_WRITE_DATA.into(),
            ],
        }
    }

    fn on_load(&self, context: Context) -> Result<(), String> {
        let _ = started();
        let dir = context.get_data_folder();
        let path = format!("{dir}/config.yml");
        let (text, problem) = pumbo_common::config::read_or_create(&path, pumbo_bridge_core::DEFAULT_CONFIG);
        let (cfg, warnings) = pumbo_bridge_core::parse_config(&text);
        for w in problem.iter().map(|w| w.to_string()).chain(warnings) {
            warn(&format!("config.yml: {w}"));
        }
        let cfg = cfg.map_err(|e| format!("{path}: {e}"))?;
        let ([a, b, c, d], port) = cfg.proxy;
        if cfg.key.is_none() {
            warn(&format!(
                "no key in {path}: idle. Run `pumbo bridge key` in the PumboProx console and put the key there"
            ));
        } else if !pumbo_bridge_core::is_private(cfg.proxy.0) {
            warn(&format!(
                "proxy {a}.{b}.{c}.{d}:{port} is outside loopback and private networks; bridge frames are signed, not encrypted: use a tunnel"
            ));
        }
        if let Some(e) = load_export(&dir) {
            PERMS.with(|p| p.borrow_mut().set_export(e));
        }
        DIR.with(|d| *d.borrow_mut() = dir);
        SERVER.with(|s| *s.borrow_mut() = Some(context.get_server()));
        INSTANCE.with(|i| *i.borrow_mut() = random_bytes());
        CFG.with(|c| *c.borrow_mut() = Some(cfg));
        TASK.with(|t| {
            *t.borrow_mut() = Some(Task {
                net: Net::Idle { retry_at_ms: 0 },
                backoff: Backoff::default(),
                queue: VecDeque::new(),
                tps: TpsMeter::default(),
                ticks: 0,
                stats_ms: (5000, 2000),
                next_stats: (0, 0),
                worlds: Changes::default(),
                player_stats: Changes::default(),
                next_perms_check: 0,
                last_entities_ms: 0,
                last_log: String::new(),
            })
        });
        // Spec §3.6: nothing our own calls fire (crate::SUBSCRIBED).
        context.register_event_handler::<ServerListPingEvent, _>(Ping, EventPriority::Normal, false)?;
        context.register_event_handler::<PlayerJoinEvent, _>(Join, EventPriority::Lowest, true)?;
        context.register_event_handler::<PlayerLeaveEvent, _>(Leave, EventPriority::Lowest, false)?;
        context.register_event_handler::<PlayerDeathEvent, _>(Death, EventPriority::Lowest, false)?;
        context.register_event_handler::<PlayerRespawnEvent, _>(Respawn, EventPriority::Lowest, false)?;
        context.register_event_handler::<PlayerCommandSendEvent, _>(AliasGuard, EventPriority::Highest, true)?;
        context.schedule_repeating_task(1, 1, |server| tick(&server));
        info(&format!(
            "PumboBridge {} loaded (Pumpkin {}, Minecraft {}), proxy {a}.{b}.{c}.{d}:{port}",
            env!("CARGO_PKG_VERSION"),
            PUMPKIN.0,
            PUMPKIN.1
        ));
        Ok(())
    }

    fn on_unload(&self, context: Context) -> Result<(), String> {
        // Only non-pumping calls here (spec §2 G3): unset, no refresh.
        let changes = PERMS.with(|p| p.borrow_mut().release_all(false));
        apply(Some(&context.get_server()), changes);
        REFRESH.with(|r| r.borrow_mut().clear());
        TASK.with(|t| *t.borrow_mut() = None);
        Ok(())
    }

    fn handle_ipc_message(&self, _sender: String, message: Vec<u8>) -> Result<Vec<u8>, String> {
        let before = PERMS.with(|p| p.borrow().mode());
        let (reply, mut changes) = PERMS.with(|p| p.borrow_mut().ipc(&message, env!("CARGO_PKG_VERSION")));
        let after = PERMS.with(|p| p.borrow().mode());
        SERVER.with(|s| {
            let s = s.borrow();
            // What a stepping-back PumboPerms left on players online goes now.
            let online: Vec<Id> =
                s.as_ref().map(|s| s.get_all_players().iter().map(id_of).collect()).unwrap_or_default();
            PERMS.with(|p| {
                let mut p = p.borrow_mut();
                changes.extend(
                    p.leftover_players().into_iter().filter(|u| online.contains(u)).filter_map(|u| p.leftover(u)),
                );
            });
            // Unset and set do not pump; the command lists follow from the
            // task, never inside another plugin's call.
            apply(s.as_ref(), changes);
        });
        if before != after {
            info(match after {
                PermsMode::Local => "PumboPerms took over the permissions here; the bridge stopped writing them",
                PermsMode::Bridge => {
                    "PumboPerms stepped back (the proxy's PumboPerms rules); the bridge writes the permissions"
                }
            });
            queue_event(Event::PermsMode { mode: after });
        }
        Ok(reply)
    }
}

crate::papi::register_plugin!(PumboBridge);

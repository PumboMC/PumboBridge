# PumboBridge

PumboBridge is the server half of the [PumboProx](https://github.com/PumboMC/PumboProx) proxy on [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) servers. Put the same file and the same `config.yml` on every server behind one proxy: the bridge connects to the proxy by itself, finds out which server it is, and from then on the proxy and its plugins can teleport players, change game modes, heal, read and change inventories and ask about players and the server. It also brings the proxy's ranks to the server: who may use `/gamemode` on `survival` is decided in the proxy's `permissions.yml` or by PumboPerms on the proxy.

The bridge has no commands and keeps no player data apart from a copy of the proxy's permission export for PumboPerms (`perms-export.json`). It sends a player a message only when it refuses one of the aliases Pumpkin leaves open (below); everything else it does at the proxy's request. It is written in Rust and runs as a WebAssembly plugin. The protocol logic lives in `pumbo-bridge-core`, the contract (messages, frames, service types) in `pumbo-bridge-proto` of PumboProx.

## Compatibility

| File | Pumpkin | Minecraft |
| --- | --- | --- |
| `PumboBridge-26.3.wasm` | release `0.2.0+26.3-26.51` | 26.3 |
| `PumboBridge-26.2.wasm` | release `0.1.0-dev+26.2-26.45` | 26.2 |

A new Pumpkin version needs a new build of the bridge only; a new Minecraft version for players changes only the proxy.

## Installation

1. **Proxy.** PumboProx runs its side of the bridge by default: it listens on `127.0.0.1:25578` and creates `bridge.key` at its first start. `prox bridge key` in the proxy console shows the key (in game `/prox bridge key`, with `pumbo.proxy.bridge.key`). If the port is taken, the proxy does not start: set `bridge.listen`, or `bridge.enabled: false` for a proxy without bridges. Other settings go into `pumboprox.yml`:

   ```yaml
   bridge:
     listen: "127.0.0.1:25578"     # where bridges connect
     trusted-plugins: [pumbo-core]  # proxy plugins that may send commands
   ```

   A server you create with `/prox servers new` gets PumboBridge from its latest release (checked against `SHA256SUMS`) and a `config.yml` with the address and the key, so steps 2 and 3 are for servers you set up yourself. Without network, or when the checksum does not match, the proxy creates the server without the bridge and the log says where to put the file.
2. **Each server.** Copy the matching `.wasm` file into `plugins/` and start the server once. The bridge writes `plugins/data/pumbobridge/config.yml`; put the proxy's address and the key there and restart the server:

   ```yaml
   proxy: 127.0.0.1:25578       # bridge.listen of PumboProx (ip:port, no names)
   key: "<64 hex digits from `prox bridge key`>"
   ```

3. **Pumpkin's plugin sandbox.** The bridge asks for `network.tcp.connect`, `fs.read.data` and `fs.write.data`. With `ask_permission_confirmation = true` (Pumpkin's default) the first start asks in the console; to approve it in advance use `allowed_permissions = ["network.tcp.connect", "fs.read.data", "fs.write.data"]` in `[plugins]` of `pumpkin.toml`. Keep `loopback_only = true` for all plugins when the proxy runs on the same machine. When the proxy runs on another machine, open only the bridge:

   ```toml
   [plugins]
   loopback_only = true

   [plugins.overrides.pumbobridge]
   loopback_only = false
   ```

4. **Check.** `/prox bridge` in game (permission `pumbo.proxy.bridge`) or `prox bridge` in the proxy console lists every server with the state of its bridge (`connected`, `lost`, `none` or `rejected`), the versions of the bridge and its contract, the ping and since when.

The connection is signed with the key (every frame has a sequence number and an HMAC), not encrypted, like Velocity forwarding: keep it on 127.0.0.1, a private network or a tunnel (WireGuard, Tailscale). The bridge warns about a public proxy address.

If the connection drops, the bridge logs `lost the proxy connection: <reason>, reconnecting` and tries again after 1, 2, 5 and 10 s, then every 30 s, each wait up to a fifth longer at random. A failed attempt logs `not connected to the proxy: <reason>; retrying` once per reason. When the other end cannot prove the key (a wrong key, or not PumboProx on that port), the bridge waits 30 s before the next attempt. Back online it logs `connected to PumboProx as server <name>` and gets the ranks of the players on the server again; until then they keep the ranks they had. The proxy logs every close with its reason, for example `bridge on survival closed: silent for 15 s (no answer to pings)`.

While the server's ticks are frozen (`/tick freeze`), Pumpkin runs no plugin tasks. When the bridge's task has not run for 2 s, the bridge talks to the proxy from its event handlers, so it answers only when something happens on the server: a player command, a join or leave, a server list ping. The proxy drops the bridge of a frozen server where nothing happens for 15 s; the bridge reconnects at the next such event or after `/tick unfreeze`. The log says when the bridge sees frozen ticks and when they run again. Commands from the proxy wait for the ticks: the proxy answers the plugin that sent one with `timeout` after `command-timeout-ms`, and the bridge answers a command that waited more than 30 s with `timeout` instead of running it.

## Configuration

`config.yml` (the same on every server; changes need a server restart):

| Option | Meaning |
| --- | --- |
| `proxy` | IPv4 address and port of `bridge.listen` in `pumboprox.yml`. |
| `key` | The bridge key (`prox bridge key` in the proxy console). Empty: the bridge stays idle. |

The bridge keeps a copy of the file it wrote in `.generated/`. After an update, a `config.yml` nobody changed follows the new version, unless the new version changed a default value: then the file stays and the log names that option. A changed file stays as it is, and the log names the options the new version added. A deleted file comes back.

Proxy side (`bridge:` in `pumboprox.yml`): `enabled` (on by default), `listen` (`127.0.0.1:25578`), `key-file` (`bridge.key`), `command-timeout-ms` (5000), `trusted-plugins` (`[pumbo-core]`), `stats-ms` (5000) and `player-stats-ms` (2000) for the placeholders below.

## Ranks from the proxy

When a player is about to join a server, the proxy sends the bridge the player's decisions for that server, and sends them again when the player joins: the permissions of Pumpkin's commands and of the Pumbo plugins (`minecraft:command.gamemode`, `pumbobans:ban`, ...), every namespaced node and namespace wildcard of the player and `group.<name>` for each of the player's groups, resolved with the server's contexts (`server` > `group` > `global`). The bridge writes them as Pumpkin permission attachments and sends the player the command list again; when the player leaves, it takes them back. With PumboPerms on the proxy the set also carries the player's rank, prefix, suffix, groups and meta, which a PumboPerms on the server reads from the bridge (`player-info`). Example `permissions.yml` of the proxy:

```yaml
groups:
  builder:
    permissions: ["-minecraft:command.gamemode"]
    server:
      creative: { permissions: ["minecraft:command.gamemode"] }
players:
  069a79f4-44e9-4726-a5be-fca90e38aaf5:
    name: Notch
    groups: [builder]
```

A node without a decision is left to Pumpkin's operator levels, and so is everything before the player's set arrives: give players whose ranks the proxy manages operator level 0. Pumpkin matches operators by UUID, and behind the proxy the UUID follows the proxy's online mode: when `ops.json` lists a joining player's name under another UUID, the bridge logs a warning with the `op <name>` command to run.

**A local PumboPerms has precedence.** Only one plugin may write attachments on a server. When PumboPerms is installed, it imports the proxy's table (only the copy the bridge got from the proxy in its current session; see the PumboPerms README, `/pp import`), asks the bridge to release its attachments and writes them alone; the bridge stops writing and logs it, and the proxy log says `<server>: ranks from the local PumboPerms`. When PumboPerms is removed, the bridge writes them again. Only the plugin `pumboperms` may switch the bridge this way.

**Unless the proxy's PumboPerms rules.** With PumboPerms on PumboProx, the proxy's export holds only the flag that it rules (`proxy-rules`); a local PumboPerms then steps back: it hands its attachments to the bridge (`take-permissions`), the bridge takes them off (players offline now at their next join, before the proxy's set) and writes the proxy's decisions. Ranks are then set with `/pp` on the proxy, with `server=<name>` for this server.

**Aliases Pumpkin leaves open.** Pumpkin 0.2.0 and 0.1.0-dev run `/tp`, `/xp`, `/banip` and `/pardonip` without any permission check (the alias of a command without an executor of its own gets no requirement; fixed upstream after 0.2.0, Pumpkin #3801), so on a plain server every player may teleport, give experience and ban IPs. The bridge checks these four, typed directly or after `run` in `/execute`, against the permission of their command (`minecraft:command.teleport`, `.experience`, `.banip`, `.pardonip`: operator level 2 or 3, or a rank) and answers a player without it like an unknown command.

## For proxy plugins

The proxy provides the service `pumbo:bridge@1.0`. Declare it and call it with the typed client of `pumbo-bridge-proto` (feature `sdk`):

```yaml
uses:
  - { service: "pumbo:bridge", version: "1.0", required: false }
```

```rust
use pumbo_bridge_proto::{api, client::Bridge};
let r = Bridge::client().set_gamemode(&api::SetGamemode { player, mode: api::GameMode::Creative, server: None }).await;
```

Commands (`teleport`, `set-gamemode`, `heal`, `effect`, `fly`, `inv-set`, `inv-give`, `inv-clear`, `show-items`, `send-to`) are for `trusted-plugins` only; queries (`q-player`, `q-server`, `q-spawn`, `q-entities`, `inv-get`, `status`) for every plugin that uses the service. Without `server` a call goes to the server the player is on. Errors are `service-error::rejected` with a code: `no-bridge` (no bridge on that server, at once), `no-player`, `unsupported`, `timeout`, `disconnected`, `expired` and `superseded` (teleports), `not-allowed`, `bad-args`, `offline`, `busy` (`q-entities` more than once a second, or a full queue to the bridge) and `failed`.

A teleport waits in the proxy until the client confirmed the previous one (the spawn on joining, a teleport of Pumpkin or another plugin), so a second teleport does not kick the player with "Wrong teleport id". `send-to` answers `no-bridge` before the player is moved when the target has no bridge, and runs its actions after the player arrived and confirmed the spawn position.

The proxy publishes `pumbo:bridge-event@1.0` (join, leave, death, respawn, world change, PumboPerms mode) and `pumbo:bridge-status@1.0`, and fills the placeholders `%server_tps:<server>%`, `%server_mspt:<server>%`, `%player_health%`, `%player_food%`, `%player_level%`, `%player_world%` and `%player_gamemode%` (`?` without a bridge).

## Not in this version

- Land protection: the future PumboGuard plugin will send its rules through the bridge (`guard-rules` is reserved in the contract).
- `kill` and console commands: Pumpkin's plugin API gives no console command sender to a task, and `kill` fires an event inside the call that would come back to the bridge.
- Enchantments and exact item components in inventory calls: items carry their id, count, name and lore.

## Building

```sh
./build.sh          # dist/PumboBridge-26.3.wasm and dist/PumboBridge-26.2.wasm
cargo test -p pumbo-bridge-core -p pumbo-bridge-pumpkin
```

`pumbo-bridge-proto` comes from [PumboProx](https://github.com/PumboMC/PumboProx) as a git dependency.

## License

GPL-3.0-only.

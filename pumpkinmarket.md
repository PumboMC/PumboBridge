# PumboBridge

Connects your Pumpkin servers to the [PumboProx](https://github.com/PumboMC/PumboProx) proxy. **It does nothing on its own: install it only on servers behind PumboProx.**

The proxy moves players between servers by itself. The bridge lets it and its plugins act inside a server as well: teleport within the world, change game modes, heal, read and change inventories. It also brings the proxy's ranks to every server.

## Features

- **Teleports** that wait for the client's confirmation, so nobody is kicked with "Wrong teleport id".
- **Player actions:** game mode, health, effects and flight.
- **Inventory:** read, set, give and clear items, or open a read-only view.
- **Ranks** from the proxy on every server, with server contexts.
- **Placeholders** such as `%server_tps:<server>%` and `%player_health%`.
- **One file and config for every server.** The bridge finds out which server it is by itself.
- **Signed messages** that cannot be faked or replayed. It reconnects by itself when the proxy restarts.

## Installation

1. Turn the bridge on in `pumboprox.yml` (`bridge: enabled: true`) and restart the proxy. `bridge key` in the proxy console shows the key.
2. Put this file into `plugins/` of every server and start it once. Put the proxy's bridge address and the key into `plugins/data/pumbobridge/config.yml` and restart.
3. Approve `network.tcp.connect`, `fs.read.data` and `fs.write.data` when Pumpkin asks, or add them to `allowed_permissions` in `pumpkin.toml`.
4. `/pumbo bridge` on the proxy lists every server and its bridge.

Keep the connection on `127.0.0.1`, a private network or a tunnel: it is signed, not encrypted. Works with Pumpkin 0.2.0 (Minecraft 26.3) and PumboProx 0.1.1-beta.

---

PumboBridge is in beta. Try it on a test network before you put players on it.
Source, documentation and issues: https://github.com/PumboMC/PumboBridge (GPL-3.0)

[![PumboProx: everything you need to run a network on Pumpkin](assets/pumboprox.webp)](https://github.com/PumboMC/PumboProx)

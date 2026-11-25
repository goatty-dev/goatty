# goatty over the tailnet (#15)

**Status:** implemented (protocol v12, `goatty-net`). Not measured on a real tailnet yet (see "Performance").

Another computer on the user's tailnet that runs goatty can be paired in the app and then opened like any host, without SSH. goatty uses **the Tailscale the user installed and logged in to**. It never installs, logs in, updates or runs a node of its own, and it never runs sudo. goattyd binds no network socket (D5 in [`remote-architecture.md`](remote-architecture.md) still holds). SSH stays the way in for anyone without Tailscale.

## Using it

On the computer to reach (it runs goatty already):

1. Settings → Network → **Make this computer available** (or `goattyd net enable`). goatty adds one `tailscale serve` TCP route.
2. **Allow a new device** (or `goattyd net pair`): the pairing window opens for 5 minutes.

On the other computer: Ctrl+Shift+H lists the tailnet's computers that answer on goatty's port. Pick one, choose **Pair**, compare the 6-digit code with the first screen, and confirm on both. From then on it is a host like any other (sessions, tabs, reconnect, shell setup sync) under its Tailscale name.

Settings → Network also shows Tailscale's state, the port, the paired devices (revoke), and the hosts this computer paired with (forget). `goattyd net status|enable [--port N]|disable|pair|devices|revoke KEY|leave` does the host side from a shell.

## How it works

```text
 client                                              host
 GUI ── Endpoint::Tailnet ──► tailscaled ═WireGuard═► tailscaled ── serve --tcp ──► goattyd gateway socket
        TCP to the peer's tailnet IP                  (the user's own)               (0600, next to goattyd.sock)
        (or `tailscale nc` in userspace mode)                                         handshake, then the protocol
```

- **Only Tailscale's public CLI** (`goatty_net::tailscale`): `status --json`, `serve`, `serve status --json`, `nc`. Not the LocalAPI, since goatty doesn't pin a Tailscale version.
- **Inbound.** `tailscale serve --bg --tcp=<port> unix:<runtime dir>/goatty/goattyd.gateway.sock`. tailscaled runs as root, so it can open the user's 0600 socket in its 0700 directory. goattyd listens there only while the host is available (`goatty_net::host`).
- **Inbound on Windows.** Serve can't forward into a Unix socket or a named pipe on Windows, so the gateway listens on a loopback port instead and the route is `tailscale serve --bg --tcp=<port> tcp://127.0.0.1:<local port>` (`tailscale::Target::Loopback`). The local port is saved in `state.json` and bound again on start, so the route stays valid across restarts. If something else took it, the gateway moves to a new port and repoints its route (a route to the old port still counts as goatty's). A second goattyd with the same device key that finds the port answering with its own key refuses to listen. Each authenticated connection is bridged into a named pipe pair and served by `client::handle` like any other (`goatty-daemon/src/net.rs`). This is the one network socket goattyd binds, and only on Windows: any local process can connect to it, but none gets past the gateway handshake without a paired device key, so the security gates below are unchanged. A local process could at most read the probe answer (host name, goatty version).
- **Outbound.** With a TUN device (`status --json` has `TUN: true`) the client opens a `TcpStream` to the peer's tailnet IPv4. In userspace-networking mode it runs `tailscale nc <addr> <port>` (`goatty_net::link`).
- **Wire.** The main protocol is unchanged. Every gateway connection starts with the gateway handshake (`goatty_net::gateway`, newline-delimited JSON), then carries the normal protocol and is served by `client::handle` like any other connection. The GUI asks a goattyd about its availability with `Net` (protocol v12, JSON in `goatty_net::api`, like `Shell` in v9), so new operations need no bump. Over an SSH or tailnet link, the settings window manages the *remote* host's availability.
- **Discovery** (`goatty_net::discovery`, `goatty_client::tailnet::discover`) runs when the switcher, the palette or the Network page opens: one `tailscale status --json`, then a probe of goatty's port (at most 16 at once) on every online peer of the same Tailscale user on an OS goattyd runs on. Peers that don't answer as a gateway are left out. Nothing polls. From a tagged node (which belongs to the tailnet, not a person) every peer counts. `tailnet.all_devices` includes other people's and tagged devices.

### Rules for the user's Serve config

It is per node and shared with whatever else the user publishes.

- goatty adds **one route on one port** and removes **only that route** (`serve --tcp=<port> off`). It never runs `serve reset`.
- A route is goatty's if it forwards to goatty's gateway (its socket, or on Windows its loopback port). A port serving anything else is left alone. The settings page says so, and the port can be changed there. **Limit:** discovery probes only the default port (7422), and pairing starts from a discovered host, so a host must be on 7422 while it pairs. After pairing it can move to another port, but then the client needs its new port, and nothing tells it yet. Pair first, then change the port only if 7422 is taken.
- Changing the port adds the new route first, then removes the old one. If the new port is taken, nothing changes.
- On start, goattyd serves again if the host was available (the route still points at the same socket path). Otherwise it removes a leftover route of its own. A route that disappeared from outside (`serve reset`) shows as a problem on the page.
- **Permission.** Tailscale lets a non-root user serve a *Unix socket* only if they are the operator **and** may run `sudo tailscale` (tailscaled checks `sudo -U <user> -l tailscale` itself). Without that, goatty shows `sudo tailscale set --operator=$USER`, plus the exact `sudo tailscale serve …` line as an alternative, and waits. While goatty's Unix route exists, any Serve change on that node needs the same permission, because Tailscale checks the whole config. On Windows there is no operator and no sudo: Serve belongs to the Windows user signed in to Tailscale (or an Administrator), so goatty says that instead, plus the `tailscale serve …` line to run in an Administrator terminal.
- Turning availability off removes the route and closes the gateway socket. `goattyd net leave` also forgets every paired device.

## Security

Two independent gates:

1. **Tailnet reachability**: the user's own ACLs decide who can reach the port at all.
2. **A goatty device key**: each install has an ed25519 key (`$XDG_DATA_HOME/goatty/tailnet/device.key`, 0600, never in `settings.json`, which can be exported). A key file that belongs to another user or that others can read is refused, and without `$HOME` the tailnet is off (never a shared temporary directory). The gateway accepts only a client that signs a fresh challenge with a paired key. The client accepts only a host that signs with the key it pinned at pairing. Every signature covers both keys and both nonces, so nothing can be replayed.

**Pairing** is possible only while the host's user has the window open. Otherwise the gateway answers probes and paired devices only. The code comes from both keys, the host's pairing nonce and the client's nonce. The host **commits** to its pairing nonce in the greeting (a SHA-256 over its key and the nonce) and reveals it only after the client sent its own, and the client checks the reveal against the commitment. Neither side can pick its nonce after seeing the other's, so a device in the middle, which must swap in its own keys, gets matching codes on both screens only by luck (one in a million per attempt). Without the commitment it could search about a million nonces until the codes matched. The host adds the device only after its user confirmed. The client pins the host only after its own user confirmed and the host said yes. `Leave` voids approvals that haven't been saved yet. Every change to `devices.json` happens under one lock.

Limits: 3 waiting requests and 10 attempts per window. After 20 failed authentications in a minute, further failures are refused for the rest of that minute (with a delay), but a paired device's valid signature always gets in, so nobody can lock paired devices out. Revoking deletes the key.

**Limit:** the gate is authentication, not encryption, which WireGuard provides. Someone who could sit in the middle of the WireGuard connection itself (a compromised control server handing out its own node key for the host's address) could relay the handshake and then read the stream. `serve --proxy-protocol` (which would have let the gateway `whois` the peer) is refused by Tailscale for Unix socket targets.

## Files

| Where | What |
|---|---|
| `$XDG_DATA_HOME/goatty/tailnet/device.key` | this install's key (0600) |
| `…/tailnet/devices.json` | devices paired with this host |
| `…/tailnet/hosts.json` | hosts this computer paired with (MagicDNS name, address, port, key) |
| `…/tailnet/state.json` | whether this host is available, and its port (state mirroring the Serve route, not a setting) |
| `$XDG_RUNTIME_DIR/goatty/goattyd.gateway.sock` | the gateway socket (only while available; on Windows a loopback port, `local_port` in `state.json`) |
| `settings.json` `tailnet.discover`, `tailnet.all_devices` | discovery in the switcher (client settings) |

## Findings from building it (phase 0, from Tailscale's source, v1.104.7)

- `serve --tcp` forwards to `unix:` targets since **1.102** (`ipn/ipnlocal/serve.go`: `dialServeUnixSocket`). Older versions show as "too old" (`tailscale::MIN_VERSION`).
- `--tcp` needs no HTTPS certificates or MagicDNS. Only `--https` asks to enable certificates.
- `--proxy-protocol` is refused with Unix targets, so the "same Tailscale user" check by `whois` isn't possible. The device key is the gate.
- Serving a Unix socket needs root, or the operator with sudo rights for `tailscale` (`authorizeServeConfigForGOOSAndUserContext`, `connIsLocalAdmin`). Reading the Serve config and `tailscale nc` need neither.
- tailscaled refuses to forward to its own socket (it would hand out a root LocalAPI). goatty's gateway socket is unrelated.

## Tests

- `goatty-net`: unit tests (status and Serve config parsing, routes, codes, signatures, key files others could read, the handshake: pairing, rejection, a window that runs out, revocation, a host with another key, a reveal that doesn't match its commitment, a decision kept for its own connection, `Leave` voiding an approval in flight, rate limits that don't lock out paired devices) and `tests/host.rs` against a fake `tailscale` (`tests/fake-tailscale.sh`: routes come and go without touching others, port changes, missing permission, logged out, pairing and authentication through the gateway and through the fake `tailscale nc`).
- `goatty-daemon/tests/tailnet.rs`: a sandboxed goattyd with the fake on `PATH` enables, opens pairing over `Net`, a second "computer" with its own key discovers it, pairs over `tailscale nc`, runs a command in a real session over `Endpoint::Tailnet`, gets refused after a revoke, and availability survives `goattyd restart`.
- Windows: `goatty-net` unit tests cover the loopback gateway (listens and answers probes, keeps its port across restarts, moves when the port is taken, refuses to run twice). The fake `tailscale` is a shell script, so `tests/host.rs` and the daemon test run on Unix only.
- Nothing touches the user's Tailscale or daemon. The fake `nc` needs python3 (the test skips without it).
- `cargo run -p goatty-net --example tailscale_status` prints what goatty sees of the real Tailscale (read-only: `status` and `serve status`).

## Performance

Not measured yet: it needs two machines on a tailnet. Measure with

```sh
cargo run --release -p goatty-daemon --example netbench -- tailnet HOST.TAILNET.ts.net   # a paired host
cargo run --release -p goatty-daemon --example netbench -- ssh HOST                      # the same path over SSH
```

and record direct and relayed paths, in TUN and userspace mode, here. Budget from #15:

| Path | Target |
|---|---|
| Echo, direct path | ≤ SSH over the same path + 1 ms |
| Flood, direct path | ≥ SSH over the same path (256 KiB window) |
| Connect to first byte | Below SSH's handshake time |
| Idle | 1 ping per 5 s; no extra process in TUN mode |

## Not done

- No QUIC and no tokio (decided in #15): per-pane streams without head-of-line blocking are the only thing lost, and skip-and-resync bounds that. Several TCP connections per host would be the follow-up if `netbench` over `tc netem` shows a need.
- "Also reach it over your tailnet" from the connect screen of an SSH host: the Network page already manages the active tab's host over its link, so enabling it there works for an SSH host that has Tailscale. There is no separate button on the connect screen.
- No one-command install for a fresh machine: the other computer must already run goatty.

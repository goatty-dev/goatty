# Remote goattyd: architecture and plan

**Date:** 2026-11-07, updated 2026-11-09 · **Status:** R0 implemented (protocol v2, SSH/stdio transports, flow control, reconnect). R1 in progress: version negotiation, goattyd auto-install, plain SSH fallback, SSH prompts in the GUI, the systemd user service, lazy history and compression are done (§4a, §5a, §5b).

**Goal:** the GUI connects to *any* goattyd: on this machine, on a home server over Tailscale, on a cloud box over SSH, or in a container. Rendering stays local on the GPU. Sessions live on the host and survive the client. Local performance must not regress, and remote must stay usable on poor links.

This turns the ideas in [`archive/remote-goattyd-plan.md`](archive/remote-goattyd-plan.md) into concrete decisions and makes them consistent with what Stage 0 measured ([`stage0-findings.md`](stage0-findings.md)).

---

## 1. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | **The GUI is always a client.** Local mode uses the same protocol and code path as remote; `--local` (window-owned PTY) stays only as a debugging fallback. | One code path to keep correct and fast. "Local" is just the transport with the biggest window. |
| D2 | **The daemon's VT is authoritative; the client keeps a mirror VT fed from the byte stream.** No grid diffs on the wire. | See §2. Bytes are smaller than diffs for typical output. Scrollback, selection and search stay local with no round trips. A future web client can feed the same bytes straight into xterm.js. |
| D3 | **Flow control lives in the protocol (acks + window), and a client that falls behind is skipped forward with a `Resync` snapshot.** | Over SSH, the kernel, ssh and TCP buffers hide megabytes, so socket backpressure is meaningless. Skip-and-resync gives mosh's key property, bounded latency on slow links, without mosh's state-sync protocol. |
| D4 | **One connection per host, multiplexing every session** (Stage 0 used one connection per pane). | Each SSH connection costs a handshake (100 ms to 1 s) and a process. Per-pane flow control now comes from per-subscriber windows (D3), not from per-socket backpressure. |
| D5 | **Remote transport = a stdio command running `goattyd proxy` on the host.** `ssh host` is the default. | goatty contains no SSH code, yet gets `~/.ssh/config`, agent, keys, ProxyJump, ControlMaster, known_hosts and Tailscale SSH for free. The same mechanism works for `docker exec`, `kubectl exec` and `distrobox`. The daemon never listens on a network port. |
| D6 | **Wire types are goatty-owned** (`goatty-proto`). rio-vt types never cross the wire; snapshots are plain VT byte streams. | Swapping the VT core (rio → alacritty_terminal) doesn't break remote compatibility. |
| D8 | **Two ways to reach a host: goattyd (default) or plain SSH (fallback). No third mode.** | See §5a. goattyd gives persistence, reconnect, the managed shell, host settings and plugins; goatty installs it with one confirmation. Plain SSH is `ssh host` in a session of the *local* goattyd, for hosts goattyd can't or shouldn't run on. A raw "remote PTY over an ssh channel" mode would fork every feature into two code paths (against D1) for no gain over plain SSH. |
| D9 | **Peers negotiate a protocol version from a range; the wire grows append-only.** | §4. A GUI update no longer strands the sessions in a running daemon, as long as the ranges overlap. |
| D7 | **Threads, not async**, still. Per connection: reader + writer thread. Per session: PTY reader + reply thread. | Simple and fast at the scale of "tens of sessions". The tailnet transport (R2) kept it: no QUIC, so no tokio. |

## 2. Why bytes plus a mirror VT, not structured screen updates

The pasted plan proposed that the server send "structured terminal updates". We already have the important half of that, since the daemon owns the canonical state and reconnects from it. For the live stream, raw bytes are the better encoding:

| | Raw VT bytes + client mirror (chosen) | Grid diffs (tmux control mode / zellij style) |
|---|---|---|
| Typing, prompts, `ls` | A few bytes per change | A row diff per change: larger |
| `cat huge.log` on a fast link | Lossless, ~100 MB/s locally | Diffs per frame: fewer bytes, loses scrollback |
| `cat huge.log` on a slow link | Skip + resync (D3): bounded | Naturally bounded |
| Scrollback, search, selection | Local, instant | Needs a fetch protocol |
| Client CPU | Parses bytes twice in total (≥175 MB/s each, negligible) | Applies diffs |
| Web client later | xterm.js eats the bytes as-is | Needs a custom renderer |
| Protocol surface | Tiny | Large: cell attrs, images, hyperlinks, wide chars… |

The only case where diffs win is the slow-link flood, and D3 covers it. Grid diffs would still matter for a very thin client that can't run a VT (a watch face?). That isn't a goal.

## 3. Layers

```text
 goatty (GPUI)                                         host
┌──────────────────────────┐                ┌──────────────────────────────┐
│ TerminalView / Element   │                │ goattyd                      │
│   mirror Terminal (rio)  │                │   Session ─ PTY ─ zsh        │
│ goatty-client::Connection│                │     authoritative Terminal   │
│   reader ─ dispatch/ack  │                │     subscribers (window/ack) │
│   writer ─ never blocks UI                │   per-connection writer      │
│   heartbeat (remote)     │                │                              │
└───────────┬──────────────┘                └──────────────▲───────────────┘
            │ framed postcard (goatty-proto v2)            │ Unix socket 0600
            ▼                                              │
   Endpoint::Local ──── Unix socket ───────────────────────┘
   Endpoint::Command ── ssh host 'goattyd proxy' ── stdio ── goattyd proxy ─┘
                        docker exec -i box goattyd proxy …
```

Crates: `goatty-proto` (wire types, framing), `goatty-client` (transports, `Connection`, `PaneSink`; no UI dependencies, reusable by a future CLI or TUI), `goatty-daemon`, `goatty-app`, `goatty-term`.

## 4. Protocol v2 (implemented)

Framing: `u32` LE length + postcard. Byte payloads use `serde_bytes`, so a 64 KiB chunk carries a ~5-byte header.

- **Handshake and versions (v6):** `Hello{version, client}` → `Hello{version, daemon_pid, host: HostInfo}`. Each build speaks `MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION` (now 5–6). The client announces its newest version; the daemon answers with `min(client, its newest)`, or with its own newest and hangs up when that's below its minimum, so the client can tell "daemon too old" from "daemon too new" (`VersionMismatch`). Afterwards neither side sends a message newer than the agreed version (e.g. `Connection::run` needs v6). This only works because the enums are **append-only**: new messages are new variants at the end, existing ones never change, each addition bumps `PROTOCOL_VERSION` and is marked `vN:` in `msg.rs`; anything else raises `MIN_PROTOCOL_VERSION`. Daemons up to v5 required an exact match and reply `Error("protocol mismatch: daemon vN, …")`; the client parses that once and reconnects announcing `vN`, so a new GUI keeps working with a running v5 daemon.
- **`Run` (v6):** like `Create`, but the session runs a given `argv` instead of the user's shell (plain SSH uses it).
- **Requests** carry a client-chosen `ReqId`: `ListSessions`, `Create`, `Attach`, `Kill`, `SendInput`, `Shutdown` → `Sessions | Attached | Ok | Error{req}`.
- **Streams are keyed by `SessionId`.** The daemon encodes each PTY chunk **once** and shares the same `Arc<[u8]>` frame with every subscriber on every connection.
- **Stream:** `Attached{snapshot}` (starts with RIS, replays ≤ `history` lines), then `Output{data}`, with optional `Resync{skipped, snapshot}` (does *not* reset, so client scrollback is kept), then `Exited`.
- **Client → daemon stream control:** `Input`, `Resize`, `Detach`, `Ack{consumed}` (cumulative stream bytes), `Ping{nonce}` → `Pong`.
- `StreamOpts{window, history}` per subscription: `LOCAL = 8 MiB / 5000 lines`, `REMOTE = 256 KiB / 2000 lines`.

### Flow control in detail

Per subscriber, the daemon tracks `sent` (stream bytes queued) and `acked`.

1. On each PTY chunk (holding the session's subscriber lock, which also orders snapshots): if `sent - acked + n > window`, mark the subscriber **lagging** and count `n` as skipped. Otherwise push the shared frame.
2. While lagging, no output goes to this subscriber, so its queue drains.
3. The client acks **whenever its transport read buffer is empty** and in between every `window/4`. The "buffer empty" rule guarantees the final partial window is acked, so a lagging subscriber can never deadlock waiting for an ack.
4. When `acked == sent` while lagging: send `Resync` (one screen + modes, about 2–20 KB) and resume streaming.

Effects:
- **Local:** the client parses at ≥175 MB/s with an 8 MiB window, so it never lags. Lossless, same as before.
- **Remote flood** (`cat 1GB.log` over a 2 MB/s link): bandwidth use is bounded, the screen shows current output every round, and **Ctrl+C → prompt visible ≈ window / bandwidth + RTT** (256 KiB at 2 MB/s ≈ 130 ms). With unbounded forwarding (plain ssh, or our Stage 0 queues) that is "after the whole backlog drained".
- **Many panes on one link:** worst-case head-of-line delay ≈ Σ windows / bandwidth. Bounded, and see R1 "interactive priority".
- **Memory:** daemon queues are bounded by Σ windows. Stage 0 had unbounded per-client queues, which a stalled SSH client could grow to gigabytes.
- **Cost of skipping:** lines skipped during a flood are missing from the *client's* scrollback until the user scrolls up to the gap; then the client fetches them from the daemon (§4a).

Covered by `slow_client_is_skipped_forward_with_resync`: an 8 MB flood to a non-acking client delivers < 1 MB, and the subsequent `Resync` shows the final screen.

## 4a. Scrollback on demand and compression (v11, #10, #11)

The attach snapshot replays only the newest lines (2000 remote, 5000 local; the daemon keeps 10 000), and a resync skips lines. Both are fetched when the user scrolls up to them, so attaching stays fast and nothing the daemon has is lost.

- **Host lines.** Every row of a session's primary screen has a number: rows count from the session's start (rio-vt's evicted-row count plus the position in the scrollback), so a number keeps meaning the same row while output scrolls and old rows drop off. Column reflow renumbers rows, on both sides alike.
- **`TopLine{session, line, cause}`** (in stream order, not stream data): "the top row of the screen is host line `line`". The daemon sends it after `Attached`, after `Resync` and after a resize (to every v11 subscriber when the size changed, else to the client that asked), while the primary screen is active. The client's screen matches the daemon's at that point, so it pins the mirror's rows to host lines (`goatty_term::history`): after an attach every row is the daemon's, after a resync the screen starts a new run, and rows that came in as live output continue the numbering. Missing lines are host lines older than the oldest mapped row, and jumps between two runs (what a resync skipped).
- **`FetchHistory{req, session, before, count}` → `History{req, session, before, first, oldest, data}`**: the rows `first..before` as VT text (styles, prompt marks, soft wraps and OSC 8 links, like a snapshot), at most 5000 per request; `oldest` stops the client asking for lines the daemon dropped. The GUI asks whenever a frame is drawn scrolled up and the viewport is within two screens of missing lines (`Session::fill_history`, 1000 lines a time, one request in flight per pane).
- **Inserting** rebuilds the mirror's primary grid from a snapshot of its own rows with the fetched ones spliced in (rio-vt can't insert rows into its scrollback), on a scratch terminal, then swaps the grid in under the lock. Modes, the cursor and its pen stay; the viewport keeps showing the same rows. The parser lock is held throughout, so no output gets in between.
- **Limits.** A reflow (column change) loses track of a resync gap: the client then keeps only the screen's run and fetches nothing above it. Nothing is fetched while the alternate screen is active, nor after a resize there until the next resync or reattach. The mirror keeps at most 10 000 lines, like the daemon.
- **Compression.** A v11 client on a remote link sends `Compress{zstd: true}` right after the handshake. From then on the daemon sends `Attached`, `Resync` and `History` of at least 1 KiB wrapped in `Zstd{data}` (zstd level 3 over the postcard message) when that's smaller. Live output stays uncompressed: chunks are small, and latency matters more. Flow control counts the inner message's stream bytes on both sides, so windows mean the same as before. The local socket doesn't compress (it would only cost time).

Covered by `goatty-term`'s `history` tests (trimmed attach, resync gap, resize, hyperlinks and the pen surviving an insert), `scrollback_is_fetched_from_the_daemon_on_demand` (4000 lines over `goattyd proxy`: the second client attaches with 2000, scrolls up and ends with all 4000 in order) and `snapshots_and_history_are_compressed_when_asked`.

## 4b. Remote working directory and file access (v13, #110)

When hovering or clicking a file path in a remote tab, the client checks whether the path exists on the remote host and either opens it in a remote editor or fetches a read-only local copy over the existing session link (D1, D4).

- **`Cwd { session, cwd }`** (v13): the daemon pushes the session's working directory on attach (gated per subscriber version) and whenever OSC 7 or the foreground `/proc` watcher updates it, so relative link paths resolve against the remote shell's directory instead of the local GUI process's.
- **`StatPath { req, session, cwd, candidates }` → `PathStat { req, info }`** (`crates/goatty-daemon/src/files.rs`): resolves up to 16 candidates against `cwd` when supplied by the client (binding async lookups to the directory in the client's cache key), or else the session's live cwd, `~/`, and `/`, returning `info: Option<RemotePathInfo>` (`{ path, is_dir, size }`) for the first existing regular file or directory. Candidates containing `..` are canonicalized through the kernel rather than collapsed lexically so symlinked working directories resolve like the shell's filesystem; FIFOs, sockets, and devices are ignored. Bounded to 4 concurrent stat workers per connection (`MAX_STAT_WORKERS = 4`) so hung network mounts cannot exhaust threads.
- **`ReadFile { req, path, max_bytes }` → `FileData { req, path, data }`** (`crates/goatty-daemon/src/files.rs`): opens regular files with `O_NONBLOCK | O_NOCTTY` after `stat`, re-checks `fstat` to defeat stat-to-open races, and caps reads at `min(max_bytes, HARD_MAX_FILE_BYTES)` (`MAX_FRAME - 64 KiB`), returning `Error` if the file exceeds the cap. Bounded to 2 concurrent read workers (`MAX_FILE_WORKERS = 2`) and bounded queued `FileData` bytes (`MAX_QUEUED_FILE_BYTES = MAX_FRAME`) per connection without blocking control or terminal stream frames.
- **Head-of-line trade-off and client cache:** `FileData` travels as one postcard frame (zstd-compressed when enabled) on the connection's single writer queue, so large transfers temporarily queue with live terminal `Output` on slow links. To keep transfers bounded, `max_bytes` defaults to `links.remote_max_size_mb` (10 MiB, prompting before larger fetches up to `HARD_MAX_FILE_BYTES`), and `Connection::read_file` scales its request timeout from `max_bytes`. Fetched files are cached under `<runtime_dir>/goatty/remote/p<pid>/<hash16>/<basename>` (`goatty_proto::runtime_dir()`) as read-only (`0444`) files written via temp-and-rename; `<hash16>` hashes `(Endpoint::cache_key, remote_path)` so distinct endpoints never overwrite one another's files, and process-scoped `p<pid>` directories let startup cleanup remove stale caches after 24 hours only when the owning PID is no longer alive.

## 4c. Size policy for several clients (v14, #12)

A session has one size, the PTY's, and one **controlling** client sets it (`Session::controller` in goattyd). The other clients watch at that size.

- **Who controls.** The client that creates a session (reserved for it until it subscribes, even if another client attaches in between). A client that attaches takes control only when nobody has it (no other subscriber, or the controller is gone), so reattaching from a new window still sizes the session to it, while a second window on a session someone is using doesn't shrink it under them. When the controller detaches or its connection ends, the newest remaining subscriber gets control and the session takes the size that client last asked for.
- **`Resize`** from any client records the size it wants. It is applied only while that client controls the session; a watcher's resize changes nothing yet.
- **`TakeSize{session}`** (v14): take control explicitly. The session takes the size this connection last sent.
- **`SizeControl{session, size, controlling}`** (v14, in stream order under the subscriber lock): the session's size and whether this client controls it. It comes right *before* `Attached`, so a watcher sizes its mirror before the snapshot lands (the client keeps it until the `Attached` that adds the pane; for `Create` it precedes the reply carrying the session id), and to every v14 subscriber whenever the controller or the size changes, before the reflowed output and the `TopLine`.
- **The GUI** (`goatty-app` `Session`): while controlling, the mirror follows the pane as before. While watching, the mirror stays at the session's size and the pane letterboxes it: the space past the grid is shaded with an edge line, a grid larger than the pane is clipped and pans so the cursor's cell shows, clicks in the letterbox don't reach a cell (a right click there opens the pane's menu), and a chip says "Sized by another window · Take over". Taking over (the chip, the pane's menu, the palette's "Size the session to this pane", bindable as `take_session_size`) sends `TakeSize`; on gaining control the mirror goes back to the pane's size and sends it as `Resize`.
- **Sizes are bounded** (`WinSize::is_sane`: at most 4096 columns, 2048 rows, 2²¹ cells). goattyd refuses a larger `Create`/`Run`/`Attach` with an `Error` and a larger `Resize` with `Error{req: None}`; a client ignores a larger `SizeControl`. They are refused, not clamped down: a snapshot made for one grid can't be fed into another. Small sizes are rounded up to a 2×1 grid (`to_size`), and `SizeControl` carries the grid the session really has.
- **The PTY has the size first.** goattyd sets it (`TIOCSWINSZ` on the master fd, without the PTY lock a blocked input write can hold) under the subscriber lock, before the reflow and before any client hears the size. When the ioctls succeed, concurrent resizes leave the PTY at the size decided last. A failed ioctl is logged (the PTY keeps its previous size), and the terminal still takes the new one.
- **Older clients** (before v14) can't watch at another size or take over, so for them nothing changed: each `Attach` and `Resize` takes control. A v14 daemon tells the v14 clients about it like any other change of control.
- **Limits.** goattyd has no idle timeout of its own yet: a controller that went silent while its transport stays open keeps control until the daemon sees the connection end (the client's heartbeat doesn't help the daemon). Roadmap R1.10 adds a daemon-side timeout. Control isn't part of the restore state: after a restart, the first client to reattach gets it.

Covered by `crates/goatty-daemon/tests/size_control.rs`.

## 5. Transports

### Local
`$XDG_RUNTIME_DIR/goatty/goattyd.sock`, dir 0700, socket 0600. The client auto-starts `goattyd serve` with `setsid` if it's missing.

### SSH (implemented: `goatty --ssh <host> [--remote-daemon PATH] [--plain]`)
`Endpoint::Ssh` runs `ssh -T -e none -o ServerAliveInterval=10 -o ServerAliveCountMax=3 -o ConnectTimeout=10 -- <host> sh -c '…'` (`--` so a host can't be read as an option). The remote script prefers this build's installed goattyd (`~/.local/share/goatty/bin/goattyd-<version>-p<protocol>`), then `goattyd` in `PATH`, `~/.local/bin` or `~/.cargo/bin` (non-interactive SSH often lacks those in PATH), and execs `goattyd proxy`. Before that it prints one line to stderr, `goatty-remote: <uname -s> <uname -m> <path or ->`, which tells the client the host's platform and what it found without another round trip. It is POSIX sh because the login shell may be fish.

ssh's stderr is captured (logged, last 40 lines kept): a failed connect shows ssh's own message instead of "no greeting". Prompts are unaffected, since ssh asks on `/dev/tty` or through askpass, never on stderr.

`goattyd proxy` connects to the host's daemon socket, starting the daemon if needed, and relays bytes in both directions. It writes stdout through the raw fd, because std's `Stdout` is line-buffered and would hold binary frames back.

**Tailscale:** works out of the box with `--ssh <magicdns-name>`, using normal OpenSSH over the tailnet or Tailscale SSH. Without SSH, paired tailnet hosts connect through `Endpoint::Tailnet` ([`tailnet.md`](tailnet.md)): still no network socket in goattyd, because tailscaled forwards into a Unix socket.

### Any stdio command (implemented: `goatty --via '<cmd>'`)
E.g. `--via 'docker exec -i devbox goattyd proxy'`. The command line runs under `sh -c`, so shell quoting works for arguments with spaces. The tests use this with the local `goattyd proxy` to exercise the full remote path, including reconnect, without ssh.

### Socket watchdog (implemented)
If another listener has taken over the path (a second daemon, or a systemd socket installed meanwhile), the watchdog leaves it alone and says so once.

On servers, logind deletes `/run/user/$UID` when the last login session ends, unless linger is enabled. The daemon survives, but its socket path disappears, and the next `ssh host goattyd proxy` would silently start a second, empty daemon. goattyd checks the socket's inode every 2 s and re-binds if it changed. (R1 also recommends `loginctl enable-linger` and ships a systemd user unit.)

## 5a. goattyd vs. plain SSH (decided 2026-11-09, #5)

Connecting to a host always means goattyd on that host, unless the user picks plain SSH.

`goatty --ssh host` opens the terminal window right away; its tab shows the connect screen (`goatty-app/src/connect.rs`, #37) until the session is there, and so does every tab opened on a host. Failures goatty can fix become questions in that tab (`RemoteProblem`):

| Situation | Offered |
|---|---|
| No goattyd on the host, platform supported | **Install goattyd** · Plain SSH · Cancel |
| Platform without a goattyd build (macOS, BSD, armv7, …) | Plain SSH · Cancel |
| Supported, but this goatty carries no binary for it | Retry (after installing by hand) · Plain SSH · Cancel |
| Running goattyd older than our range | **Update and restart** (says it ends the host's sessions: a daemon that old can't keep them) · Plain SSH · Cancel |
| This build's goattyd installed, but an older daemon still runs | **Restart goattyd** (same warning) · Plain SSH · Cancel |
| Running goattyd newer than our range | Plain SSH · Cancel ("update goatty on this computer"); goatty never downgrades a daemon |
| Anything else (auth, network) | Retry · Plain SSH · Close, with ssh's last words |

After an install, the first connection brings this computer's shell setup along (settings, plugins, tools; [`profiles.md`](profiles.md#the-same-shell-on-every-host-159)), unless Settings → Hosts says never.

**Install** (`remote::install`) streams the binary over the same ssh into `~/.local/share/goatty/bin/.goattyd-….$$`, checks the size, runs `--version` there (catches wrong CPU, too-old libc, `noexec` home), then renames it into place. Nothing else on the host changes: no sudo, no PATH or dotfile edits; `rm -r ~/.local/share/goatty` undoes it. Restarting runs that goattyd's `restart --force` (§5c): a daemon from protocol v10 on re-executes itself as the new build and keeps its sessions; an older one is stopped like `stop --force` does, which works on a daemon of *any* version through its socket (`SO_PEERCRED`, only if the process is a `goattyd`; through systemd when it's the service). The binaries come from the release archive: `remote/goattyd-{x86_64,aarch64}-linux`, static musl builds (`release.yml`; measuring them against glibc: #132). Development builds upload their own `goattyd` when the host has the same CPU, with a note that it may not run on an older libc. After a successful install it deletes the other `goattyd-*` builds in that directory unless a running process uses one (`/proc/<pid>/exe`; without `/proc` it keeps them all), so updates don't pile up binaries.

**Plain SSH** (`--plain`, or the button) runs `ssh -- host` in a session of the local goattyd (`Run`, v6), falling back to a window-owned PTY if the local daemon is too old. You get exactly what `ssh` in any terminal gives: the user's ssh config, prompts in the terminal, the host's own shell. Nothing is installed, and nothing of goatty runs there (no managed shell, no host settings or plugins; the settings window configures the local host). The session survives closing the window (the ssh process lives in the local daemon), but not a network drop. Typing `ssh host` inside a goatty shell is the same thing and needs no feature at all.

Remembering the choice per host: the Hosts settings page sets `ssh.hosts[].plain`, and `goatty --ssh host` and the palette then go straight to plain SSH for that host.

**Host list (#115).** The command palette lists the hosts to connect to: the ones added in the settings (`ssh.hosts`) and, with `ssh.discover` (default on), every concrete `Host` in `~/.ssh/config` and the files it `Include`s (`goatty_client::ssh_config`, read off the UI thread into the app's `hosts::HostList`). In the full palette (Ctrl+Shift+P) hosts appear once something is typed, or when recently used, and picking one connects.

**Host switcher and hosts in tabs (#4, #37).** Ctrl+Shift+H (`connect_to_host`) lists this computer and the hosts, with `user@hostname · OS · RTT` (`Connection::host()`/`rtt()`) for the ones goatty is connected to, and offers to connect to any `user@host` typed there. Picking a host lists its sessions (`list_sessions`, connecting first when no tab is on it): a new session, or one to attach to. Plain SSH hosts connect right away. A host opens where `ssh.open_in` says: a window of its own (default) or a new tab of the active window (Shift+Enter in the session list: the other way; Settings → Hosts). Either way its tab shows the connect screen above, so a missing or old goattyd is handled the same way as with `--ssh`.

A window can hold tabs on several hosts. Each tab keeps its host: new tabs and splits from it open there (plain SSH tabs run `ssh host` again). Tabs on the same host share one `Connection` (`hosts::pool`, weak: it closes with its last tab), so a second tab doesn't log in again. Each goattyd keeps the layout of the window's tabs on it, so `goatty --ssh host` brings back that host's tabs. The settings window and "reload shells" act on the host of the active tab.

## 5b. SSH prompts in the GUI (#6) and goattyd as a systemd service (#7)

**Prompts.** For `Endpoint::Ssh` the GUI sets `SSH_ASKPASS` to its own binary, `SSH_ASKPASS_REQUIRE=force` (OpenSSH ≥ 8.4; also when started from a terminal) and `GOATTY_ASKPASS=1` (internal marker, not a setting). ssh then runs `goatty "<prompt>"`; `main` sees the marker and shows one small window (`goatty-app/src/askpass.rs`): a masked field for passphrases and passwords, "Trust and connect"/"Cancel" for unknown host keys (answers `yes`/`no`), Yes/No for `SSH_ASKPASS_PROMPT=confirm`, a notice for `none` (security key touch). Answer on stdout, verdict in the exit status. The prompt kind is decoded in `goatty_client::askpass`. Reconnects can prompt again (#130).

**systemd user service** (opt-in: `goattyd service install|uninstall|status`). `goattyd.socket` listens on `%t/goatty/goattyd.sock` (0600, dir 0700); `goattyd.service` runs `goattyd serve`, which takes fd 3 per `sd_listen_fds` and removes `LISTEN_*` so shells don't inherit them. Clients that find the socket missing start the socket unit (`systemctl --user start goattyd.socket`) when it's installed, otherwise a detached daemon as before. Decisions:

- **Opt-in, not the default.** Without lingering, systemd stops user services, and with them every session, when the last login session ends: on a server that is every SSH disconnect. The detached daemon survives that (unless logind's `KillUserProcesses=yes`). `install` reports the linger state and prints `loginctl enable-linger` instead of running it. On desktops, a systemd-started daemon gets the user manager's environment, not the GUI's (`SSH_AUTH_SOCK`, PATH additions), which shells notice.
- **No `Requires=goattyd.socket`** in the service: stopping or uninstalling the socket never takes sessions with it. `uninstall` leaves a running daemon alone.
- **Never hijack a running daemon:** `install` only starts the socket when no daemon started the old way is serving the path; otherwise systemd takes over after it stops.
- **Sessions outlive the daemon process** (#131, §5c): the unit has `NotifyAccess=main`, `FileDescriptorStoreMax=` and `KillMode=process`. Units from older goattyds lack them; `goattyd service status` says so and `goattyd service install` rewrites them.

## 5c. Sessions across daemon restarts, upgrades and crashes (#131)

A shell lives as long as its PTY master is open, so goattyd hands the masters on instead of closing them (`goatty-daemon/src/restore.rs`):

- **`goattyd restart`** (protocol `Restart`, v10; the GUI's restart of an outdated local daemon, and "Update and restart" on a host). The daemon first runs the new build's `goattyd restore-version` and refuses unless it reads its state format, so an old or foreign program never gets the sessions. New sessions are refused from then on (one being started is waited for). Every session's output pump pauses between two reads (nothing read is lost; the rest waits in the PTY) and the query answers already produced reach the PTY; if either doesn't happen within a second the restart is called off and nothing changes. Then the screens go to `<socket>.restore/` (0600 in the socket's 0700 dir; both of them while a full-screen program shows the alternate screen) with ids, pids, creation times and the saved window layouts, the PTYs and the listening socket lose close-on-exec, and the process `execve`s `goattyd serve --restore FILE`. Same pid: the shells are still its children (exit codes keep working) and systemd notices nothing. The socket never stops listening, so clients reconnect and reattach as after any dropped link. Without systemd this is the whole story.
- **Under the systemd service** each PTY also sits in the fd store from the moment its session starts until it ends. On SIGTERM (`systemctl --user restart`, or `stop`) goattyd saves the screens and exits without touching the shells (`KillMode=process`). The next start gets the PTYs back in `LISTEN_FDS` (named `goatty-s<id>-<created>-<m|p>`), finds each shell as the terminal's session leader (`TIOCGSID`) and rebuilds the sessions, with the saved screens when the pid matches. After a crash (`Restart=on-failure`) nothing was saved: the screens start empty and the foreground programs get SIGWINCH to draw theirs again. These shells aren't goattyd's children any more, so a session ends at PTY EOF without an exit code.
- **What `stop` means.** `goattyd stop` (and the GUI's restart of a daemon too old for the above) ends the sessions, as before. `systemctl --user stop goattyd` empties the fd store, which closes the terminals: the shells get SIGHUP like when a window closes, and a program that ignores SIGHUP keeps running (in the unit's cgroup, without a terminal), as it would after closing any terminal emulator. A logout without lingering is such a stop.
- **Ending sessions without opening them.** Since sessions now live until someone ends them, the session list ("Sessions on this host…", or picking a host in Ctrl+Shift+H) ends one with Shift+Delete or its trash button (twice: the first asks). `goattyd list` / `goattyd kill ID` do the same from a shell.
- **Limits.** A reboot still ends everything. Scrollback carries over up to 5000 lines (the attach snapshot's limit). The palette colors and other per-connection state come back when clients reattach.

## 6. Reconnect (implemented)

- The client's reader sees EOF/error, or the heartbeat sees 20 s of silence (Ping every 5 s on remote links). It closes the transport (kills the ssh child), marks panes `Disconnected`, and fails in-flight requests.
- The daemon side (#13): a client that vanished without closing its link (dead TCP behind ssh) leaves the proxy's socket open, so goattyd never sees EOF. So every 2 s it shuts down each connection that has pinged and then sent nothing for 60 s (`client::reap_silent`, on its own thread). That ends the connection's subscriptions and corrects the `attached` count and size control. The proxy exits once its relay sees the daemon's EOF or an I/O error (a write to a stdout nobody drains can still hold it up). Input goes to the PTY on a separate thread per connection (`client::input_loop`), so a program that stops reading and fills its PTY can't block the reader: it still sees pings and EOF. Connections that never ping (local clients) are left alone: an idle one is just idle, and a dead one closes its socket.
- Backoff: 0.2, 0.5, 1, 2, then every 5 s. Each attempt runs a new transport, handshake, and then `Attach` for every pane with its last size.
- The reattach snapshot starts with RIS, which also wipes the local "[goatty: connection lost; reconnecting…]" line the client printed into its mirror. The window title shows `— user@host (reconnecting…)`.
- `Attach` fails (e.g. the daemon restarted) → `Lost`.
- The UI never blocks: input/resize go through the writer thread. Input typed while disconnected is dropped, not replayed later (replaying stale keystrokes into a shell is dangerous).

Covered by `client_over_proxy_reconnects_and_reattaches`: kill the proxy mid-session → reconnect through a fresh proxy → same shell PID, shell variables intact, screen restored.

## 7. Performance

### Measured (`cargo run --release -p goatty-daemon --example netbench -- [proxy | slow RATE | ssh HOST]`)

Headless client feeding a real mirror VT (the GUI's work minus rendering), 200×50, `/bin/sh`, same machine as Stage 0:

| Endpoint | Echo p50 / p90 | 200 MB flood | Delivered | Resyncs |
|---|---|---|---|---|
| Local Unix socket (8 MiB window) | 0.57 / 0.99 ms | 0.83 s (242 MB/s) | 200.0 MB, lossless | 0 |
| `goattyd proxy` over stdio (256 KiB window: the remote code path) | 0.49 / 0.76 ms | 0.89 s (224 MB/s) | 199.9 MB | 1 |
| Same, throttled to **2 MB/s** with `pv -L` | 0.75 / 1.28 ms | **0.74 s** | 3.0 MB | 5 (197 MB skipped) |

Echo latency includes the PTY round trip and the benchmark's 0.2 ms polling. On the throttled link, naive forwarding would need ~100 s to drain the flood before the prompt (or a Ctrl+C) became visible. With skip-and-resync the prompt shows when the command finishes, and keystrokes stay fast during the flood. Local throughput is lossless and well above what the GUI renders (~100 MB/s end to end in Stage 0), so the protocol is not the bottleneck.

### Snapshots and history (`cargo run --release -p goatty-daemon --example snapbench [FILE]`)

120×40 terminal fed with this repository's Rust sources (lightly colored), 10 000 lines of history:

| | Time | Size |
|---|---|---|
| Attach snapshot, 2000 lines: zstd level 3 | 0.23 ms (decompress 0.06 ms) | 95 KB → 25 KB (3.7×) |
| `History`, 1000 lines: serialize / zstd | 0.39 / 0.11 ms | 44 KB → 13 KB (3.4×) |
| Insert 1000 fetched lines above 2000 / 8000 (client) | 1.9 / 4.4 ms | |

Compression costs far less than sending the bytes over any remote link (25 KB less is 12 ms at 2 MB/s). Inserting rebuilds the whole grid, so it grows with the mirror's scrollback; at the 10 000-line cap it stays a few milliseconds per 1000 fetched lines, on the reader thread.

### Budget

| Path | Budget | Notes |
|---|---|---|
| Local flood throughput | ≥ Stage 0 | One encode per chunk shared by all subscribers, batched writes with flush-on-idle, acks only every ≥2 MiB or per drained read. Still to do: re-run the GUI `scripts/bench.sh` |
| Local keystroke → PTY | < 0.2 ms added | Writer thread flushes as soon as its queue is empty |
| Remote keystroke echo | RTT + ~1 ms; shown at once with prediction | R3 local echo prediction above 30 ms RTT |
| Remote attach | ≤ 2000 lines of history | ~100 KB of source code, 25 KB with zstd (§7 snapbench); older lines on scroll-up |
| Remote flood: Ctrl+C visible | ≈ window/bandwidth + RTT | 256 KiB window |
| Daemon memory per subscriber | ≤ window | Was unbounded |
| Idle | 0 wakeups locally, 1 ping per 5 s remote | |

### Pitfalls found while building R0
- **`std::io::copy` from a pipe into a Unix socket uses `splice()` on Linux, and the data can stay stuck in the kernel.** In `goattyd proxy`, the client's Hello never reached the daemon, so the handshake deadlocked. The proxy now uses a plain read/write loop. This would have broken every SSH connection.
- std's `Stdout` is line-buffered, so the proxy writes through the raw fd.
- Over SSH, socket backpressure says nothing about client progress (ssh's channel window plus TCP buffers are megabytes). That is why flow control uses application-level acks.

## 8. Roadmap

### R0: protocol and transports ✅ (this change)
Protocol v2, `goatty-client` crate, multiplexed connections, ack/window flow control with resync, encode-once broadcast, `goattyd proxy`, `--ssh` / `--via`, reconnect with reattach, heartbeat/RTT, socket watchdog, tests for all of it.

### R1: remote MVP polish (next; tracked in the [R1 milestone](https://github.com/goatty-dev/goatty/milestone/1))
1. **Host list and switcher UI**: ✅ `~/.ssh/config` `Host` entries plus hosts added in the settings, in the command palette (#115, §5a); the host switcher with OS and RTT, a per-host session picker, and hosts as tabs of one window (#4, #37).
2. ✅ **Remote goattyd install and version negotiation**, plus the plain SSH fallback (§4, §5a).
3. ✅ **SSH prompts in the GUI** through goatty as `SSH_ASKPASS` (§5b).
4. ✅ **Daemon as systemd user service** (opt-in, socket-activated) with linger guidance (§5b). FDSTORE: #131.
5. **Interactive priority in the daemon writer**: small frames (echo, prompts) jump ahead of bulk frames from flooding panes. This removes most cross-pane head-of-line delay on one link.
6. **Adaptive window**: measure delivery rate from acks and RTT from pings, then set `window ≈ max(64 KiB, rate × 50 ms)`. A fast LAN streams losslessly, a slow link skips early.
7. ✅ **Lazy history**: `FetchHistory` fills the client's scrollback on demand, for the trimmed attach snapshot and for lines skipped by resync, with stable host line numbers (§4a).
8. ✅ **Compression** for snapshots and history (zstd, negotiated per connection; §4a). Live output stays uncompressed (it's small, and latency matters more).
9. ✅ **Size policy for multiple clients**: one *controlling* client per session sets the PTY size. Others watch (letterboxed) or take over explicitly (§4c, protocol v14).
10. **Daemon-side liveness**: drop subscribers whose connection has been silent for >60 s (pings), so the attached count and size control are correct after a client vanished without FIN.
11. **Client vs host settings split** in the profile model (font/keys/theme are client; shell/plugins/env are host). The plugin manager always acts on the connected host.

### R2: hosts on the tailnet ✅ (#15, protocol v12)
Paired in the app, no SSH setup: see [`tailnet.md`](tailnet.md). goatty uses the user's installed Tailscale. A host that is made available gets one `tailscale serve --tcp` route into a Unix socket of goattyd's, so goattyd still binds no network socket (D5). Each connection authenticates with a device key pinned by pairing (a 6-digit code confirmed on both screens). Outbound, the client uses plain TCP to the peer's tailnet address, or `tailscale nc` in userspace mode. QUIC was dropped: WireGuard already encrypts and authenticates, and Tailscale's stable addresses cover roaming. Per-pane streams without head-of-line blocking would be the only gain, and skip-and-resync bounds that (D7 stays).

### R3: latency hiding
✅ Mosh-style **local echo prediction** (#16), an overlay on the client's mirror VT with no protocol change (`goatty-term/src/predict.rs`). Typed printable, single-width characters are drawn underlined where they will land, with the cursor after them, and checked against the grid when the daemon's output arrives: once the real cursor has passed a guessed cell, a matching character confirms it (the overlay drops it; the real cell shows it), a different one drops every guess. The cursor leaving the line drops them too, and so does a guess that isn't echoed within 3 × RTT + 400 ms. Backspace takes back a guess that hasn't been echoed; prediction stops before the last column (wrapping is the application's business).

Guesses belong to *epochs*. Input we can't predict (Enter, arrows, control keys, escape sequences, pastes) starts a new one, and a new epoch is **tentative**: nothing in it is shown until the host echoes one of its guesses. That is what keeps password prompts (echo off) from ever showing what is typed. An epoch is shown right away only when the shell reports a prompt with its line editor running (the managed zsh's `OSC 7337;zle=1`, cleared by `OSC 133;C`), that prompt is newer than the last unpredictable input (each `zle=1` counts a prompt; a key bound to a widget could read a secret with `read -s` while zle stays on), the reaction to the last unpredictable input has arrived (the guesses before it resolved, then a further output chunk), and no shown guess was wrong since the host last confirmed one. Guesses typed after unpredictable input can't be confirmed before that reaction: their cursor is stale, and the echo of an earlier guess (the `s` of `sudo`) could seem to confirm them (the `s` of a password typed ahead). Turning drawing on or off, a resize or a reconnect forgets the guesses but keeps waiting for that reaction. So at a fresh managed zsh prompt typing shows at once from the first key; after an arrow, Tab or a Backspace with nothing to take back, and in other shells, from the second key on.

`ssh.local_echo` (Settings → Hosts → Connecting): **Auto** draws guesses when the host is slower than 30 ms: the link's RTT (`Connection::rtt`) or the measured **echo delay**, whichever is longer. Guesses are always made and checked, just not drawn while the host is fast, and each confirmed one gives an echo delay sample (smoothed 7/8 old + 1/8 new, a sample counts at most 1 s). That covers what the link RTT can't see: plain SSH tabs (`ssh host` inside a local session), a loaded host, `ssh` run by hand inside a session. **Always** draws them however fast the host is (useful to try it locally), **Never** turns prediction off. Never in the alt screen or while reconnecting; a resize or reconnect forgets all guesses.

## 9. Risks and open questions

| Risk | Mitigation |
|---|---|
| Version skew between GUI and remote daemon | Version range negotiation; install/restart from the connect screen (§5a) |
| Head-of-line blocking across panes on one SSH stream | Bounded by Σ windows; R1 interactive priority; several connections per host if measurements show a need |
| Scrollback gaps after resync | Fetched from the daemon's scrollback on scroll-up (§4a); lost after a column resize |
| Security: proxy access = full control of the daemon | Same trust as an SSH shell on that host; the daemon socket stays 0600 and is never exposed |
| `$XDG_RUNTIME_DIR` removed on logout | Watchdog re-bind (done); systemd user unit + linger (done, opt-in) |
| `goattyd` not in PATH for non-interactive SSH | Script searches common dirs; `--remote-daemon`; auto-install (done) |
| Windows client | `Endpoint::Command` with `ssh.exe` works unchanged; local transport needs named pipes instead of Unix sockets, and process detaching instead of `setsid` |
| Session size conflicts with two clients | R1 controlling-client policy |

## 10. How to try it

```sh
cargo build --release --workspace
# goatty offers to install goattyd on the server if it's missing or too old
# (or install it yourself: cargo install --path crates/goatty-daemon → ~/.cargo/bin/goattyd)

./target/release/goatty --ssh myserver          # ~/.ssh/config alias, user@host, or a Tailscale name
./target/release/goatty --ssh myserver --plain  # just `ssh myserver` in a local session
goattyd service install                         # on a server: systemd user service (+ loginctl enable-linger)
./target/release/goatty --ssh myserver --new    # another session on that host
./target/release/goatty --via 'docker exec -i devbox goattyd proxy'

# Reconnect: start `htop` remotely, then toggle Wi‑Fi or `pkill -f 'goattyd proxy'` on the server.
# Persistence: close the window, then run `goatty --ssh myserver` again → same session.
```

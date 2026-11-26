# goatty architecture

How the code is laid out and where new work goes. For the product's goals and decisions see [`goatty-project-plan.md`](goatty-project-plan.md); for the remote design see [`remote-architecture.md`](remote-architecture.md). This doc names modules and rules, not their details: read the module's own doc comments for those.

## Crate graph

```
                 goatty-config      (profile model: client/host JSON documents, store, themes, keys, profiles, the prompt model)
                   ▲        ▲   ▲
                   │        │   │   goatty-plugin  (plugin format, permissions, signed registry index; bin goatty-plugin)
                   │        │   │      ▲
                   │        │   goatty-shell    (managed zsh: generated ZDOTDIR, tool installs, plugins on a host, diagnostics, the shared config; uses goatty-net's device key)
                   │        │      ▲      ▲
goatty-term ◄──────┼────────┼──────┼──────┼─┐   (the only crate that touches rio-vt)
   ▲               │        │      │      │ │
   │   goatty-proto│        │      │      │ │   (wire protocol: messages, framing, socket path)
   │      ▲        │        │      │      │ │
   │      │   goatty-net    │      │      │ │   (the tailnet: tailscale CLI, device keys, pairing, gateway; std only)
   │      │     ▲  ▲        │      │      │ │
   │   goatty-client        │      │      │ │   (Connection, Control, transports; no UI)
   │      ▲     ▲           │      │      │ │
   │      │     └──── goatty-daemon ──────┘─┘   (lib + bin `goattyd`)
   │      │                        │
   └── goatty-app ─────────────────┘            (GPUI client, bin `goatty`)
```

Rules:

- **rio-vt stays inside `goatty-term`.** Other crates use `Terminal`, `Frame`, `TermSize`, `TermMode` and the input encoders. `Terminal::with_term` is crate-private on purpose.
- **`goatty-proto` has no I/O beyond framing.** Transports live in `goatty-client`.
- **`goatty-client` has no UI dependencies.** The app, the daemon's CLI, tests and benchmarks all use it.
- **Which shell a session runs is `goatty-shell`'s decision** (`Launcher::launch`). goattyd and `goatty --local` both use it; `goatty-term` only spawns what it's given. `goatty-shell` has no rio-vt or UI dependency.
- **The shared shell config** (#313, [`profiles.md`](profiles.md)): host settings resolve as defaults ← shared ← this host's override. Each daemon keeps the signed document (`shared.json`) and applies it itself; daemons never talk to each other, clients relay. Trust is per machine: a machine counts only entries signed by devices it trusts, so a host can't change another machine's shell. `goatty-shell` uses `goatty-net` only for the device key.
- **Client vs host settings** (#14): `ClientSettings` (`settings.json`) is read by the app; `HostSettings` (`host.json`) by goattyd/`HostService` on its own host. The GUI edits host settings through the protocol, never by writing a remote file. See [`profiles.md`](profiles.md).
- **`goatty-plugin` knows the format, not the host.** Manifests, settings schema, templates, permissions, zsh rendering, the signed index and the JSON API live there with no knowledge of `Dirs`; `goatty-shell::plugins` decides where things go. The `goatty-plugin` CLI depends on nothing else in the workspace, so registry CI builds it quickly. See [`plugins.md`](plugins.md).
- **Settings come from `goatty-config`.** Don't read `GOATTY_*` variables elsewhere (the exceptions: `GOATTY_SOCKET` in `goatty_proto::socket_path`; `GOATTY_ASKPASS` in `goatty_client::askpass`).
- **The tailnet** (#15, [`tailnet.md`](tailnet.md)): `goatty-net` knows Tailscale's CLI, the device key, pairing and the gateway handshake. It knows nothing of the daemon or `Dirs`: callers pass `goatty_net::Paths`. goattyd owns one `Host` (`goatty-daemon/src/net.rs`); `goatty_client::tailnet` connects, discovers and pairs. goatty only uses the user's installed Tailscale through its public CLI, and changes only its own Serve route.
- **Remote hosts** (`goatty-client`): `transport` (ssh endpoints), `remote` (the connect script, its `goatty-remote:` probe line, `RemoteProblem`, install/stop over ssh), `askpass`, `service` (systemd, launchd and Windows service units), `ssh_config`, and `local` (the same for this computer's goattyd: outdated detection, restart, stop). The app turns `RemoteProblem` and `LocalProblem` into questions in `connect.rs`. See [`remote-architecture.md`](remote-architecture.md) §5a/§5b.

## Daemon (`crates/goatty-daemon`)

| Module | Owns |
|---|---|
| `server` | socket bind (or the socket from systemd), accept loop, watchdog, the session registry (`Daemon`), shutdown |
| `workspaces` | workspaces (#215): the client's layout JSON, sessions, revisions and conflicts, which windows show which, lifecycle. See [`workspaces.md`](workspaces.md) |
| `restore`, `systemd` | sessions that outlive the daemon process (#131): `Restart` re-execs with the PTYs inherited, and under systemd the PTYs come back from the fd store |
| `client` | one connection: decode requests, subscriptions, the writer thread, liveness |
| `session` | PTY + authoritative `Terminal` + subscribers with flow control, the output pump |
| `writer`, `flow` | the per-connection writer with per-session lanes, and the adaptive flow-control window |
| `proxy` | `goattyd proxy`: stdio ⇄ socket relay for remote transports |
| `net`, `net_cmd` | this host on the tailnet; `goattyd net …` |
| `settings`, `files` | host settings, shell and plugin requests (through `goatty_shell::HostService`), remote file access |
| `main.rs` | the clap CLI; control commands go through `goatty_client::Control` |

Threads per session: a PTY reader and a reply thread (query answers → PTY). Per connection: a reader and a writer.

## Shell environment (`crates/goatty-shell`)

One module per concern: `launch` (`Launcher`), `zsh` (renders `settings.zsh`, plugin slot files and the static startup files in `zsh/`), `starship`, `builtins` and `plugins` (the built-in tools are plugins compiled in; `plugins/` is registry, lock, install and load order), `packages` and `install` (tool downloads), `completions`, `doctor` and `setup` (diagnostics as JSON), `import` (the user's own shell setup, harvested without a terminal), `everywhere` (goatty's shell in every terminal, #283), `path_report`, `generation` and `history` (build, validate with `zsh -n`, activate, roll back, diff), `shared` (the shared shell config on a host: storage, the `Shared` API, applying it, plugin reconciling, relaying between hosts, #313), `sync` (the push sync for older goattyds, #159), `snippets`, `service` (`HostService`), `cmd` (`goattyd shell|plugin|snippet …`), `dirs`.

Design and startup sequence: [`managed-zsh.md`](managed-zsh.md). The zsh behaviour is tested against real zsh in `tests/zsh_startup.rs` (sandboxed `$HOME`, PTY for OSC 133/7).

## App (`crates/goatty-app`)

| Where | What |
|---|---|
| `main.rs`, `cli.rs`, `commands.rs`, `links.rs` | CLI parsing, what the window starts with (a connect plan, or a local PTY for `--local`), the headless `settings`/`theme` commands, and `goatty://` deep links + GUI-to-GUI handoff |
| `app.rs`, `settings.rs`, `actions.rs` | GPUI bootstrap and windows; the `AppSettings` global (file ⇄ live values, theme previews); the action map, including `plugin.<id>.<cmd>` and `custom.<id>` |
| `workspace/` | a window's root view: tabs of split panes, each tab on its own host. `mod.rs` behaviour, `layout.rs` split trees (pure), `arrange.rs` swap/rotate/move, `tab_bar.rs`, `sidebar.rs`, `groups.rs` (tabs grouped by host), `menu.rs` (context menus), `chrome.rs` (title bar and frame when goatty draws them), `notices.rs` (agent and program notifications), `gestures.rs`, `icons.rs`, and the workspace sync below: `sync.rs`, `saved.rs`, `reopen.rs`, `switcher.rs` |
| `terminal/` | `TerminalView` (input → terminal, actions, emits `TerminalEvent`), the grid `Element`, per-cell glyph cache, box drawing, key translation (pure), links |
| `session.rs` | a pane's data: local PTY or goattyd session → mirror `Terminal`; shared by the windows of this process; `Spawner` for new panes |
| `connect.rs`, `hosts.rs`, `tailnet.rs`, `askpass.rs` | connecting off the UI thread, the host list and connection pool, tailnet hosts and pairing, goatty as ssh's askpass |
| `command_palette/` | the palette: fuzzy ranking, the registry, input mode for questions (`Ask`), host and session scopes |
| `settings_window/` | the settings window. One file per page, shared widgets in `kit.rs`, and the index of every setting in `search.rs` |
| `output_view/` | the output viewer over a pane (all output, or one command's), with its own search and layout |
| `toast/` | notifications: per-window stacks, a layer, a history of the last 50 |
| `ui/` | widgets: controls, text input, color picker, context menu, icons, theme-derived colors, the prompt preview |
| `snippets/`, `plugin_commands.rs`, `custom_commands.rs`, `typing.rs` | snippets picker, plugin and user commands, and which pane goatty may type a command into |
| `shell_setup.rs`, `shared_sync.rs`, `shell_sync.rs`, `shell_path.rs`, `welcome.rs`, `pickers.rs`, `close_confirm.rs`, `paste_confirm.rs`, `agent/`, `fonts.rs` | first-run setup and tool installs, the shared shell config (exchanges, membership and trust questions), the push sync for older goattyds, PATH commands, the welcome window, pick-from-list commands, the two confirmation dialogs, coding agents, the bundled Nerd Font |

View tree: `Workspace` → tab bar (or, vertical, the sidebar beside the panes card) + the active tab's panes (`TerminalView`s, absolutely positioned from `layout::Node::layout`). A `TerminalView` never touches the window directly: it emits `TerminalEvent`s and the workspace decides what they mean.

### Behaviour that isn't obvious from the code

- **Tabs and panes** (#30). Every pane is its own session from the window's `Spawner`, started in the focused pane's OSC 7 directory off the UI thread. Closing a pane kills its session. *Detaching* a pane, tab or host unsubscribes without killing it. Pane shortcuts are actions on the workspace's root div; Alt+digit/arrow `cx.propagate()` when there's no such tab or pane so the key reaches the shell. Background tabs aren't rendered, so their output costs no frames.
- **Workspaces** (#215, [`workspaces.md`](workspaces.md)). Each host a window has tabs on shows one of that goattyd's workspaces, shared live with every other window showing it. A workspace saves its tabs on every change (`Workspace::save_layout`): tabs, splits, names. The active tab, focus, zoom and window size are per window and never go in the shared layout. Saves are revisioned. A conflict or another window's `LayoutChanged` is merged per tab (`saved::merge`) and applied by `reconcile`. Switching parks the left workspace's tabs with their sessions attached.
- **Restoring windows** (#252). A goattyd only knows its own workspaces, so the app records which goattyds each window has tabs on (`workspace/reopen.rs`). With `workspace.on_launch = reattach`, plain `goatty` reopens each recorded window whose process is gone. Concurrent connects to one host share one attempt (`hosts::pool::open`).
- **Activity markers** (#122). A pane emits `TerminalEvent::Activity` for a bell, a finished command (OSC 133;D) and new output (not the attach snapshot). The workspace keeps the strongest per background tab (bell > failed > finished > output) and clears the active tab's.
- **Foreground program.** `goatty_term::process::ForegroundWatch` checks the PTY's foreground process group shortly after output or input, never while idle. goattyd sends changes as `ServerMsg::Process`. Tabs use it for the icon and as the title fallback.
- **Links** (#27). `Terminal::link_at` finds the OSC 8 link, URL or path under a cell, on the whole wrapped line, only when asked (mouse move or modifier change), so idle and flood frames pay nothing.
- **Output viewer.** `Terminal::transcript` copies the scrollback and screen as logical lines with resolved colors, split into commands by the OSC 133 marks. `Terminal::feed` applies each mark at the cursor it was printed at, so it splits the chunk at marks. Reattach and resync snapshots replay the marks, soft wraps and the shell state, so a reattached client sees the same commands. Without shell integration only "All output" works. A live view reads the transcript at most every 250 ms (cost: `stage0-findings.md`). The terminal window has no focus pool, so the viewer moves focus between its controls itself.
- **Prompt jumps** (#28). `Terminal::scroll_to_prompt` returns `NoPrompts` on the alternate screen or when the shell never marked a prompt, and the view then propagates the key to the program. A client mirror holds only the history it fetched, so a jump can return `Fetching` and resume from `on_wake` when `Session::fill_history` delivers.
- **Paste** (#29). Every paste goes through `goatty_term::paste::check`; `paste.confirm` decides which findings ask first.
- **Feedback.** Anything that finishes without a visible result reports through `crate::toast` (`Toast::loading` then `toast::update`). Give repeated feedback a `key` so it replaces itself. Toasts never take focus. In the settings window, the bar is for unapplied host changes and the outcome of a preview, apply, rollback or import.
- **Input.** GPUI dispatches a bound action before the view's raw `on_key_down`. Shortcuts are actions in `actions.rs`, and anything unbound goes to the shell through `terminal::keys` and `goatty_term::input`. Don't add shortcuts as `match` arms in `on_key`. A key `on_key` sends stops propagation. IME text comes through `TerminalView`'s `EntityInputHandler`, registered by `TerminalElement::paint`. A plain key without text (`key_char` none) is part of a dead-key or compose sequence and sends nothing.
- **Coding agents** (#205): see [`coding-agents.md`](coding-agents.md).
- **Images** (kitty graphics, sixel, iTerm2). rio-vt parses them, keeps the placements and answers queries; `goatty_term::graphics` keeps the pixels (from rio-vt's upload queue), turns what's visible into `Frame::images` (in cells, with a layer), and writes images into attach and resync snapshots (each image once, each placement while its row is replayed). Kitty Unicode placeholder cells are blank in the frame; their image pieces come from the placeholders. A program may send an image as a file, temporary file or shared memory, which only the daemon can read and which it deletes; goattyd's output pump runs `MediaRewriter` first, so every terminal gets the data inline (`t=d`). The renderer keeps one texture per `Image` (`terminal/images.rs`) and frees it the first frame it isn't shown. Restore snapshots leave images out.

## Where new work goes

| Work | Where |
|---|---|
| Tab/pane features | `workspace/` (`mod.rs` behaviour, `layout.rs` tree operations, `tab_bar.rs` looks); a program icon: `workspace/icons.rs` |
| Workspaces | the daemon store: `goatty-daemon/src/workspaces.rs`; client: `Connection::*_workspace` + `WorkspaceSink`; shared layout and merge: `workspace/saved.rs`; the window's side: `workspace/sync.rs`; something a window keeps per host: `reopen::HostRecord`. See [`workspaces.md`](workspaces.md) |
| Keyboard encoding, kitty protocol | `goatty-term/src/input.rs` (`encode_key_event`), fed from `terminal/keys.rs` |
| Images (kitty graphics, sixel, iTerm2) | what's shown and snapshots: `goatty-term/src/graphics.rs`; file/shared-memory transmissions: `goatty-term/src/media.rs`; `icat`: `goatty-term/src/icat.rs` (the subcommand in both CLIs, the shell function in `zshrc.zsh`); drawing: `terminal/element.rs` (`image_draws`, layers) and `terminal/images.rs` (textures); check with the `imgcheck` example |
| Coding agents, program status | protocol: `goatty-term/src/status.rs`, applied in stream order by `marks.rs`; a new agent: `agent/mod.rs` (+ logo in `assets/agents/`); how it shows: `workspace/tab_bar.rs`, `sidebar.rs`, `notices.rs`. See `coding-agents.md` |
| Shell integration, terminal side | `goatty-term`'s listener (OSC 7), `marks.rs` (OSC 133), `transcript.rs`; the zsh side is `goatty-shell/zsh/integration.zsh` |
| Output viewer | `output_view/`; what counts as a command's output: `goatty-term/src/transcript.rs` |
| Managed zsh, tool versions | `goatty-shell/zsh/zshrc.zsh` (load order matters, see `managed-zsh.md`); tools are built-in plugins: `goatty-shell/plugins/<id>/plugin.toml` (+ `builtins.rs`) |
| The shared shell config (#313) | model, merge, signatures, resolve: `goatty-config/src/shared.rs` (`scope = machine`: `HostSettings::MACHINE`); on a host (storage, `Shared` API, apply, plugins, relay): `goatty-shell/src/shared.rs`; the GUI (exchange on connect, membership and trust questions): `shared_sync.rs`; the scope picker and row markers: `settings_window/scope.rs`; Hosts page: `settings_window/hosts.rs`. See `profiles.md` |
| The push sync for older goattyds (#159) | `goatty-shell/src/sync.rs`; when goatty asks: `shell_sync.rs` |
| First-run setup, diagnostics | host side: `goatty-shell/src/setup.rs` (new operations go in its `Request`/`Reply`, no protocol bump); GUI: `welcome.rs`, `shell_setup.rs`, `settings_window/diagnostics.rs` |
| Bash/Fish adapters | `goatty-shell`: a sibling of `zsh.rs` + its startup files; `Launcher::launch` picks by program |
| New client setting | field in `goatty-config/src/client.rs` (+ `validate`), a `kit::row` on its page in `settings_window/` (+ `search.rs`), apply in `TerminalView::apply_settings` |
| New host setting | field in `goatty-config/src/host.rs`, render in `goatty-shell/src/zsh.rs`, a row in `settings_window/shell.rs` (+ `search.rs`) |
| Prompt option, Starship module | model and catalog in `goatty-config/src/prompt/`, `goatty-shell/src/starship.rs`, a row in `settings_window/prompt.rs` (+ `search.rs`) |
| A color setting | embed `Entity<ColorPicker>` (`ui/color_picker.rs`); parsing in `goatty-config/src/colorspace.rs` |
| New theme import format | `goatty-config/src/theme_import.rs` |
| Settings migration | bump `Document::VERSION`, add `(from, fn(&mut Value))` to `migrations()` |
| Something a session carries over a restart | `goatty-daemon/src/restore.rs`: a field in `SavedSession`/`State` (new fields need `#[serde(default)]`; a change older builds can't read bumps `STATE_VERSION`) |
| A palette command that asks for input | build a `command_palette::Ask` in a `Command::handler` and call `command_palette::ask`. Plugins: `[[command]]` (`goatty-plugin/src/commands.rs`) |
| Host list, switcher | `hosts.rs`, `command_palette` (`Scope::Hosts`/`Sessions`), `settings_window/hosts.rs`, `SshSettings` in `goatty-config`, `goatty-client/src/ssh_config.rs` |
| Snippets and expansions | format and rendering: `goatty-plugin/src/snippets.rs`; on a host: `goatty-shell/src/snippets.rs` and `zsh/expand.zsh`; GUI: `snippets/`, `settings_window/snippets.rs`. See [`snippets.md`](snippets.md) |
| A plugin feature (manifest field) | `goatty-plugin/src/manifest.rs` (+ validation, permissions if it can run code), `goatty-plugin/src/zsh.rs`, document it in `docs/plugins.md` and the template repo |
| A plugin operation (GUI/CLI) | `goatty_plugin::api::Request`/`Reply` (no protocol bump), `HostService::plugin_request`, `settings_window/plugins.rs` |
| Local echo prediction | `goatty-term/src/predict.rs`, driven by `Session::predict`/`ingest` in `goatty-app/src/session.rs`; see `remote-architecture.md` R3 |
| Several clients on one session | `Session::subscribe`/`resize`/`take_size` in `goatty-daemon/src/session.rs`; see `remote-architecture.md` §4c |
| Scrollback on demand, snapshots | `goatty-term/src/history.rs`, `snapshot.rs`; fetching: `Session::fill_history` in the app; see `remote-architecture.md` §4a |
| The tailnet | `goatty-net` (tested against `tests/fake-tailscale.sh`); a new host-side operation: `goatty_net::api::Request` + `Host::request`; client: `goatty-client/src/tailnet.rs`; GUI: `tailnet.rs`, `settings_window/network.rs`. See [`tailnet.md`](tailnet.md) |
| `goatty://` deep links | parser, per-process app socket (`app-<pid>.sock`/`app-active`), and dispatch: `goatty-app/src/links.rs`; UI targets: `settings_window/` (`mod.rs`, `search.rs`, `plugins.rs`). See [`links.md`](links.md) |
| New wire messages | `goatty-proto/src/msg.rs`: a new variant at the *end* (append-only), bump `PROTOCOL_VERSION`, mark it `vN:`; handle in `goatty-daemon/src/client.rs` and `goatty-client/src/connection.rs`, which must not send it to a daemon that agreed on an older version (`Connection::protocol_version`) |

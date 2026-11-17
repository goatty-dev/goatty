# goatty architecture

How the code is laid out, and where each upcoming stage's work goes. For the product plan see [`goatty-project-plan.md`](goatty-project-plan.md); for the remote design see [`remote-architecture.md`](remote-architecture.md).

## Crate graph

```
                 goatty-config      (profile model: client/host JSON documents, store, themes, keys, profiles, the prompt model)
                   ▲        ▲   ▲
                   │        │   │   goatty-plugin  (plugin format, permissions, signed registry index; bin goatty-plugin)
                   │        │   │      ▲
                   │        │   goatty-shell    (managed zsh: generated ZDOTDIR, tool installs, plugins on a host, diagnostics)
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
- **Client vs host settings** (#14): `ClientSettings` (`settings.json`) is read by the app; `HostSettings` (`host.json`) by goattyd/`HostService` on its own host. The GUI edits host settings through the protocol (`GetHostSettings`, `SetHostSettings`, …), never by writing a remote file. See [`profiles.md`](profiles.md).
- **`goatty-plugin` knows the format, not the host.** Manifests, settings schema, templates, permissions, zsh rendering, the signed index and the JSON API live there with no knowledge of `Dirs`; `goatty-shell::plugins` decides where things go. The authoring/registry CLI (`goatty-plugin`) depends on nothing else in the workspace, so registry CI builds it quickly. See [`plugins.md`](plugins.md).
- **Settings come from `goatty-config`.** Don't read `GOATTY_*` variables elsewhere (the exceptions: `GOATTY_SOCKET`, a location, in `goatty_proto::socket_path`; `GOATTY_ASKPASS`, goatty's marker to itself as ssh's askpass, in `goatty_client::askpass`).
- **The tailnet** (#15, [`tailnet.md`](tailnet.md)): `goatty-net` knows Tailscale's CLI (`tailscale`), the device key (`identity`), pairing (`pairing`), the gateway handshake (`gateway`), byte streams to peers (`link`: TCP or `tailscale nc`), discovery, the `Net` JSON (`api`) and a host's availability with the gateway socket's accept loop (`host`). It knows nothing of the daemon or `Dirs`: callers pass `goatty_net::Paths` (`Dirs::tailnet_dir`). goattyd owns one `Host` (`goatty-daemon/src/net.rs`); `goatty_client::tailnet` connects (`Endpoint::Tailnet`), discovers and pairs. goatty only ever uses the user's installed Tailscale through its public CLI, and changes only its own Serve route.
- **Remote hosts** (`goatty-client`): `transport` (endpoints; ssh with captured stderr and askpass), `remote` (the connect script and its `goatty-remote:` probe line, `RemoteProblem`, install/stop over ssh, bundled binaries), `askpass` (decode ssh's question), `service` (systemd user units), `ssh_config` (the hosts in `~/.ssh/config`, for the host list). The app turns `RemoteProblem`s into questions in `connect.rs`. `local` does the same for this computer's goattyd: `outdated` (the running daemon's `/proc/<pid>/exe` is another, older file than the goattyd a restart would start: next to `goatty`, or the systemd unit's), `restart_daemon` (keeps the sessions from protocol v10 on, else stops it with `force`), `stop_daemon` (any protocol version; `goattyd stop --force` uses it too), `LocalProblem`. See [`remote-architecture.md`](remote-architecture.md) §5a/§5b.

## Daemon (`crates/goatty-daemon`)

| Module       | Owns                                                                     |
|--------------|--------------------------------------------------------------------------|
| `server`     | socket bind (0600 in a 0700 dir) or the socket from systemd (`LISTEN_FDS`), accept loop, socket watchdog, the session registry (`Daemon`: shells and `Run` commands; `start` for spawned and taken-over sessions alike), shutdown |
| `restore`    | sessions that outlive the daemon process (#131): `Restart` pauses every output pump, saves the screens/ids/layouts next to the socket (`<socket>.restore/`) and `execve`s the new goattyd with the PTYs and the socket inherited (`serve --restore FILE`, `take_over`); under systemd the screens are saved on SIGTERM and the PTYs come back from the fd store (`adopt_stored`) |
| `systemd`    | `LISTEN_FDS`/`LISTEN_FDNAMES` (the socket, the stored PTYs), `NOTIFY_SOCKET`: each PTY goes into the fd store when its session starts (`FDSTORE=1`, `FDNAME=goatty-s<id>-<created>-<m\|p>`) and out when it ends |
| `client`     | one connection: decode requests, its subscriptions, the writer thread    |
| `session`    | PTY + authoritative `Terminal` + subscribers with flow control, the output pump (`poll` on the PTY and a wake pipe, so a restart can `Pause` it between two reads) |
| `proxy`      | `goattyd proxy`: stdio ⇄ socket relay for remote transports              |
| `net`        | this host on the tailnet: the `goatty_net::host::Host` (made on start when `tailnet/state.json` exists, else on the first `Net` request), its gateway connections handed to `client::handle`, `Net` requests; `net_cmd.rs` is `goattyd net …` |
| `main.rs`    | clap CLI; control commands go through `goatty_client::Control`; `restart [--force]` (`goatty_client::local::restart_daemon`), `restore-version` (hidden: the state format it reads, checked before a restart runs it); `stop --force` (any protocol version, `goatty_client::local::stop_daemon`), `service install|uninstall|status`, `completions SHELL` (`serve` also writes `_goattyd` for the managed shell) |

Threads per session: `s{id}-reader` (PTY → terminal → subscribers) and `s{id}-replies` (query answers → PTY). Per connection: a reader (the accept thread's spawn) and `c{conn}-writer`.

## Shell environment (`crates/goatty-shell`)

| Module    | Owns                                                                    |
|-----------|-------------------------------------------------------------------------|
| `launch`  | `Launcher`: resolve the shell program, write the managed ZDOTDIR, build the `Launch` (program, args, env) |
| `zsh`     | render `settings.zsh`, the plugin slot files (`plugins-*.zsh`) + the static startup files in `zsh/`, write them atomically |
| `starship`| `starship.toml` from the prompt settings: the layout as format strings, each module's preset, symbol, color and format; the transient prompt's zsh string |
| `builtins`| the plugins that come with goatty (`plugins/<id>/`, compiled in): fzf, fzf-tab, autosuggestions, syntax highlighting, Starship |
| `packages`| where a plugin's package is: system copy, goatty's managed copy, or missing |
| `install` | download (curl/wget), verify, unpack, activate `bin/` and `plugins/` symlinks, uninstall packages |
| `completions` | zsh completion functions in `share/zsh/site-functions/` (on fpath): goatty's and goattyd's own (written when they start), packages' `completions` command after an install (removed with the package, backfilled when goattyd starts); drops the compinit dump on changes |
| `doctor`  | start a real managed shell and parse `goatty-status` (or what another command prints) |
| `import`  | importing the user's own shell setup (`shell.import`): `Context` (home, login shell, environment; injectable), `source` (which shell), `harvest` (run it without a terminal, parse the NUL-separated dump, keep changed variables, the PATH folders it added, aliases marked as the user's own or a framework's), `offer_aliases` (the one-time copy into goatty's aliases), `prepare` (fingerprint the startup files: cached, harvest now, or in the background), `import.zsh`, `Status` for the Dotfiles page and `goattyd shell import` |
| `path_report` | the PATH a new shell gets and where each folder came from (the startup files' `_goatty_path_marks`), what became of each `shell.path` folder; the PATH page's `path` request |
| `generation` | build → validate (`zsh -n`) → activate generations; list, rollback, diff; `info.json` |
| `history` | what each generation changed against its base (settings, plugins, files), one-line summaries, names, file diffs |
| `sync`    | bringing this computer's shell setup to another host (#159): `LocalSetup` (settings without machine-specific values, installed plugins, registries with keys; no network) and its fingerprint, `fetch` a host's setup through the `Remote` trait (the GUI's `HostLink`, or a `HostService`), `plan` (settings to apply, plugins to install/update from the same signed registry, tools, skips with reasons), `Step::run` |
| `setup`   | shell setup and diagnostics as JSON (`Request`/`Reply`, protocol v9 `Shell`): `Status` (configuration, a real startup, the built-in tools), install built-in tools, the offered/dismissed markers, the generation history (list with changes, name, file diff); `goattyd shell status` prints the same `Status` |
| `service` | `HostService`: host settings get/set(preview, apply)/generations/rollback, plugin requests (snippets and suggestions too), for the daemon API and `goatty --local` |
| `snippets` | snippets and expansions on this host (#89): the user's own from `host.json` and the active plugins', with the programs the host lacks (`list`), suggestion sources run from the host's own definition (`suggest`: managed PATH, pane directory, no shell for plugins, timeout, output cap, process group killed), `expansions.zsh` (`render_expansions`); `snippet_cmd` is `goattyd snippet …` |
| `cmd`     | `goattyd shell status|install|uninstall|dismiss|generations|name|diff|rollback` |
| `plugins` | plugins on this host: `registry` (trusted sources, fetch → verify → snapshot), `lock` (installed files + approvals), `Plugins` (find, plan, install, uninstall, catalog; built-in ones included), `active` + `load_order` (what a generation renders, in slot order), `cmd` (`goattyd plugin …`) |
| `dirs`    | XDG locations under `goatty/`                                           |

Design and startup sequence: [`managed-zsh.md`](managed-zsh.md). The zsh behaviour is tested against real zsh in `tests/zsh_startup.rs` (sandboxed `$HOME`, PTY for OSC 133/7).

## App (`crates/goatty-app`)

```
main.rs        parse CLI (subcommands run without a window; `completions SHELL`; writes `_goatty` for the managed shell at start), pick what the window starts with (a connect plan, or a local PTY for `--local`), app::run
commands.rs    `goatty settings …` / `goatty theme …` (headless)
cli.rs         clap definition
fonts.rs       the bundled JetBrainsMono Nerd Font (assets/fonts, `scripts/update-nerd-font.sh`): registered at start, the default terminal font when no installed Nerd Font comes first, the fallback of every terminal font, and the tab icons' font
app.rs         GPUI bootstrap: settings, terminal windows (`Initial`: ready, or connecting in the window), OpenSettings handler
settings.rs    AppSettings global: client settings file ⇄ live values, file polling, keybindings; theme previews (`preview_theme`: shown, never saved, other edits save the theme from before it; `end_theme_preview` confirms or undoes)
actions.rs     actions, name → action map, loading user keybindings (Terminal key context)
command_palette/
  mod.rs       the Ctrl+Shift+P overlay: ranking (fuzzy + keywords + recently used), list, keys; hosts once something is typed; `Scope::Hosts` (Ctrl+Shift+H, the host switcher): this computer and the hosts with user@host/OS/RTT when connected, plus a typed user@host; `Scope::Sessions` ("Sessions on this host…", `list_sessions`): a host's sessions (fetched in the background) and a new one, opened per `ssh.open_in` (Shift+Enter: the other way), or ended without opening (Shift+Delete or the row's trash button, twice)
  registry.rs  Command (id, category, title, keywords, action or handler), CommandRegistry global, built-ins
  fuzzy.rs     fzf-style matcher: word starts, consecutive runs, gaps; any word order; highlight positions
  input.rs     asking for input (#91): `Ask` (title, `Field`s, answer handler) and `ask(window, …)`; a `Field` is the plugin format's `CommandInput` (text/integer/number/path/folder/choice, label, placeholder, default, limits, pattern) plus an extra check and suggestions (choices, hosts, folders listed off the UI thread when the host is this computer); the palette shows one field at a time (Enter next, Esc back, Tab fills in) and emits `PaletteEvent::Answered`; lists to pick from (#79): `Field::list`/`loaded_list` (items from a `Loader` off the UI thread: a plugin's `[command.input.source]`), `Ask::pick` for a one-step pick, `Field::on_select` (live preview as the selection moves) and `Ask::on_cancel` (runs when the palette closes unanswered: Esc restores)
settings_window/
  mod.rs       the settings window: pages, navigation, host draft → preview → apply, themes, export/import, rollback
  host.rs      HostLink: the daemon connection or the local HostService
  kit.rs       the pieces pages are built from: section/row, dropdown (`select`), search field, chips, choice cards, modal; search-result highlight + scroll
  chrome.rs    sidebar (rail + drawer below 920 px), page header, the floating bar for unapplied host changes, `Render`
  search.rs    "Search settings": the index of every setting (row label, section, keywords) + pages + shortcut actions
  appearance.rs  live preview, theme gallery + color editor, font picker, cursor, padding per side, opacity
  terminal.rs  the Terminal page (links, selection, mouse: what a right click does, pasting) and the Workspace page (startup, tabs, panes, notifications)
  shell.rs     Shell, History
  dotfiles.rs  Dotfiles: the import of what the user's shell sets up (on/off, which shell; the last check with search, filter chips and a switch per PATH/variable; failures with stderr; "Check again" runs it on the host), a link to copying the aliases, and (advanced) running ~/.zshenv, ~/.zprofile and (guarded) ~/.zshrc in goatty's shell
  path.rs      PATH: your folders (`shell.path`: add with a live check, browse when local, reorder, remove; each folder's state on the host) and the PATH a new shell gets, each folder with its origin, searchable, "Doesn't exist" filter
  prompt.rs    Prompt: live preview, style cards (Starship/Lean/Classic/Rainbow), symbols, segment ends, layout (frame, connection, transient), directory, prompt character, colors (`color_select`: theme colors, grays, custom); the module list as a sub-page (search, filters, categories) and a module's dialog (side, symbol, color)
  shortcuts.rs the Shortcuts table: groups or sorting, search by name or pressed keys, changed/unbound filters
  aliases.rs   the Aliases page: mine first, plugin groups folded, filters, the add/edit dialog
  snippets.rs  the Snippets page: mine, the plugins' folded (make my own copy), expansions with their switch and a live "Try it"; the editor turns {name}s in the line into input rows; "Save the last command as a snippet" lands here
  plugins.rs   the Plugins page: installed list, each plugin's own page (schema-driven settings by group), browse, install review
  diagnostics.rs  the Diagnostics page: the host's `setup::Status` (features and their source, startup time, startup errors, your files), install missing tools, copy report
  hosts.rs     the Hosts page: open hosts in a tab or window, add/remove hosts, plain SSH per host, `~/.ssh/config` discovery
  network.rs   the Network page (#15): the active tab's host on the tailnet over `Net` (Tailscale's state, availability, port, the missing-permission commands, the pairing window with codes to confirm, paired devices with revoke; refreshed every second while pairing is open), and this computer as a client (discovery switches, paired hosts with forget)
  profile.rs   files (copy path, open folder), problems, export/import, generations (summary, details: every change, file diff, name), reset
  about.rs     version, connection, links, "copy for a bug report"
output_view/
  mod.rs       the output viewer over a pane (Ctrl+Shift+F: all output, searched; Ctrl+Shift+L: the last command's): sources (last/running command, an earlier one, all), search with highlights and "Only matching lines", wrap toggle, selection and copy, live refresh while a command runs; owned by `TerminalView`
  element.rs   its text area as one element: visible rows only, line numbers, backgrounds, matches, selection, scrollbars
  layout.rs    columns and wrapping (pure, unit-tested): character widths, wrap points (after a space when it's worth it), column windows for sideways scrolling, row ↔ line mapping
  search.rs    plain or regex search through `regex`, case-insensitive by default (pure, unit-tested)
toast/
  mod.rs       Toast (level, title, body, buttons, key, lifetime) and the API: show/show_in/update/dismiss/dismiss_all/press_primary, routed to a window's layer (terminal windows, and the settings window while it's active); every toast is recorded in the history
  stack.rs     one window's toasts without UI: replace by key, eviction, pausable timers, the newest main button (unit-tested with explicit clocks)
  layer.rs     ToastLayer view: bottom-right cards, spring enter/exit, pause on hover / inactive window, one timer task; cards are live regions (alert/status) for screen readers
  history.rs   the last 50 notifications without handlers (results, not progress; updates and keys replace their entry; unit-tested)
  recent.rs    "Notifications: Show recent": an overlay like the palette that lists, filters and copies the history
ui/
  style.rs     UI colors derived from the terminal theme
  icons.rs     Icon enum + Assets (SVGs from assets/icons/, embedded, served to svg()); `Icon::by_name` for plugins' menu icons
  context_menu.rs  the right-click menu widget (#186): `Entry`s (items with icon, label, shortcut, disabled reason; separators, headings, notes; one level of submenus), keyboard (arrows, first letter, Right/Left for submenus, Esc) and screen readers (menu items, active descendant, shortcuts), a backdrop that swallows the click outside; emits `MenuEvent::Run(handler)` for its owner to run after closing
  controls.rs  toggle, segmented, stepper, button, rows
  text_input.rs single-line text field (IME-aware; adapted from GPUI's example)
  prompt_preview.rs draws a prompt from `PromptSettings::layout` with sample values (Prompt page, style cards, welcome window)
  color_picker.rs color picker widget: `Entity<ColorPicker>` emitting `Changed`/`Committed`; `Pick` (float state), `Wheel` (HSV) and `Disc` (OKLCH, a generated bitmap) geometry, painting, drag and arrow-key handling (`Knob`s are tab stops), lighter/darker nudges, the contrast row (`ContrastRef`, filled per slot by the settings window)
workspace/
  mod.rs       root view per window: tabs of split panes, each tab on its own host (`Spawner`), connecting tabs (connect screen until the session is there), a saved layout per goattyd, spawning/closing panes, window title, closes with the last pane; command palette, paste confirmations, toast layer, recent notifications
  layout.rs    a tab's split tree (pure, unit-tested): n-ary splits with weights, split/remove/normalize, layout → leaf rects + dividers, neighbor, resize and divider drags by a share of the tab with a minimum pane size (`MinSize`), swap, rotate, join two trees, equalize all or one split
  arrange.rs   rearranging (#124): swap and rotate panes, move a pane to a new tab or another tab on the same host (`Spawner::same_host`), bring a tab into another one, the palette's "which tab?" questions; the minimum pane size (12×3 cells) and the resize step (2 columns / 1 row) from the cell size and the panes' painted bounds; a split that wouldn't fit is refused with a toast
  tab_bar.rs   the horizontal strip (`tabs.style`: modern / compact; scrolls when the tabs don't fit) and what every style shares: switch, close, rename, live drag to reorder, the mouse wheel (`tabs.scroll_to_switch`), activity markers on background tabs, coding agents' logos, state marks (spinner, waiting badge, done/failed) and progress lines
  notices.rs   what panes have to tell (#205): agent and program notifications as a per-pane toast (replaced in place, dismissed when the pane is looked at or the agent moves on) and desktop notifications (spaced out per pane), per `notifications.*`; `show_pane` for their "Show" button and clicks
  groups.rs    tabs grouped by host once a window has a tab on another host: each host's tabs kept together (`group_order`, `regroup` on every activation), a label per host in a color from its name (`color_slot`, the theme's ANSI colors; this computer is uncolored), folding a group (the active one switches to the nearest other group first; next/previous tab and the wheel skip folded tabs), show only one host / every host, next/previous host (to the tab last active there), a new tab on a group's host; `strip_items` is what the strip and the sidebar list
  sidebar.rs   the vertical style (#168): a sidebar of rows (icon, title, directory and host) that folds into a rail of icons, resized or folded by dragging its edge (saved shortly after the drag stops), next to the panes drawn as a rounded card
  menu.rs      the right-click menus on `ui::context_menu`: a tab's (new tab, rename, duplicate, move left/right, "Move into tab", close it / the others / the ones after it), a pane's (`TerminalView::menu_entries`: link, clipboard, the command line at a goatty zsh prompt, output; then split, a Pane submenu: split left/up, swap with a neighbour, rotate, zoom, equalize, close; "Move pane to": a new tab or another tab; plugin commands with `[command.menu]`; palette and settings; a note in programs that use the mouse) and a divider's (even out that split or all; double-clicking a divider does the first)
  gestures.rs  the tabs' pointer maths (pure, unit-tested): where a dragged tab goes, wheel notches and touchpad distance → tab switches, the sidebar's width limits and fold thresholds
  icons.rs     process name → Nerd Font glyph + theme color, drawn with the bundled font (tests check every code point against it)
  saved.rs     the window's layout as goattyd keeps it (JSON: tabs, split trees with session ids, names, focus, zoom, window size and state); `restore` claims and reattaches it
agent/
  mod.rs       coding agents (#205): `Agent` (known by OSC 7501 `app` or the foreground program), their logos (`assets/agents/`, in color or tinted), `AgentStatus` (the top record for tabs, done/error hidden once seen; a notice from a news record)
  notify.rs    `Notice` (an agent waiting/done/failed, a program's OSC 9/777), its texts, when it goes out (`applies`), desktop notification tags and clicks
pickers.rs     goatty's own pick-from-a-list commands on `Ask::pick` (#79): "Themes: Switch theme…" (live preview through `AppSettings::preview_theme`, Esc restores; following the system it changes the dark or light theme in use), "Themes: Dark, light or follow the system…", "Shell: Install a built-in tool…" (the active host's missing or outdated tools, then `shell_setup::start_install`)
close_confirm.rs  "Close this pane?" dialog when a program other than the shell still runs (`panes.confirm_close`, "Don't ask again")
connect.rs     ConnectView, the connect screen in a tab: connects off the UI thread (local daemon or remote), turns RemoteProblem into questions (install / update and restart / plain SSH) and LocalProblem too (a local goattyd that's incompatible or an older build than the installed one: restarted right away when that keeps its sessions or it has none, else asked), hands the tab its session or restored layout
snippets/
  mod.rs       the active host's snippets (`Store`: `Request::Snippets`, refetched on host and plugin changes), the fill-in steps (`ask_for`: palette input mode with the line as preview, suggestions via `Request::Suggest`), putting the line on the prompt, "Save the last command as a snippet"
  picker.rs    the Ctrl+Shift+S overlay: fuzzy search, topic chips, recently used first, missing programs last, preview with blanks and the safety badge
typing.rs      which pane goatty may type a command into (the focused one, on the command's host, at a prompt); otherwise a toast offers to copy it (plugin commands and snippets)
plugin_commands.rs  the active host's plugins' `[[command]]`s in the palette (`plugin.<id>.<cmd>`): catalog fetched off the UI thread when the host link changes or plugins change; picking one asks for its inputs and types the quoted line into the focused pane (a busy pane: offer to copy it); `MenuCommands`: those with a `[command.menu]`, for the pane menu (`start` fills in the selected text); inputs with a `[command.input.source]` load their list from the host (`Request::CommandSuggest`, in the focused pane's directory)
shell_path.rs  "Shell: Add/Remove a folder to/from PATH…": asks, then changes `shell.path` on the active tab's host through its goattyd (the PATH page edits the same setting)
tailnet.rs     computers on the user's tailnet (#15): `TailnetList` global (paired hosts + the last discovery, refreshed with `hosts::refresh`), their palette/switcher commands (`tailnet.host:<dns>` connects, `tailnet.pair:<dns>` pairs), pairing (`pair`: a toast with the code, "Codes match"/"Cancel", pinned once both sides confirmed), forget
hosts.rs       hosts to connect to: `ssh.hosts` + `~/.ssh/config` (`HostList` global, read in the background), palette commands, `pool` (one shared Connection per host), `open` → a connecting tab of the last active window or a new window (`ssh.open_in`)
shell_setup.rs installing a host's built-in tools with progress (`install`), the per-host install offer toast after a tab connects (`offer`)
shell_sync.rs  bringing this computer's shell setup to the host a tab connects to (`on_connect`: compare off the UI thread, ask with a toast once per change made here, or sync right away per `ssh.sync`/`ssh.hosts[].sync` and after a fresh goattyd install), the progress/result toast, "Sync shell setup to this host" (`sync_now`)
welcome.rs     the welcome window (first-run setup): theme and text size, the host's tools, importing the PATH and variables your .zshrc/.bashrc sets up (on by default; shown when you have one), prompt style, shortcuts
askpass.rs     goatty as ssh's SSH_ASKPASS: one window per prompt (passphrase, host key, confirm), answer on stdout
paste_confirm.rs  "Paste this?" dialog: risks, preview with control/invisible chars marked, paste as is / cleaned / as one line
session.rs     a pane's data: local PTY or goattyd session → mirror Terminal; coalescing waker; foreground program; local echo prediction; `Spawner` for a tab's new panes (goattyd, local PTY, plain `ssh host`)
terminal/
  view.rs      TerminalView: input → terminal, actions, emits TerminalEvent
  element.rs   custom grid Element (prepaint/paint)
  glyphs.rs    per-cell glyph cache
  keys.rs      GPUI keystroke → terminal KeyEvent (pure, unit-tested)
  links.rs     what a detected link means here (URL, other scheme → ask, local file if it exists, remote path → copy), file openers
  stats.rs     GOATTY_STATS frame statistics
```

View tree: `Workspace` → tab bar (or, vertical, the sidebar beside the panes card) + the active tab's panes (`TerminalView`s, absolutely positioned from `layout::Node::layout`). A `TerminalView` never touches the window directly. It emits `TerminalEvent::{TitleChanged, Exited, ConfirmPaste}` and the workspace decides what that means (tab and window title, close the pane, its tab or the window, show the paste dialog and hand the chosen text back via `TerminalView::paste_text`).

Tabs and panes (#30): every pane is its own session from the window's `Spawner` (another `Create` on the same `Connection`, or another local PTY), started in the focused pane's OSC 7 directory and created off the UI thread. Closing a pane kills its session; closing the window only detaches. Closing asks first (`close_confirm.rs`) when a pane's foreground program isn't a shell and `panes.confirm_close` is on. Pane and tab shortcuts are actions handled on the workspace's root div (they bubble up from the focused `TerminalView`); Alt+digit / Alt+arrow `cx.propagate()` when there's no such tab or pane, so the key still reaches the shell. Dividers are GPUI drags (`DividerDrag` + `on_drag_move` on the pane area). Background tabs aren't rendered, so their output costs no frames.

Tabs (#168): a dragged tab (`DraggedTab`) moves while it's dragged. Every tab has an `on_drag_move`, and the one under the pointer moves it once the pointer passes its middle (`gestures::reorder_target`). `moved_unpainted` skips moves until the tabs are laid out again, because their bounds are stale until then. The active tab is scrolled into view (`tab_scroll`) whenever it changes. The vertical sidebar's edge is a GPUI drag too (`SidebarDrag`, followed on the root): its live size stays in the workspace and is saved to `tabs.sidebar_width` / `tabs.sidebar_collapsed` 400 ms after it stops and when it's dropped. Ctrl+Shift+B (`toggle_tab_sidebar`) folds it and propagates with horizontal tabs.

Restoring windows (#118): the workspace sends its layout with `Connection::save_layout` (protocol v7 `SaveLayout`, fire-and-forget, only when the JSON changed, re-sent after a reconnect) whenever tabs, splits, sizes, names, focus or zoom change, and 500 ms after the window's size or maximized/fullscreen state settles. The window's size and state (not its position, which Wayland doesn't let a window pick) come back with the layout (#128): the window opens at the default size and resizes when the layout arrives, but only if the connecting tab is its only tab (restoring into a window with other tabs leaves its size alone). The JSON (`workspace/saved.rs`, `version` 1) is opaque to goattyd, which only keeps the session list and which connection holds the layout (`goatty-daemon/src/layouts.rs`). A connection ending frees its layout. `goatty` in reattach mode sends `ClaimLayout`: the daemon hands out the newest free layout that still shows a running session, to one window only (a second window gets the next free layout, else the newest detached session as before, else a new shell). Sessions that ended drop out of the tree (`Node::map_leaves`). `--new`, `--attach` and `--local` start a fresh layout.

Coding agents (#205): see [`coding-agents.md`](coding-agents.md).

Activity markers (#122): a pane emits `TerminalEvent::Activity` for a bell, a finished command (OSC 133;D, from `goatty-term`'s mark scanner, failed when the status isn't 0) and new output (`bytes_in` grew; output from before the view existed, such as the attach snapshot, doesn't count). The workspace keeps the strongest one per background tab (bell > failed > finished > output) if `tabs.mark_bell` / `tabs.mark_activity` want it. Rendering the workspace clears the active tab's marker, however it became active.

Foreground program: `goatty_term::process::ForegroundWatch` checks the PTY's foreground process group (`tcgetpgrp` → `/proc/<pid>/comm|cmdline`; libproc's `proc_name` on macOS) shortly after output or input, never while idle. goattyd sends changes as `ServerMsg::Process` (v6; also right after `Attached`, and in `SessionInfo.process`); `--local` watches its own PTY. Tabs use it for the icon and as the title fallback (app title → program unless it's a shell → directory name).

Links (#27): `goatty_term::Terminal::link_at(col, row)` finds the OSC 8 hyperlink, URL or path candidate under a cell, on the whole wrapped line, only when asked (mouse move / modifier change / new output while hovering), so idle and flood frames pay nothing. `terminal/links.rs` resolves it (relative paths against the OSC 7 directory, which rio-vt only accepts from this host) and the view underlines it, shows the target at the bottom of the pane, and opens it on Ctrl+click (`links.open_on`).

Output viewer: `goatty_term::Terminal::transcript` copies the scrollback and screen as logical lines (soft wraps joined) with resolved colors, split into commands by the prompt marks. `Terminal::feed` applies each OSC 133 mark at the cursor it was printed at (it splits the chunk at the marks): `A` marks the row `Prompt`, `C` marks the rows between that prompt and the cursor `PromptContinuation`, using rio-vt's per-row `semantic_prompt`, which travels with the row into the scrollback and through reflow. A command's output is the lines after its prompt lines up to the next prompt; without shell integration only "All output" works. Reattach and resync snapshots replay the marks, the soft wraps (scrollback and screen) and the shell's state and how many commands started (`OSC 133;C;goatty=prompt|run|done;n=<count>`, which only sets them; the count tells commands apart where transcript indices shift), so a reattached client finds the same commands. The shell state is prompt / running / finished: a finished command without a prompt after it yet is still "the last command". The viewer covers the pane (`TerminalView` renders it instead of the grid and forwards output to it). The terminal window has no focus pool (that's the settings window's `ui::focus`), so the viewer keeps one focus handle per control and moves between them on Tab/Shift+Tab itself, skipping disabled ones; a live view reads the transcript again at most every 250 ms (`stage0-findings.md` has the cost).

Prompt jumps (#28): `Terminal::scroll_to_prompt` scrolls so the `Prompt` row above (or below) the view's top row becomes the top row, past the newest one to the bottom (Ctrl+Shift+Up/Down, `previous_prompt`/`next_prompt`). It returns `NoPrompts` on the alternate screen or when the shell never marked a prompt, and the view then `cx.propagate()`s the key to the program; at the oldest prompt or the bottom (`Boundary`) a toast says so. A client mirror only holds the history it fetched: with no prompt above the view but older lines on the daemon (`Terminal::older_history_missing`), `Previous` scrolls to the oldest row held and returns `Fetching`; the redraw fetches near the view (`Session::fill_history`), and each fetch's wake resumes the jump in `on_wake` until it lands. Typing or scrolling cancels it.

Paste (#29): every paste goes through `goatty_term::paste::check` (runs commands without bracketed paste, control or invisible characters, `curl … | sh`, > 256 KiB, multiline); `paste.confirm` decides which of those ask first.

Feedback: anything that finishes without a visible result (a palette command, a background request, later plugins) reports through `crate::toast`: `toast::show(cx, Toast::success(…))`, or `Toast::loading` first and `toast::update` with the result. Give repeated feedback a `key` so it replaces itself (in the history too: a key keeps one entry there). Toasts never take focus: from the keyboard, `dismiss_notifications`, `press_notification_button` and `show_recent_notifications` (bindable, unbound by default, since Escape belongs to the terminal) dismiss them, press the newest one's main button, or list them again. In the settings window, short results are toasts; its bar is for unapplied host changes and the outcome of a preview, apply, rollback or profile import.

Input: GPUI dispatches a bound action before the view's raw `on_key_down`. Shortcuts are actions in `actions.rs`, and anything unbound goes to the shell through `terminal::keys` and `goatty_term::input`. Don't add shortcuts as `match` arms in `on_key`. A key `on_key` sends stops propagation; otherwise the platform would also hand its text to the input handler. Text that only the input method produces (IME commits, dead keys, the compose key; #21) comes through `TerminalView`'s `EntityInputHandler`, which `TerminalElement::paint` registers: commits go to the shell as plain UTF-8, and the composition (`preedit`) is drawn underlined over the cursor (`Frame::caret`, also while the cursor is hidden), which is also where the IME popup goes. gpui doesn't reset the input method when focus moves to another pane, so a composition finishes in the pane it started in (the `Composing` global). A plain key without text (`key_char` none) is part of a dead-key or compose sequence and sends nothing; only Ctrl/Alt combos fall back to the key name.

## Where upcoming work goes

| Work                                   | Where                                                                 |
|----------------------------------------|-----------------------------------------------------------------------|
| Tab/pane features (layout restore, titlebar tabs, …) | `workspace/` (`mod.rs` for behaviour, `layout.rs` for tree operations, `tab_bar.rs` for looks); a new program icon: `workspace/icons.rs` |
| Box drawing (#24)                      | `terminal/element.rs` (+ a new `terminal/boxdraw.rs`)                 |
| Keyboard encoding, kitty protocol (#22) | `goatty-term/src/input.rs` (`encode_key_event`: legacy xterm, or kitty once the program pushes flags; the flags are `TermMode::kitty_flags`, read from rio-vt's per-screen stack, which also answers `CSI ? u`; snapshots carry both screens' top flags, the primary's noted by the mark scanner just before each screen switch, but not the levels below), fed from `terminal/keys.rs` (GPUI keystroke → `KeyEvent`); `TerminalView::on_key`/`on_key_up` send repeats and the releases of keys whose press the program got. Not reported: lone modifier keys, Caps/Num Lock, keypad keys as their own keys |
| Coding agents, program status (#205)  | the protocol: `goatty-term/src/status.rs` (OSC 7501 parsing, limits, records, snapshot replay; OSC 9;4 progress), applied in stream order by `marks.rs`/`Terminal::feed` (the daemon answers the probe); a new agent: `agent/mod.rs` (+ its logo in `assets/agents/`, `goatty-term/src/process.rs` when it runs under a runtime); how it shows: `workspace/tab_bar.rs`, `sidebar.rs`; notifications: `agent/notify.rs`, `workspace/notices.rs`. See `coding-agents.md` |
| Shell integration, terminal side (#28) | `goatty-term` listener (OSC 7 → `UiEvent`), `goatty-term/src/marks.rs` (OSC 133, which rio-vt doesn't act on → row marks, `UiEvent::CommandFinished`, `Terminal::command_running`), `goatty-term/src/transcript.rs` (commands and their output) + daemon `SessionInfo`; the zsh side is in `goatty-shell/zsh/integration.zsh` |
| Output viewer features (export, more sources) | `output_view/mod.rs`; what counts as a command's output: `goatty-term/src/transcript.rs` |
| Managed zsh features, tool versions    | `goatty-shell/zsh/zshrc.zsh` (load order matters, see `managed-zsh.md`); the tools are built-in plugins: `goatty-shell/plugins/<id>/plugin.toml` (+ `builtins.rs` to list a new one) |
| Syncing the shell setup to hosts (#159) | what gets copied and the trust rule: `goatty-shell/src/sync.rs` (pure, unit- and end-to-end-tested); when goatty asks: `shell_sync.rs`; per-host choice: `SshSettings::sync_mode`; answers: `goatty_config::sync_state` |
| First-run setup, shell diagnostics     | host side: `goatty-shell/src/setup.rs` (new operations go in its `Request`/`Reply`, no protocol bump); GUI: `welcome.rs`, `shell_setup.rs`, `settings_window/diagnostics.rs` |
| Bash/Fish adapters (Stage 6)           | `goatty-shell`: a sibling of `zsh.rs` + its startup files; `Launcher::launch` picks by program |
| New client setting                     | field in `goatty-config/src/client.rs` (+ `validate`), a `kit::row` on its page in `settings_window/` (+ an entry in `search.rs`), apply in `TerminalView::apply_settings` |
| Prompt option / a new Starship module  | model, layout and module catalog in `goatty-config/src/prompt/` (`modules.rs` from Starship's defaults and presets), `goatty-shell/src/starship.rs`, a row in `settings_window/prompt.rs` (+ `search.rs`); the preview follows the layout by itself |
| New host setting                       | field in `goatty-config/src/host.rs`, render it in `goatty-shell/src/zsh.rs` (`settings.zsh`/`starship.toml`), a row in `settings_window/shell.rs` (+ `search.rs`) |
| A color setting / editing a color      | embed `Entity<ColorPicker>` (`ui/color_picker.rs`): `open(color)` to start, `sync(color)` to follow outside changes, react to `ColorPickerEvent::Changed` (live; `AppSettings::preview`) and `Committed` (save). Parsing and formats: `goatty-config/src/colorspace.rs` |
| New theme import format                | `goatty-config/src/theme_import.rs` (detect + parse to `ThemeColors`) |
| Settings migration                     | bump `Document::VERSION`, add `(from, fn(&mut Value))` to `migrations()` |
| Something more a session carries over a restart | `goatty-daemon/src/restore.rs`: a field in `SavedSession`/`State` (a new field needs `#[serde(default)]`; a change older builds can't read bumps `STATE_VERSION`, and a restart then refuses to run a build that reads less); unit options in `goatty-client/src/service.rs` |
| A palette command that asks for input  | build a `command_palette::Ask` (fields from `input.rs`) in a `Command::handler` and call `command_palette::ask`; it runs deferred, so the handler may read the workspace. Plugins: `[[command]]` (`goatty-plugin/src/commands.rs`) |
| Host list / switcher (#4)              | `hosts.rs` (entries, connection pool, `open`) + `command_palette` (`Scope::Hosts`/`Scope::Sessions`) + `settings_window/hosts.rs`; per-host data in `goatty-config` `SshSettings`; `~/.ssh/config` parsing in `goatty-client/src/ssh_config.rs` |
| Snippets and expansions (#89)          | format, rendering, sources' parsing, expansion → ERE: `goatty-plugin/src/snippets.rs`; on a host: `goatty-shell/src/snippets.rs`, the widget `goatty-shell/zsh/expand.zsh`, the Basics pack `goatty-shell/plugins/basics/`; GUI: `snippets/`, the palette's input mode (`command_palette/input.rs`: loaders, several values, preview), `settings_window/snippets.rs`. See [`snippets.md`](snippets.md) |
| A plugin feature (new manifest field)  | `goatty-plugin/src/manifest.rs` (+ validation, permissions if it can run code), rendering in `goatty-plugin/src/zsh.rs`, document it in `docs/plugins.md` and the template |
| A plugin operation (GUI/CLI)           | `goatty_plugin::api::Request`/`Reply` (no protocol bump), `HostService::plugin_request`, `settings_window/plugins.rs`, `plugins/cmd.rs` |
| Local echo prediction (#16) | guessing, checking and drawing: `goatty-term/src/predict.rs` (`Predictor`, unit-tested); when to predict and draw (`ssh.local_echo`, RTT or measured echo delay, not in the alt screen or with kitty keyboard flags) and feeding it input/output: `Session::predict`/`ingest` (`goatty-app/src/session.rs`); overlay per frame in `terminal/element.rs`; see `remote-architecture.md` R3 |
| Scrollback on demand, snapshot contents | line numbers, the client's line map and inserting fetched rows: `goatty-term/src/history.rs`; what a snapshot carries: `goatty-term/src/snapshot.rs`; when to fetch: `Session::fill_history` (`goatty-app/src/session.rs`); see `remote-architecture.md` §4a |
| The tailnet (#15)                      | Tailscale CLI, handshake, pairing, availability: `goatty-net` (tested against `tests/fake-tailscale.sh`); a new host-side operation: `goatty_net::api::Request` (no protocol bump) + `Host::request`; client side: `goatty-client/src/tailnet.rs`; GUI: `tailnet.rs`, `settings_window/network.rs`; see [`tailnet.md`](tailnet.md) |
| New wire messages                      | `goatty-proto/src/msg.rs`: a new variant at the *end* (append-only), bump `PROTOCOL_VERSION`, mark it `vN:`; handle in `goatty-daemon/src/client.rs` and `goatty-client/src/connection.rs`, which must not send it to a daemon that agreed on an older version (`Connection::protocol_version`) |

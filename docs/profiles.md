# Profiles and settings (Stage 4)

The profile is the source of truth for everything goatty manages. The settings window edits it; files stay inspectable JSON. Code: `crates/goatty-config` (model, store), `crates/goatty-shell` (`generation`, `service`), `crates/goatty-app` (`settings`, `settings_window`, `ui`).

## Two documents: client and host (#14)

| Document | File (`$XDG_CONFIG_HOME/goatty/`) | Lives on | Contains |
|---|---|---|---|
| `ClientSettings` | `settings.json` | the machine running the window | font (terminal and interface), theme, cursor, padding (one number, or `{top, right, bottom, left}`), background opacity/blur, renderer, keybindings, links, paste, selection (`selection.auto_copy`: copy to the clipboard when you select, with a toast; `selection.clear_after_copy`: unselect afterwards; `selection.primary`: the Linux primary selection), workspace (on launch), tab group colors per host (`tabs.host_colors`: a theme color's name or `#rrggbb`, by the host's name as its tab group shows it), ssh hosts (added hosts, plain SSH per host, list `~/.ssh/config`, open hosts in a new window or tab, local echo prediction `ssh.local_echo`), debug |
| `HostSettings` | `host.json` | each machine running a goattyd | shell program, managed zsh and its features, user dotfiles and the import of what they set up, prompt, history. On a host that follows the shared shell config (#313), everything but the machine-only values is resolved from `shared.json` |

The split follows where a setting takes effect. The window draws text, so fonts and colors are the client's. Shells run on the daemon's host, so their configuration belongs to that host. A remote host keeps its own `host.json`, and the settings window edits it through the protocol. Connecting to a different host shows that host's shell settings.

## The store

`goatty_config::store` reads and writes both documents:

- **Lenient load.** A missing file means defaults. Invalid JSON means defaults plus an issue, and the file is left as it is. An invalid value (`"size": "big"`, an unknown enum variant, a negative `u32`) reverts *that value* to its default and becomes an `Issue { path: "font.size", message }`. Every other value is kept. This uses `serde_path_to_error` in a loop: deserialize, find the failing path, put the default back there, retry.
- **Unknown keys** (for example, written by a newer goatty) are reported and kept. Saving puts them back. Maps with user-chosen keys (`keybindings`) are exempt (`Document::OPEN_MAPS`).
- **Versions and migrations.** Each document has a `"version"`. Older versions run through `Document::migrations()`, a list of `(from_version, fn(&mut Value))`. A newer version is read as well as possible and flagged.
- **Validation.** `Document::validate` clamps ranges (font size 6–48, line height 1–2, padding 0–48, opacity 0.1–1, history 100–10M), reports unknown theme and action names, and drops empty values.
- **Sparse save.** Only values that differ from the defaults are written, so improved defaults reach everyone who never changed that setting. The previous file goes to `backups/<name>-<unix time>.json` (newest 20 kept), and the new one replaces it atomically (temp file + rename). Unchanged content isn't rewritten.
- **Environment overrides.** `GOATTY_*` variables apply on top for one run and are never saved. `ClientSettings::ENV_KEYS` / `HostSettings::ENV_KEYS` say which settings each one replaces, so the settings window can mark those rows.
- **Checks against the machine.** Some values parse but can't work here: `ClientSettings::check_environment` reports a `font.family` or `font.ui_family` that isn't installed (or no installed `font.fallbacks` / custom `font.ui_fallbacks`) and a `links.file_opener` program that isn't on PATH; `ThemeLibrary::check` reports unknown `theme.name`, or `theme.light` *and* `theme.dark` when following the system (the unused one too). On the host, `HostService` adds a `shell.program` that isn't installed there and plugin settings that don't fit their schema. These are issues like any other; nothing is rewritten. `goatty settings validate` makes the same checks without a window: it lists the installed fonts with fontdb (`fonts::installed_families`, each face's first family, as GPUI's Linux text system names them) and skips the font checks when it finds none. Entries of `ssh.hosts` that loading drops (a host ssh can't take, one listed twice) show in a banner over the Hosts page's list, which can save the list without them.

### Where problems show up

- **On the row** (#53): each settings row knows the keys it edits (`settings_window/search.rs`, `Entry::keys`). An issue whose path is that key or inside it (`terminal.padding.top` → Padding) shows as a warning line under the row's description. A row whose setting a `GOATTY_*` variable replaces says "Set by GOATTY_FONT_SIZE for this run" and its control is disabled. Host pages show a banner only for problems no row on that page covers. Host issues travel as text (`path: message`) and are read back with `Issue::parse`; an issue about a value changed in the unapplied draft is hidden.
- **Profile page:** every problem (this computer's and the host's) with the setting it's about, a "Show" button that jumps to the row, and "Copy all", which copies them as text for a bug report. The Diagnostics page links there.
- **Toast** (#85): when goatty starts with problems, or a reload of `settings.json` or `themes/` brings new ones (an edit in an editor), a warning toast keyed `settings.issues` shows the first one, with "Open settings" (opens the settings at that row, else the Profile page) and "Copy" (this computer's report only; Profile's "Copy all" has both). Fixing the last problem replaces it with "The settings on this computer are fine again". New problems in the host's `host.json` (seen when the settings window loads it again) get their own warning toast keyed `settings.host.issues`, with the same buttons, and an all-clear when the last one is gone. Edits made in the settings window are announced too (a live preview isn't: the change is announced when it's saved). A failed save shows an error toast with the reason.

## The same shell on every host (#313)

goatty's point is one shell config on every machine you use. Every host setting resolves as **defaults ← shared ← this host's override**. Model and merge: `goatty_config::shared`; on a host: `goatty_shell::shared`; the GUI: `goatty-app/src/shared_sync.rs` and the settings window's scope picker (`settings_window/scope.rs`).

| Layer | What | Where it lives |
|---|---|---|
| **Shared** | Everything in `host.json` that isn't machine-specific (prompt, aliases, snippets, expansions, history, managed zsh features, `shell.path`, plugin settings) and the registry plugins (id → version, registry, the registry keys it was approved with) | `shared.json` next to `host.json` on every daemon, merged both ways |
| **Override** | A shared setting set to another value on one host, or removed there (`null`) | In the shared document, under that host's id: any machine can see and edit any host's overrides, also while that host is offline |
| **Machine-only** | `shell.program` and `shell.import` (`HostSettings::MACHINE`, `scope = machine`) | That host's own `host.json`, never synced or exported |

**The document.** `shared.json` maps keys to signed entries `{key, value | deleted, hlc, device, sig}`: `set/<path>` (a setting; maps such as aliases, snippets and plugins merge per entry, lists such as `shell.path` are one value), `host/<id>/<path>` (an override), `member/<id>` (whether a host follows), `trust/<device>`, `plugin/<id>`, `name/<id>`. Paths are dotted, with `.` and `~` inside a segment written `~1` and `~0`. Host and device ids are ed25519 public keys: the device key goatty-net already uses for pairing (`device.key` in goatty's data dir, made on first use, also on SSH-only setups). On a GUI computer the local goattyd and the window share it.

**Merge.** For each key the document keeps the newest entry per device; the value is the newest of those (hybrid logical clock, then device id) among the devices this machine trusts. Merging is commutative, associative and idempotent (property tests in `shared.rs`), so order and repetition of exchanges don't matter and there is no conflict UI: concurrent edits of different settings both survive, and of one setting the newer wins. The losing value stays in the document under its device. Resetting a setting leaves a deletion marker.

**Trust.** Every entry is signed by the device that made it, and entries whose signature doesn't verify are dropped. A machine counts entries only from devices it trusts: its roots (`shared-trust.json`, never synced: every client that connected to its goattyd, since anyone who can connect there can already run commands as the user) plus every device a trusted device vouched for with `trust/<device>`. An untrusted device's entries are stored and relayed but never win, so a host that relays the document can at worst withhold entries. A host's own key may set that host's overrides (edits made there without a GUI) and nothing else. A server that edits its own `shared.json` can't change the laptop's shell: the laptop verifies every entry and doesn't trust the server's key. When entries from an unknown device reach this computer (another GUI computer of yours, via a server), goatty asks once: **Trust** (signs `trust/<device>`) or **Ignore**.

**Exchanges.** Daemons never talk to each other. Every client connection to a goattyd with protocol v18 runs an exchange (`goatty_shell::shared::exchange`) between this computer's goattyd, that host and every other host this process is connected to: each peer first trusts the client's device, then everything is pulled into one document (each `Pull` sends the client's version vector, so a peer returns only what it lacks) and each peer gets what it lacks (`Push`). A change made on the laptop reaches the desktop through any server both of them connect to.

**Applying.** A host that follows applies changes itself (`HostService::apply_shared`): it installs or updates the shared plugins its registry vouches for (the same registry name with one of the pinned keys, or goatty's built-in one on both, and a plan that is exactly that plugin: `sync::check_plan`), uninstalls plugins the shared config removed (not ones installed from a folder), reports the rest as needing review, resolves its settings (keeping the entries of plugins the shared config doesn't mention), builds a validated generation and asks idle shells to reload. This works without a GUI (`goattyd sync apply`).

**Membership.** The first connection to a goattyd that never decided asks once: **Use your goatty setup on pi?** with **Follow shared config** / **Keep separate**; when the host has its own changes, the question lists them and offers **Keep pi's differences as overrides**, **Replace with shared** or **Keep separate**. A host whose goattyd goatty just installed joins without a question. The Hosts page has one switch per host. A host that doesn't follow keeps a standalone `host.json`, edited only in its own scope. This computer seeds the shared config from its own `host.json` and plugins the first time (signed as of the file's modification time, so between two computers that both had a setup the newer change to a setting wins).

**Edits.** The settings window's shell pages have a scope picker in the header: **All synced hosts** (edits sign `set/` entries), or a host (rows show the values it uses; a change signs an override; machine-only values go to its own file). It opens on All synced hosts when the active tab's host follows, else on that host. Rows show "Overridden on pi, server-a" with a link, "Only on pi · Use shared value" with an accent bar, or "This host only". Installing or removing a registry plugin under All synced hosts pins or removes it for every host. Edits made on a following host without a GUI (`goattyd settings set`, an older GUI's `SetHostSettings`, a rollback) become that host's overrides signed by the host; the Hosts page offers **Share** or **Keep on pi only** for them.

**Migration.** The old push sync's choices are read once (`goatty_config::shared::ClientState::migrate`, `$XDG_STATE_HOME/goatty/shared-client.json`): hosts set to always sync, or synced before (`shell-sync.json`), join and keep their differences when next met; hosts set to never sync stay separate. `ssh.sync` and `ssh.hosts[].sync` are dropped from `settings.json` (version 4).

**Older daemons.** A goattyd with protocol v17 or older keeps the push sync (#159, `goatty_shell::sync`, `goatty-app/src/shell_sync.rs`): when the setup here differs, a toast offers **Sync**, **Always sync this host** or **Not now** (hosts that follow are synced without asking). It copies this computer's `host.json` (without machine-only values), its registry plugins (same signed registry and version, never downgraded) and goatty's tools there, and uninstalls nothing.

## Applying host settings: generations

Host settings compile into shell configuration. `goatty_shell::generation`:

1. Render the startup files from the settings (`zsh::render`: `settings.zsh`, `starship.toml`, static `.zshenv/.zprofile/.zshrc/integration.zsh`).
2. If the active generation has identical files *and* the same settings snapshot, do nothing.
3. Otherwise write `generations/.build-<pid>/` with `zsh/`, `host.json` (the snapshot, without env overrides) and `info.json`.
4. Validate with `zsh -n` on every startup file. On failure, discard the build: nothing changes.
5. Rename the build to `generations/<n>` and switch `current` to it by renaming a new symlink over the old one (atomic).
6. Keep the newest 10.

Sessions get `ZDOTDIR=~/.local/share/goatty/current/zsh`. Paths written into the config go through `current`, so the files don't depend on the generation number. Builders lock `generations/.lock` (`File::lock`), so goattyd, `goatty --local` and the CLI can't race.

**What each generation changed** (#55, `goatty_shell::history`): `info.json` records the generation that was active when it was built (its *base*; after a rollback that isn't `n-1`), what changed against it and an optional name. The changes are the settings that differ between the two `host.json` snapshots, compared with every default filled in (so `history.size 100000 → 5000` shows whether or not either file spelled the value out; unknown keys don't count), plugins whose files came or went, how many generated files differ, and the goatty version when another one built the base. They're computed at build time, so they survive the base being pruned. Generations from before #55 get theirs computed on the fly against `n-1` while it's kept.

The one-line summary stays short on purpose: one or two short changes are listed (`history.size 100000 → 5000`, `starship off`), anything more is counted with where it happened (`5 changes: history, aliases, eza`). `From 3: …` marks a generation built after rolling back to 3. Nothing changed but files: `2 generated files changed`. The full list and the generated-file diff against the base are one click away.

**Names** (`generations/<n>/info.json` `name`, at most 60 characters) make rollback targets recognizable ("before trying fish"). Naming rewrites `info.json` atomically under the build lock and touches nothing else; a rolled-back-to generation keeps its name.

**Rollback** switches `current` to an older generation *and* restores its `host.json` snapshot as the settings file (backing up the current one). Without restoring the settings, the next session would rebuild from the newer settings and undo the rollback.

**Apply order** (`HostService::set`): parse leniently → compute the diff against the active generation → build + validate + activate → save `host.json`. A configuration that doesn't validate is rejected before the file is touched.

## Protocol (v4, shared config v18)

| Request | Reply | Notes |
|---|---|---|
| `GetHostSettings` | `HostSettings { json, issues, env_overrides, generation }` | `json` is the stored document |
| `SetHostSettings { json, apply }` | `HostSettingsApplied { issues, diff, applied, generation }` | `apply: false` = validate + preview only |
| `ListGenerations` | `Generations { list }` | |
| `Rollback { generation }` | `HostSettings` | `None` = the previous one |
| `SetPalette { session, colors }` | – | 21 `0xRRGGBB`: ANSI 16, fg, bg, cursor, selection fg/bg |
| `ReloadShells` | `ShellsReloaded { count }` | v4: ask the running managed shells to restart with the active generation |
| `Shell` `{"op": "generations"}` | `{"kind": "generations", "list": [Entry]}` | v9 JSON (`goatty_shell::setup`): number, age, active, name, changes. The GUI falls back to `ListGenerations` for an older goattyd and then shows numbers and ages only |
| `Shell` `{"op": "name_generation", number, name}` | `{"kind": "done"}` | `name: null` or blank removes it |
| `Shell` `{"op": "generation_diff", number}` | `{"kind": "diff", "files": [FileDiff]}` | unified diffs against the generation's base |
| `Shared` `{"op": "status"}` | `{"kind": "status", …}` | v18 (#313, `goatty_shell::shared::api`): host id and name, version vector, member, roots and trusted devices, untrusted devices, local changes, plugins needing review, last apply |
| `Shared` `{"op": "pull", "have"}` | `{"kind": "entries", "entries", "have"}` | the entries newer than the version vector `have` |
| `Shared` `{"op": "push", "entries"}` | `{"kind": "status", …}` | merged; when that changed something and the host follows, applied before the reply |
| `Shared` `{"op": "trust", "device", "name"}` | `{"kind": "done"}` | the connecting client's device becomes a trust root there |
| `Shared` `{"op": "apply"}` | `{"kind": "status", …}` | resolve and apply now |

Settings travel as JSON documents rather than typed structs, so a schema change doesn't need a protocol bump. The daemon's parser handles versions and unknown keys.

`SetPalette` fixes #36: the daemon's terminal is authoritative and answers OSC 4/10/11 color queries, so it needs the colors the user actually sees. Clients send their theme after attaching and on every theme change, and resend it after a reconnect. With several clients on one session, the last one wins.

## The app

- `settings::AppSettings` (GPUI global) holds the loaded file, the effective value (file + env, with the font families resolved against the installed fonts), the installed font list and problems. `AppSettings::update` edits, validates, saves and replaces the global. Every view observes the global, so terminal and interface font, size, line height, padding, renderer, cursor shape/blink and theme apply live. The file is polled every 500 ms, so edits in an editor apply live too.
- Keybindings: `goatty_config::keys` holds the action table and defaults. `actions::apply_keybindings` rebuilds every binding with `KeyBinding::load`, so an invalid keystroke is reported instead of panicking.
- Keystrokes are stored canonically (`keys::canonical_keystroke`): lowercase, modifiers ordered `ctrl-alt-shift-super-fn`, aliases like `plus` resolved, and no `shift` in front of a symbol or digit. GPUI on Linux names a symbol key by the character it types and never reports Shift with it (Ctrl+Shift+= is `ctrl-+`, Ctrl+Shift with the German + key is `ctrl-*`), so `ctrl-shift-+` could never fire. Loading rewrites old spellings and drops overrides that repeat a default.
- Theme: `ThemeSettings { name, bold_is_bright, overrides }` resolves to `ThemeColors`. The terminal palette is swappable at runtime (`Terminal::set_palette`), and goatty's own UI colors derive from it (`ui::style::Ui`).
- Ctrl with + / - (Shift optional; on German layouts Ctrl+Shift++ types `*`, also bound) zooms on top of the configured size until Ctrl+0 resets it. Changing the size in settings keeps the zoom offset.

### The settings window

`Ctrl+,` (or `goatty --settings[=page]`) opens it. The sidebar groups this computer's pages, then the shell pages of the active tab's host, then Profile and About; below 920 px it becomes a navigation rail. **Search settings** at the top finds any setting (by label or what it does), page or shortcut, and jumps to it. Pages:

| Page | Edits | Applies |
|---|---|---|
| Appearance | live preview; theme gallery (one theme, or light + dark following the desktop), import, color editor (folded); font and interface font (searchable pickers, monospace/proportional or all; monospace is measured from the font files in the background), size, line height, ligatures, bold-is-bright; cursor shape (visual) and blink; padding (all sides or each side), background opacity (+ blur) | live, saved immediately |
| Terminal | how links open, remote file link mode (`links.remote_files`), remote editor command (`links.remote_file_opener`), max remote download size (`links.remote_max_size_mb`), file opener command, copy when you select (`selection.auto_copy`), unselect after copying (`selection.clear_after_copy`), middle click pastes what you select (`selection.primary`, Linux), what a right click does (`mouse.right_click`: goatty's menu / always the menu / copy or paste / nothing; Shift+right-click does the other thing), lines per mouse-wheel notch (`mouse.scroll_lines`, 1–20, default 3), what a middle click pastes, when to ask before pasting | live |
| Workspace | what a new window shows, tab style (visual), tab bar, program icons, fading inactive panes, asking before closing a running program; notifications (in goatty and on the desktop, each never / when not looking / when unfocused / always; which events) | live / next launch |
| Shortcuts | a table of every action: grouped or sorted by name or keys, search by name or keys (typed, or pressed with the keyboard button), filters for changed and unbound; per action: keycaps (× removes), + records a keystroke, reset; warnings when a recorded key took over another action's key, when a key you added is one the shell uses, and when a keystroke doesn't parse | live |
| Hosts | open hosts in a new window or tab (`ssh.open_in`), show typing before the host echoes it (`ssh.local_echo`: auto/always/never), the shared shell config (every known host with its status, last sync, waiting changes, plugins needing review, a switch to follow the shared config; Share / Keep for changes made on a host; devices waiting to be trusted; Sync now), add hosts (`user@host` or an ssh alias, optional name), plain SSH per host, list the hosts in `~/.ssh/config`, connect | saved immediately; the shared config through the hosts |
| Network | the active tab's host on the tailnet: Tailscale's state, make it available (port), allow a new device (confirm codes), paired devices; this computer: list tailnet computers (`tailnet.discover`, `tailnet.all_devices`), forget paired hosts | host side: right away through goattyd (`Net`); client switches: saved immediately |
| Shell | managed zsh, program, shell integration (the built-in tools are on Plugins), links to Dotfiles and PATH | staged → preview → apply |
| Dotfiles | importing what your shell sets up (on/off, zsh or bash; the PATH folders and variables the last check found, each switchable, "Check again"), a link to copying your aliases, running `~/.zshenv`/`~/.zprofile` and (guarded) `~/.zshrc` | staged |
| Prompt | live preview; Starship, rich/minimal style, own starship.toml (off by default), two-line, blank line, duration threshold | staged |
| History | size, duplicates, leading space, sharing, fzf history search, fzf-tab completion and its previews | staged |
| Aliases | yours first, then each plugin's (folded); filters All / Mine / From plugins; add/edit in a dialog (group completes existing groups) | staged |
| Snippets | yours, then each plugin's (folded, "make my own copy"); filters; the editor (`{name}` in the line adds an input row; preview with blanks; safety); expansions with the `shell.expansions` switch, "Try it", add/edit/switch off ([`snippets.md`](snippets.md)) | staged |
| Plugins | installed and built-in plugins, each with its own page (settings by the manifest's groups, advanced folded; tools; permissions; aliases); browse the registries; install review with progress and Cancel; this computer: tell about plugin updates (`plugins.notify_updates`) | staged / installs right away; client switch: saved immediately |
| Profile | problems (show, copy all), file locations (copy, open folder), env overrides, export/import, generations (name, one-line summary; click for every change, the file diff and naming) with rollback, reset | – |
| About | version, what the window is connected to, links, a summary to copy into bug reports | – |

Host edits collect in a draft. A floating bar at the bottom offers **Discard**, **Preview** (the host computes the diff of the generated files, shown colored) and **Apply** (a new generation, validated first); the sidebar marks the pages with unapplied changes. When an apply or a rollback changed the shell configuration, the bar offers **Reload running shells** (or **Not now**); see below. Short results (a theme saved or imported, a profile exported, shells reloaded) are toasts in the settings window's own corner, above the bar; the bar keeps what needs room or a decision. Calls to the daemon run off the UI thread. With `--local` there is no daemon, and the window uses `goatty_shell::HostService` in-process: same code path, same files.

**Export** writes `{ "goatty_profile": 1, "client": {...}, "host": {...} }` with non-default values only, minus machine-specific ones (`host.shell.program`, `client.debug`). **Import** applies the client part right away and stages the host part as a draft to preview and apply.

### Reloading running shells (#56)

New sessions start from the active generation. Shells that are already running keep the configuration they started with until they reload. After an apply or a rollback, the settings window offers **Reload running shells**. `goattyd shell reload` does the same from the command line, and `goatty-reload` reloads only the current shell.

- The daemon (or `HostService` in-process with `--local`) remembers the pid of every session it started with goatty's managed zsh (`HostService::track_shell`), and forgets it when the session exits. Plain zsh, other shells and `GOATTY_SHELL_MANAGED=0` sessions aren't tracked.
- `ReloadShells` sends each one `SIGURG`. The signal is ignored by default, so a process that isn't goatty's zsh (after `exec bash`, or a reused pid) is unaffected.
- The managed `.zshrc` traps it (`TRAPURG`). At an idle, empty prompt the shell reloads right away: the trap runs `zle .send-break`, which returns to the top level at once. From a trap, `accept-line` would only take effect on the next keypress. If the signal lands while the prompt is still being drawn (after precmd, before zle reads input), `zle` isn't active yet; a `zle-line-init` hook sees the pending request and does the same as soon as the line editor starts. The precmd hook then runs `goatty-reload` (`exec`, so the pid and the session stay). While a command runs, or with something typed on the line, the reload waits for the next prompt.
- A shell with jobs doesn't reload, since `exec` would lose the jobs. It prints a reminder to run `goatty-reload` instead.
- Reloading loses shell state that isn't in the startup files (variables, functions, `dirstack`). The working directory is kept.

## Themes

`ThemeSettings`: `name` (the theme when not following the system), `follow_system`, `light` / `dark` (used by the desktop's appearance when following), `bold_is_bright`, and `overrides` (per-theme color overrides keyed by theme id, applied only when that theme is active).

**Color values** (#71) in `settings.json`, theme files and `goatty settings set theme.overrides.<theme>.* VALUE` are read with `Color::parse`: hex (`#rrggbb`, `rrggbb`, `#rgb`, an alpha is ignored) or a CSS function (`rgb()`, `hsl()`, `hsv()`/`hsb()`, `oklch()`, `oklab()`; `goatty_config::colorspace::parse_css_color`). Bare numbers aren't accepted there, since they could mean any model. Colors are always written back as `#rrggbb`. A value that loses something (an alpha below 1, a channel out of range, an `oklch()` outside sRGB, which is gamut-mapped by lowering chroma) loads without an issue, but `goatty settings validate` lists it with the `#rrggbb` it reads as, and `settings set` prints a note.

**User themes** are files in `~/.config/goatty/themes/<id>.json`: `{ "name": …, "colors": { "foreground", "background", "cursor", "selection_foreground", "selection_background", "ansi": [16] } }`. `goatty_config::ThemeLibrary` lists built-ins plus user themes. A user file with a built-in's id replaces that built-in. An unknown theme name falls back to the default and is reported as a problem. Broken theme files are skipped and reported too. The app polls the folder, so dropping a file in makes it appear in the gallery.

**Importing** (`goatty_config::theme_import`, format detected from the content):

| Format | Notes |
|---|---|
| base16, YAML or JSON (classic `base00…base0F`, or tinted-theming's nested `palette:`) | base16-shell's terminal mapping (bg = base00, fg = base05, selection = base02, ANSI from base08–base0E, bright black = base03, bright white = base07) |
| iTerm2 `.itermcolors` | `Ansi 0–15`, foreground, background, cursor, selection, selected text; components 0–1. `Color Space` `P3` (recent iTerm2) is converted from Display P3 to sRGB (out-of-gamut colors lose chroma, keeping lightness and hue); `sRGB`, `Calibrated`, `Device` and none are read as sRGB (#63) |
| Windows Terminal | one scheme object, a list, or a whole `settings.json` (`schemes`); `#RRGGBBAA` ignores alpha, CSS colors are read too |
| goatty theme file | as above |

Imports get a fresh id when the name is taken (`name`, `name-2`, …).

**Editor** (Appearance page): the gallery shows every theme with `mine`, `light` and `dark` badges, and "Create custom theme" above the gallery creates a new user theme from the current colors and opens the color editor. With "Follow the system", a Light/Dark selector decides which side a click assigns. Below the gallery are the 22 color swatches (● = overridden for the active theme). Clicking one opens it in the color picker (below), and "Revert …" drops that override. "Reset all colors" clears the active theme's overrides (other themes keep theirs). "Save as theme" writes the colors on screen to a new user theme and switches to it. For an active user theme there are also "Save into …" (overwrite) and "Delete". Deleting a theme the settings point at falls back to the defaults.

**Color picker** (`ui/color_picker.rs`, used by the editor; any other color setting can embed it): a hue ring with a saturation/value square, or (switch below it, #68) an OKLCH disc: hue around, chroma outwards, at the color's OKLCH lightness, which the bar under the disc sets. On the disc evenly bright colors sit on one circle; colors sRGB can't show are drawn faded at the most chroma it can, and dragging past the gamut's edge stays on it. The disc is a bitmap generated per lightness (`disc_pixels`, cached, the replaced one dropped from the atlas). Sliders per model (RGB, HSL, HSV, OKLCH; the OKLCH chroma track is gamut-limited) each with a number field, "Previous → New" (click Previous to go back), **Darker**/**Lighter** (a 4 % step of OKLCH lightness, so steps look even; hue and chroma are kept, and chroma lost at the gamut's edge comes back on the way back), and a field that takes whatever you paste: `#rgb`, `#rrggbb` (`#rrggbbaa`, alpha ignored), `0x…`, `rgb()`, `hsl()`, `hsv()`/`hsb()`, `oklch()`, `oklab()` in CSS syntax (commas or spaces, `%`/`deg`/`turn`, `/ alpha`), or three bare numbers read in the selected model. Ctrl+V pastes a color from the clipboard anywhere in the picker, Ctrl+C copies the hex. Out-of-gamut OKLCH/OKLab values are mapped into sRGB by lowering chroma. The picker keeps float state, so dragging doesn't round to 8 bits and hue/saturation/chroma survive passing through gray, black and white. Changes preview live (`AppSettings::preview`, no file write); the file is written when the mouse is released, on Enter, or 600 ms after the last typed change. Colors you settle on (release, Enter, paste, history click, Previous) go into the **history** (24 colors, newest first), kept in `$XDG_STATE_HOME/goatty/color-history.json`: state, not a setting, so it stays out of `settings.json` and exported profiles. Color math and parsing: `goatty_config::colorspace`; history: `goatty_config::color_history`. Colors are opaque sRGB, like the themes.

Keyboard (#67): the ring, the square, the disc, the lightness bar and each slider are tab stops with a focus ring. Arrows move them (Shift: 10 steps): the ring's hue by 1°, the square's saturation (left/right) and brightness (up/down) by 1 %, the disc's hue (left/right) by 1° and chroma (up/down) by 0.002, sliders by one unit of their channel; sliders also take PageUp/PageDown (10 steps) and Home/End. Keys and nudges preview live and join the history once they stop for 700 ms.

Contrast (#70): under "Previous → New" the picker shows the WCAG 2 contrast ratio against the colors the slot is drawn with (`ColorSlot::contrast_checks`: text, selected text and cursor against their backdrop and the other way round, ANSI colors against the background), with an "Aa" sample. Text and its backdrop warn below 4.5:1; the chromatic ANSI colors and the cursor below 3:1 (many good themes keep their ANSI colors between 3 and 4.5); black, white, the bright pair and the accent only show the ratio.

**Following the desktop** (#51): GPUI reports the window appearance (on Linux from the XDG portal's `org.freedesktop.appearance color-scheme`, live). `Workspace` observes it and calls `AppSettings::set_system_dark`, which re-resolves the theme. The terminal palette, goatty's UI colors and the palette sent to the daemon (`SetPalette`) all follow. At startup the portal answer arrives a moment after the window opens, so goatty switches once right away, through the same path a later desktop toggle takes.

## Command line (#52)

Everything works without a window: scripts, SSH sessions, dotfile setups.

```
goatty settings path                     # settings.json, host.json, themes/
goatty settings show [KEY] [--all]       # stored values, every value, or one dotted key
goatty settings validate [FILE]          # exit 1 on problems (values, themes, keystrokes)
goatty settings set KEY VALUE            # e.g. font.size 15, theme.follow_system true
goatty settings unset KEY
goatty settings export FILE|- [--ssh HOST [--remote-daemon PATH] | --via CMD]         # profile: these settings + the host's shell settings
goatty settings import FILE|- [--yes] [--ssh HOST [--remote-daemon PATH] | --via CMD]  # host part goes to that host (default: local goattyd / host.json)

goatty theme list | import FILE… [--name N] | export ID [FILE] | set ID [--for light|dark] | delete ID

goattyd settings path | show [KEY] [--all] | validate [FILE] | set KEY VALUE [--shared|--host] | unset KEY [--shared|--host]
goattyd shell generations | name N [NAME…] | diff [N] | rollback [N]
goattyd sync status [--json] | apply      # the shared shell config on this host (#313)

goatty sync [HOST…] [--join [--keep] | --separate] [--trust DEVICE]   # exchange it with SSH hosts, record choices
```

On a host that follows the shared config, `goattyd settings set` makes the change that host's override (`--host`, the default); `--shared` changes the shared config, signed by this host, which counts wherever this host's device is trusted (a GUI computer's own goattyd).

`shell generations` prints one line per generation: `* 8  2h ago  "before trying fish"  history.size 100000 → 5000`. `shell name N` without a name removes it. `shell diff N` prints the generated-file changes of generation N (without N: what the current settings would change).

`set` takes a dotted key and reads the value as JSON when it parses (`5000`, `true`, `null`, `"#ff00ff"`), otherwise as a string (`minimal`, `/bin/zsh`). Unknown keys and invalid values are errors, and nothing is written (`store::set_key`). Entries of open maps (`keybindings.ctrl-k`) are allowed. `goattyd settings set` applies like the settings window's Apply: a validated new generation, then the save. Client changes made with `goatty settings set` reach a running app within half a second, through the file polling. `import` asks for confirmation, and without a terminal it needs `--yes`.

# goatty: vision and decisions

Why goatty exists and the decisions that shape it. Status and open work are in GitHub milestones and issues, not here. For the code layout and where new work goes see [`architecture.md`](architecture.md).

## Vision

goatty is a complete terminal environment, not just another terminal emulator: a polished, configurable desktop app instead of a blank prompt the user must assemble. A new user opens it and has:

- a fast, attractive terminal with sensible defaults
- a ready-to-use zsh with autosuggestions, syntax highlighting, completion and fuzzy history
- a configurable prompt and theme
- tabs, splits and workspaces that survive closing the window
- a GUI for every setting and for installing tools and plugins

Experienced users still get a normal shell. goatty has no proprietary command language and never locks users out of their own tools and dotfiles.

## Principles

1. **Works out of the box.** Useful without reading docs or editing dotfiles.
2. **GUI first, not GUI only.** Every setting is in the GUI, and the underlying files stay inspectable and hand-editable.
3. **Use existing tools.** Integrate mature shells and utilities (zsh, Starship, fzf) rather than reimplementing them.
4. **Keep the host safe.** No silent change of the login shell, the user's dotfiles or the global PATH, and no privileged services. The one opt-in exception is a guarded line in `.zshenv` (see `managed-zsh.md`).
5. **Reproducible and reversible.** Host configuration is generated into versioned generations that can be diffed and rolled back.
6. **Open ecosystem.** Third parties publish integrations and settings through declarative plugins, with no change to goatty's source.
7. **Separate the components.** GUI, terminal engine, session manager, shell environment and plugins talk through explicit interfaces.

## Decisions

| Area | Decision | Why |
|---|---|---|
| Language, UI | Rust, **GPUI** (pinned git rev) | Native, GPU-rendered, custom elements for the terminal grid. Pre-1.0, so bump the rev on purpose. |
| VT core | **rio-vt**, used only inside `goatty-term`, pinned to an exact version | Real terminal state machine without Rio's window or renderer. `alacritty_terminal` is the fallback if it ever blocks us. |
| Renderer | goatty's own per-cell glyph cache in a custom GPUI element | Per-line text shaping was too slow at high refresh rates. See `stage0-findings.md`. |
| PTY | `portable-pty` | |
| Shell | **zsh**, managed through a generated `ZDOTDIR` | Familiar, mature completion. Other shells are launchable but have no first-class integration yet. |
| Prompt, history, fuzzy | Starship, fzf (history on `Ctrl+R`, files, directories, fzf-tab completion) | Atuin is an optional registry plugin (#165). |
| Settings | Versioned JSON: client settings and host settings are separate documents | See `profiles.md`. |
| Plugins | Declarative `plugin.toml` manifests, derived permissions, a signed Git-backed registry | No native code loaded into the app. A WASM runtime is only worth it if real application-plugin use cases appear. See `plugins.md`. |
| Process ownership | `goattyd` owns PTYs, sessions and workspaces. The GUI is always a client, of a local or a remote goattyd. | See `remote-architecture.md`. |
| Remote | SSH or stdio transports, or the tailnet. goattyd never binds a network socket. | See `remote-architecture.md`, `tailnet.md`. |
| Concurrency | Threads, not async | tokio adds nothing at this scale. |

## Session persistence

A daemon-owned PTY keeps a shell alive when the window closes and across goattyd restarts and upgrades. It can't survive a machine reboot: after a reboot goatty restores the workspace layout and starts fresh shells, and it doesn't promise to resume processes. Layout and metadata are stored apart from live processes, and a screen snapshot is never a substitute for a live PTY.

## Shell integration

The managed zsh emits standard OSC 133 (prompt and command marks) and OSC 7 (working directory) sequences, which goatty uses for command boundaries, the output viewer, prompt jumps and directory-aware titles. Anything goatty-only is gated on `_goatty_inside`. Details: `managed-zsh.md`.

## Keyboard behavior

`Ctrl+C` copies when text is selected and otherwise sends the interrupt. `Ctrl+Shift+C/V` always copy and paste. In full-screen applications goatty must not steal keys. All shortcuts are configurable actions.

## Roadmap

Track status in GitHub milestones (`Stage 1`, `Stage 3`–`5`, `R1`–`R3`, `later`). In order:

- **Stage 0–1:** the GPUI + rio-vt terminal. Done.
- **Stage 2:** daemon, reattach, restarts, workspaces. Done.
- **Stage 3:** managed zsh, tools, diagnostics. Done.
- **Stage 4:** profiles and the settings GUI. Done.
- **Stage 5:** plugins and the registry. Done.
- **Remote track:** SSH, tailnet, local echo. Done apart from what's open in the R1 milestone.
- **Later:** Bash/Fish/PowerShell adapters (Stage 6), macOS and Windows releases, a web client, private registries, a constrained extension runtime.

## Non-goals

- A new shell or shell language, or a replacement for zsh, Bash or Fish.
- A custom prompt or history engine while Starship and fzf are enough.
- A package manager for every OS, or a mandatory Nix.
- Arbitrary native plugins.
- A terminal server exposed to the public internet.
- Full feature parity across every shell and OS.

---
paths:
  - "crates/goatty-shell/**"
---
# goatty-shell (managed zsh)

Design and load order: `docs/managed-zsh.md`.

- In the startup files (`zsh/`):
  - Source user files at top level, never inside a function, with the user's own `ZDOTDIR`.
  - Syntax highlighting loads last (`zsh.load = "last"`).
  - Write `typeset name=(…)` without `builtin`.
  - Only change these files with `tests/zsh_startup.rs` passing.
- Host config changes are generations: build, check with `zsh -n`, then switch `current` atomically. A rollback also restores the generation's `host.json`. Never write into `current/` directly.
- The built-in tools are plugins (`plugins/<id>/plugin.toml`, listed in `builtins.rs`). A new pin must be a release at least 2 weeks old, with a sha256 taken from the real download and checked against upstream's checksum file. Keep their package ids: installs from before #98 live in `tools/<id>/`.
- Tests that run zsh use `Dirs::under` a temp dir, never the real `$HOME` or XDG dirs. Tests of goatty's shell in every terminal (`everywhere`, the `.zshenv` hook) write a sandboxed `HOME`'s `.zshenv`, never the developer's, and keep the refresh stamp in the future (`Sandbox::hook`) so hooked shells don't run a goattyd. `shell.import` is on by default, but only `Launcher::from_env` imports anything: a test that wants the import gives its `Launcher` a sandboxed `import::Context` (`with_import_context`), never the developer's real login shell.
- The import never runs the user's startup files in goatty's shell, only in a harvest process. It applies PATH folders the startup *added* (never goattyd's own PATH) and variables; aliases are only offered for a one-time copy into goatty's `aliases`, never applied live.
- Outside goatty's own panes (`_goatty_inside` is 0: an IDE, another terminal, tmux), the startup files send only standard escape codes and assume no Nerd Font. Anything goatty-only (OSC 7337, its key sequences) is gated on `_goatty_inside`.
- `entry.zsh` runs inside the user's `.zshenv` for every interactive zsh on a hooked host: keep it to builtins and `-nt` tests (no processes) on its normal path.

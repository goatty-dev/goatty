# Managed zsh (Stage 3)

goatty sessions run zsh with a configuration goatty generates: autosuggestions, syntax highlighting, completion in fzf (fzf-tab), Starship, fzf's history and file search, and shell integration (OSC 133/7). Nothing in the user's dotfiles changes, unless they turn on [goatty's shell in every terminal](#goattys-shell-in-every-terminal-283), which adds one line to `.zshenv`. Code: `crates/goatty-shell`. Startup files: `crates/goatty-shell/zsh/`.

The tools (fzf, fzf-tab, autosuggestions, syntax highlighting, Starship) are **built-in plugins** since #98: ordinary plugin manifests in `crates/goatty-shell/plugins/<id>/`, compiled into goatty. They get the same install plans, settings UI and load-order slots as registry plugins ([`plugins.md`](plugins.md#built-in-plugins)).

## How a session starts

goattyd (or `goatty --local`) asks `goatty_shell::Launcher` for the shell, using the host settings (`~/.config/goatty/host.json`, re-read for every session). If it's zsh and `shell.managed` is on (the default), the launcher makes sure the active *generation* matches those settings: if not, it builds, validates and activates a new one (see [`profiles.md`](profiles.md)). It then starts `zsh -l` with:

| Variable              | Value                                              |
|-----------------------|----------------------------------------------------|
| `ZDOTDIR`             | `~/.local/share/goatty/current/zsh`                |
| `GOATTY_ORIG_ZDOTDIR` | the user's `ZDOTDIR`, only if goattyd's environment had one |

zsh then reads the startup files from goatty's directory:

```
.zshenv    source settings.zsh (generated: paths + feature toggles)
           restore the user's ZDOTDIR, source the user's .zshenv
           interactive? → ZDOTDIR = goatty's dir again   (else: done, plain zsh)
.zprofile  (login) source the user's .zprofile with their ZDOTDIR, re-pin goatty's
.zshrc     [import.zsh: variables and PATH your own shell sets up, if shell.import]
           options, history, PATH, plugins-early.zsh (plugin fpath + env), keys, compinit
           [user's .zshrc, guarded, if shell.zsh.user_rc]
           plugins-line-editor.zsh (fzf → fzf-tab → autosuggestions)
           plugins-prompt.zsh (starship), or goatty's fallback prompt when no prompt plugin loaded
           plugins.zsh (plugin aliases + code) → aliases.zsh (yours) → integration
           ZDOTDIR = the user's again
           ~/.config/goatty/zshrc (the user's additions)
           plugins-last.zsh (syntax highlighting: it wraps every widget defined before it)
           goatty-status / goatty-setup / goatty-reload, reload-on-request trap, missing-tools hint
.zlogin    the user's own, because ZDOTDIR is already theirs again
           (goatty's .zlogin only runs when something pointed ZDOTDIR back at goatty's directory: it hands it back and runs theirs)
```

Rules that came out of testing against real zsh (`crates/goatty-shell/tests/zsh_startup.rs`):

- The user's files run at top level, never inside a function. Otherwise `typeset` in them would create locals.
- The user's files see their own `ZDOTDIR` (unset → `$HOME`). If their `.zshenv` sets `ZDOTDIR` (the common `~/.config/zsh` pattern), the later files come from there.
- A user `ZDOTDIR` that resolves to goatty's own directory is never sourced as "the user's". Without this guard, `.zprofile` sourced itself in a loop.
- `typeset name=(…)` must not be prefixed with `builtin`, because that disables typeset's array-assignment syntax.
- If `settings.zsh` fails to load, `.zshenv` restores the user's `ZDOTDIR` and returns: the session degrades to plain zsh instead of breaking.
- Non-interactive zsh started during startup (e.g. `zsh -c` from a user's `.zprofile`) gets the user's `ZDOTDIR` straight away and behaves like plain zsh.
- After startup, `ZDOTDIR` is exactly what the user had (set or unset), so nested shells, `exec zsh` and `.zlogout` behave like plain zsh (with [goatty's shell in every terminal](#goattys-shell-in-every-terminal-283) on, they start goatty's shell again through the line in `.zshenv`). If something points `ZDOTDIR` at goatty's directory once `.zshrc` is done (VS Code's shell integration does), the first prompt hands it back. `goatty-reload` restarts the current shell with goatty's config. `SIGURG` (sent by `goattyd shell reload` or the settings window after an apply) makes the shell reload at its next idle prompt; see `docs/profiles.md`.
- Debian/Ubuntu's `/etc/zsh/zshrc` runs `compinit` unless `skip_global_compinit` is set; `.zshenv` sets it so compinit runs once, with goatty's dump file in `~/.cache/goatty/`.
- fzf's key bindings are only evaluated when zle has a terminal (`-o zle && -t 0`). Without one, fzf's option save/restore fails on `zle`.

## Files and directories

```
~/.config/goatty/host.json      the host settings (edit in the settings window)
~/.local/share/goatty/generations/<n>/   one built configuration each (newest 10 kept):
  zsh/settings.zsh              paths, feature list, history options, what goatty-setup installs
  zsh/plugins-*.zsh             the plugins, one file per load slot (see plugins.md)
  zsh/.zshenv .zprofile .zshrc .zlogin  static, from crates/goatty-shell/zsh/
  zsh/entry.zsh                 what the line in .zshenv sources (goatty's shell in every terminal)
  zsh/integration.zsh           OSC 133 + OSC 7, goatty's command-line widgets (OSC 7337)
  zsh/rc-guard.zsh              what a guarded .zshrc sources goes through it
  zsh/expand.zsh                the expansion widget (Space, Enter, Backspace), static
  zsh/expansions.zsh            the expansion rules as data: yours, then the plugins' (empty with shell.expansions off)
  zsh/starship.toml             generated from the prompt settings; used unless you prefer your own starship.toml
  zsh/starship-plain.toml       the same prompt without Nerd Font glyphs, for other terminals (prompt.icons_elsewhere off)
  host.json, info.json          the settings it was built from; when, by which goatty, on which generation, what changed, its name
~/.local/share/goatty/current → generations/<n>   the active one
~/.local/share/goatty/tools/<package>/<version>/   managed installs (built-in and registry plugins' packages)
~/.local/share/goatty/bin/<exe>                 → active managed binary (appended to the shell's PATH)
~/.local/share/goatty/plugins/<package>         → active managed zsh plugin (a package with a `source` script)
~/.local/share/goatty/share/zsh/site-functions/ completion functions: `_goatty`, `_goattyd`, managed tools' (appended to fpath)
~/.config/goatty/zshrc          the user's additions (goatty never writes it)
~/.cache/goatty/                zcompdump, completion cache, init/ (cached tool init scripts), downloads
~/.local/state/goatty/shell-hint-dismissed      set by `goatty-setup --dismiss` or the GUI's "Don't ask again"
~/.local/state/goatty/shell-setup-offered       set once goatty's window offered the install (the shell hint stays quiet)
~/.local/state/goatty/import/   what your own shell sets up: harvest.json (the last harvest and failure), import.zsh
~/.local/state/goatty/shell-refreshed   when the configuration and the import were last brought up to date (hooked shells compare against it)
~/.local/state/goatty/zshenv-backups/   your .zshenv before goatty added or removed its line (newest 10)
```

XDG variables are respected. History stays in the file plain zsh would use (`${ZDOTDIR:-$HOME}/.zsh_history`), so existing history keeps working.

## Features and settings

`goatty_config::HostSettings`, edited on the settings window's Shell, Prompt and History pages (or in `host.json`):

| Setting | Default | Env |
|---|---|---|
| `shell.program` | zsh if installed | `GOATTY_SHELL` |
| `shell.managed` | on | `GOATTY_SHELL_MANAGED=0/1` |
| `shell.zsh.user_env` (`.zshenv`, `.zprofile`) | on | |
| `shell.zsh.user_rc` (`.zshrc`, guarded) | off | `GOATTY_ZSH_USER_RC=0/1` |
| `shell.import.enabled`: apply the PATH folders and variables your own shell sets up ([below](#importing-your-shell-setup)) | on | |
| `shell.import.from` (`auto`/`zsh`/`bash`), `shell.import.skip_vars` | auto, none | |
| `shell.zsh.integration` | on | |
| `shell.zsh.highlight_paste`: show a paste in reverse video until the next key (plain zsh's `zle_highlight` default); off sets `paste:none` | off | |
| `shell.path`: folders added to the end of PATH (before goatty's `bin/`), `~/…` = home, missing ones skipped; edited on Settings → PATH and with the palette's "Shell: Add a folder to PATH…" | none | |
| `prompt.style` (`rich`/`lean`/`classic`/`rainbow`), `prompt.symbols` (`nerd`/`emoji`/`text`/`none`), `prompt.show` (`all`/`essential`) | rich, nerd, all | |
| `prompt.two_line`, `prompt.newline_before`, `prompt.transient`, `prompt.duration_threshold_ms` (0 = never) | on, off, off, 2000 | |
| `prompt.icons_elsewhere`: keep Nerd Font icons (and Powerline shapes) in other terminals; off, they get `starship-plain.toml` | off | |
| `prompt.separator`, `prompt.heads`, `prompt.tails` (Classic/Rainbow), `prompt.frame`, `prompt.connection` (two lines) | angled, sharp, flat, none, none | |
| `prompt.directory.*`, `prompt.character.*`, `prompt.colors.*`, `prompt.modules.<module>.{enabled, side, symbol, color}` | see below | |
| `history.size` (100 000), `history.ignore_duplicates`, `history.ignore_space`, `history.share_between_sessions` | 100 000, on, on, off | |
| `plugins.<id>.enabled` for `fzf`, `fzf-tab`, `autosuggestions`, `syntax-highlighting`, `starship` | on | |
| `plugins.fzf.settings.{hidden_files, previews}` | on, on | |
| `plugins.fzf-tab.settings.previews` | on | |
| `plugins.autosuggestions.settings.{strategy, max_length}` | `history-completion`, 80 | |
| `plugins.syntax-highlighting.settings.brackets` | off | |
| `plugins.starship.settings.prefer_user_config` | off | |

Each built-in plugin has its own page under Plugins with all its settings; Prompt shows the Starship switches, History the fzf and fzf-tab ones. `host.json` version 1 had the switches as `shell.zsh.{autosuggestions, syntax_highlighting, fzf}`, `prompt.starship`, `history.atuin` and the options as `prompt.prefer_user_config`, `history.atuin_up_arrow`: loading one moves them to `plugins.*` (version 2; only keys the file had, so it stays sparse). Version 3 (#165) drops `plugins.atuin`: Atuin left the built-in plugins, and its entry would only be reported as a plugin that isn't installed.

`.zshrc` is off by default because most `.zshrc` files load their own prompt and plugins, which would fight goatty's. What it sets up (PATH folders, variables) comes along without running it through the import below, and its aliases can be copied into goatty's once. With `user_rc` on, it runs guarded after compinit and before goatty's plugins (see [Running your .zshrc](#running-your-zshrc-guarded)), so goatty's prompt still wins. Turn off the Starship plugin to keep your own prompt.

## Importing your shell setup

Installers (rustup, nvm, bun, pnpm, conda, Homebrew) and years of habit put PATH entries, `EDITOR` and aliases in `.zshrc` or `.bashrc`, which goatty's zsh doesn't run. `shell.import` (Settings → Dotfiles; on by default, and shown in the welcome window with what it brings when you have such a file) brings the PATH folders and variables they set up into goatty's shell without running them there, so a tool an installer set up works in goatty's first tab, also on a remote host where goattyd starts from sshd's minimal environment. Code: `goatty_shell::import`.

**The harvest.** goattyd runs your login shell the way your old terminal did, `zsh -l -i -c …` or `bash -l -i -c …`, in its own process group with no terminal, stdin from `/dev/null`, stdout discarded and stderr kept (shortened, without bash's job-control complaints). Once the startup files are done, a script writes every exported variable, the aliases (zsh: regular, `-g`, `-s`) and the final `ZDOTDIR` as NUL-separated records to a file, with alias expansion off first so the startup's aliases can't change it. It gets goattyd's environment without goatty's own variables, plus `GOATTY_IMPORT=1` (a `.zshrc` can skip slow parts), `DISABLE_AUTO_UPDATE=true` (oh-my-zsh) and `POWERLEVEL9K_INSTANT_PROMPT=off`. A login bash reads `.bashrc` only through its login file, so the harvest sources `~/.bashrc` itself when the first of `.bash_profile`/`.bash_login`/`.profile` doesn't mention it. After the shell exits, its process group is killed, so nothing it started in the background outlives it, unless it moved itself to a session of its own (`setsid`, as daemons like `ssh-agent` do): that keeps running.

**What's kept.** Exported variables the startup set or changed, except shell, terminal and session state (`import::is_shell_state`): `PWD`, `SHLVL`, `ZDOTDIR`, `ZSH*`, prompts, history settings, `TERM*`, `SSH_*`, `GPG_TTY`, `STARSHIP_SHELL`, `P9K_*`, `GOATTY_*`; the desktop session's and the terminal's own (`DISPLAY`, `WAYLAND_*`, `DBUS_SESSION_BUS_ADDRESS`, `XDG_SESSION_*`, `TMUX`, `KITTY_*`, `VSCODE_*`, …); an activated virtualenv, conda or nix shell (`VIRTUAL_ENV`, `CONDA_*`, `IN_NIX_SHELL`, `DIRENV_*`); and short-lived credentials (`AWS_SESSION_TOKEN`). The PATH folders it *added*: what the shell started with is goattyd's own PATH (it may come from a venv or `nix develop` goattyd was started in) and is never baked into the import. The aliases, each marked as your own when an `alias NAME=` line in one of your files defines it (the startup files, `~/.bash_aliases`, `~/.aliases`, oh-my-zsh's `custom/`, …), else as a framework's. `Auto` takes the login shell's files (passwd, else `$SHELL`); a zsh without any startup files of yours, but with a `.bashrc` or `.bash_profile`, takes bash's. With another login shell (fish), `Auto` takes zsh's files when you have some, else bash's; with neither there's nothing to import.

**Applying it.** The harvest is cached in `$XDG_STATE_HOME/goatty/import/harvest.json` and written as `import.zsh` (arrays only). goatty's `.zshrc` applies it right after the system's zshrc: variables (not the skipped ones, not read-only ones) are exported, and the added PATH folders that aren't on PATH yet go in front of it (`${imported:|path}`), so plugins find your tools; a folder your `.zshenv`/`.zprofile` already put on PATH keeps its place, and goatty's `shell.path` and `bin/` still come after.

**Aliases are copied once, not imported.** Re-applying a `.zshrc`'s aliases in every shell would bring a framework's hundred-odd aliases along and keep them out of goatty's alias editor. Instead the Aliases page offers "Your aliases from ~/.zshrc" (`import::offer_aliases`): plain aliases with a name goatty accepts that aren't in goatty's `aliases` yet, your own picked, the frameworks' behind "Show more". Adding them writes them to `aliases` (group "From ~/.zshrc") like any other alias; from then on they're goatty's, and your file stays as it is. The settings live in `settings.zsh`, so switching entries off is a generation like any setting; `import.zsh` only changes with a new harvest. `goatty-status` reports `import` (ok with the files, or missing), and the PATH page attributes imported folders to `~/.zshrc (imported)`.

**When it runs.** `Launcher::launch` (every new session) checks a fingerprint of the shell, its startup files (size and mtime of the user's and the system's, `/etc/profile.d` included) and the environment it starts with. A change, or no harvest yet, harvests before the session starts: waiting for the lock and the harvest together takes at most 5 s. When another harvest holds the lock that long, the session uses the cached one. A harvest older than a day is redone in the background (with up to 30 s), for files the startup files source; installers edit the startup files themselves, which harvests at once. goattyd harvests when it starts and after an apply with the import on, so the first tab and reloaded shells don't wait. A failed harvest keeps the previous one and is shown with its reason and stderr on the Dotfiles page and by `goattyd shell import` (which exits non-zero when `--refresh` fails). Failures are a timeout (the shell and everything it started are killed) or a startup that runs `exec tmux` or `exit` and never reports. A failure is cached like a harvest: it's retried once a startup file changes or it's older than a day, or with Check again. A startup file that changes while the harvest runs leaves the harvest marked stale, so the next session harvests again. The import directory is private (0700, files 0600): it holds your environment, tokens included. A lock there keeps processes from running your startup twice at once.

**Not imported:** shell functions and hooks (`nvm`, `conda activate`, `pyenv shell`, direnv's or zoxide's hooks). Their PATH effects come along (node from `nvm use default` is on PATH), the functions themselves don't. Run `.zshrc` guarded for those, or use goatty's plugins for zoxide, direnv and the like.

`goattyd shell import [--refresh]` prints the same as the Dotfiles page: where it reads from, the files, the last harvest (PATH folders, variables, the aliases goatty doesn't have yet, stderr) and the last failure.

## Running your .zshrc, guarded

With `shell.zsh.user_rc`, goatty's `.zshrc` sources your `.zshrc` at top level with your `ZDOTDIR`, but while it runs, `source` and `.` are aliases for `source rc-guard.zsh`, which receives the file as `$1`. A sourced file still runs at top level (a sourced file, not a function), so its `typeset`s stay global. The alias passes the caller's arguments along, so a file sourced without arguments sees the same `$@` as the file that sourced it, like plain `source`; `source -- FILE` works too. `rc-guard.zsh` skips what would fight goatty's shell and records it in `goatty-status` as `rc:<name>` (off): oh-my-zsh (`oh-my-zsh.sh`), Powerlevel10k (instant prompt, theme, `.p10k.zsh`), prezto, zim, antigen, antidote, zinit, zplug, and second copies of zsh-autosuggestions, zsh-syntax-highlighting (fast-syntax-highlighting too), fzf-tab and fzf's key bindings, each only while goatty's plugin for it is on. A skipped framework's commands (`omz`, `p10k`, `antigen`, `antidote`, `zinit`/`zi`, `zplug`, `zimfw`, `pmodload`) do nothing for the rest of the file, so `zinit light …` lines after it don't print "command not found". `compinit` is a no-op while it runs (goatty ran it; its own definition comes back afterwards), and `starship init`, `oh-my-posh init` and `fzf --zsh` print only a line that records them (they're run inside `eval "$(…)"` or `source <(…)`). Everything else runs: your functions, key bindings, options, zstyles, hooks. Afterwards the aliases and stubs are gone. A prompt your `.zshrc` sets stays when no prompt plugin is on; otherwise goatty's fallback prompt is used.

### The prompt

The `prompt` settings (`goatty_config::prompt`) cover about what `p10k configure` does, on top of Starship. The settings window's Prompt page edits them with a live preview.

- **Style**: `rich` is Starship's own look ("on main", "via 🦀 v1.85"); `lean` is colored text without words; `classic` puts every segment on one bar (`colors.segment`, 236 by default) with thin `separator`s between segments; `rainbow` gives each segment its own background (the module's color) and text in `colors.segment_text`. In `classic` and `rainbow`, `heads` (the end pointing toward the input) and `tails` shape the segment ends: flat, sharp, round, slanted, blurred.
- **Symbols** come from Starship's presets: `nerd` (`nerd-font-symbols`, drawn with goatty's bundled JetBrainsMono Nerd Font whatever the terminal font is), `emoji` (Starship's defaults), `text` (`plain-text-symbols`: any font), `none`.
- **Layout**: `two_line` puts the input on its own line; then `frame` (`left`: `╭─`/`╰─`, `full`: also `─╮`/`─╯`) and `connection` (`dotted`/`solid`, between the left side and what's on the right) apply. `transient` redraws an accepted command's prompt as just the prompt character (`.zshrc`, after integration so it keeps the OSC 133 marks; `transient_rprompt` drops the right prompt).
- **Modules**: every Starship module goatty knows (`goatty_config::prompt::MODULES`, from Starship 1.27's defaults and presets) can be switched (`show: essential` starts from directory, git and duration), put on the `right` (after a `$fill` on the first line with two lines, else `right_format`), and given its own `symbol` and `color` (text in lean/classic, background in rainbow; git's modules share the branch's segment). The directory always shows; the command duration's switch is `duration_threshold_ms`.
- **Directory**: `truncation_length` (4; 0 = whole path), `truncate_to_repo`, `abbreviate` (keep cut folders as their first letters, like fish), `truncation_symbol`, `home_symbol`, `read_only`, `repo_root` (bold), `substitutions`.
- **Character**: `success`/`error`/`vicmd` symbols ("" = `❯`/`❯`/`❮`, or `>`/`x`/`<` with text symbols) and `success_color`/`error_color`.

Colors are Starship colors: the theme's 16 by name (`bright-blue`) or number, 16–255, or `#rrggbb`. `PromptSettings::layout` describes what goes where (modules, ends, separators, frame, fill); `goatty-shell/src/starship.rs` writes it as Starship format strings and the preview (`goatty-app/src/ui/prompt_preview.rs`) draws it with sample values. Separators between Classic segments survive modules that don't show because each sits in a conditional group with its segment, on the side away from an always-shown anchor (the directory, or the OS icon). `host.json`'s `minimal` style (up to version 3) loads as `lean` with text symbols, essential modules and no branch symbol (version 4).

Each plugin checks for its tool at shell startup and is skipped if the tool is missing. Binaries come from `$commands`, which sees the PATH after the user's `.zprofile` ran; zsh plugins come from their package's `system_paths` (distro/Homebrew/NixOS), then the managed copy. A system install always wins over a managed one.

Defaults chosen here: emacs keymap (with navigation and word-deletion bindings in both `emacs` and `viins`); Up/Down search history by prefix; Home/End/Delete/Ctrl+arrows/Ctrl+Backspace/Ctrl+Delete/Shift+Tab bound to the sequences goatty sends; word motions stop at `/`; completion with case-insensitive matching, in fzf (fzf-tab; zsh's menu selection when fzf-tab is off or missing); Tab on an empty prompt completes the command instead of inserting a tab (#250; `insert-tab` is true only on continuation lines, where `$PREBUFFER` is set, so a `for` body or heredoc can still be indented): commands on PATH, builtins, aliases, your functions and, with `auto_cd`, directories, but no parameters and none of the completion system's `_` functions; `auto_cd`; 100k history entries, `extended_history`, `inc_append_history`, no duplicates; fzf on Ctrl+R/Ctrl+T/Alt+C.

## Completions for goatty and its tools

`share/zsh/site-functions/` (`goatty_shell::completions`) is appended to `fpath` before `compinit`, after the system's directories, like `bin/` on PATH:

- `goatty` and `goattyd` write their own `_goatty` / `_goattyd` (generated by clap) whenever they start (the GUI, `goattyd serve`), so they match the installed version. A remote host gets `_goattyd` when its goattyd starts. `goatty completions SHELL` and `goattyd completions SHELL` print the same scripts for bash, fish, zsh, elvish and PowerShell outside goatty's shell.
- A plugin package with `completions` (`starship completions zsh`) gets `_<exe>` right after goatty installs its copy; uninstalling the package removes it. When goattyd starts it also fills in missing ones for copies installed earlier. A copy on the system brings its own completions, so nothing runs for it. fzf's come with its key bindings (`fzf --zsh`).

Adding or removing a function deletes `zcompdump-*`, so the next shell's `compinit` scans `fpath` again. A changed function needs nothing: zsh loads the file when the command is first completed.

## fzf: history, files, completion

fzf is goatty's one fuzzy finder (#165), split over two built-in plugins:

- **fzf** loads fzf's key bindings (`fzf --zsh`): **Ctrl+R** searches the zsh history (the same file plain zsh uses) and puts the pick on the command line, **Ctrl+T** inserts files, **Alt+C** changes directory. Files and directories come from `fd` (`fdfind` on Debian/Ubuntu: hidden ones included unless `hidden_files` is off, `.git` excluded, symlinks followed); without fd, fzf walks the directory itself with the same rules (`--walker`, `--walker-skip=.git`). With `previews` on, Ctrl+T previews the file with bat and Alt+C the directory with eza, each only when that tool is installed.
- **fzf-tab** (Aloxaf/fzf-tab 1.3.0) makes Tab open the matches in fzf: no group headers, `/` accepts a directory and completes inside it, zsh's own menu off. Its previews: directory contents (eza) and files (bat) for paths and `cd`, the man page for a command name (a directory there: its contents), the full command line for `kill`/`ps`, the value for variables (`export`, `unset`, `$…`), `git diff` for `git add/diff/restore`, the last commit for `git log/checkout/switch/show`, `systemctl status`, `tldr` and `man` (at the preview's width). It needs fzf (`requires.commands`) and loads after fzf's bindings, before autosuggestions.

**Look.** fzf's options come from a file the fzf plugin generates (`plugins/fzf/fzfrc`, through `FZF_DEFAULT_OPTS_FILE`): reverse layout, 60 % height, and colors given as the terminal's 16 ANSI colors (`fg:-1,bg:-1,hl:2,…`), so fzf, its previews (bat with `--theme=ansi` unless `BAT_THEME` is set, eza's own ANSI colors) and fzf-tab follow goatty's theme, local or remote. Previews sit on the right at 60 %, below in panes narrower than 80 columns, wrap long lines, and Ctrl+/ cycles below/hidden/right. fzf's key bindings paste that file into `FZF_DEFAULT_OPTS`, which fzf splits like a shell: values with `|`, `<` or `(` are quoted (`zsh::tests::fzf_options_survive_the_key_bindings`).

**Yours win.** `FZF_DEFAULT_OPTS` is applied after goatty's file; `FZF_DEFAULT_OPTS_FILE`, `FZF_DEFAULT_COMMAND`, `FZF_CTRL_T_COMMAND`, `FZF_ALT_C_COMMAND`, `FZF_CTRL_T_OPTS` and `FZF_ALT_C_OPTS` set before goatty's plugins load (`.zshenv`, `.zprofile`) are kept, and `~/.config/goatty/zshrc` runs after them anyway (zstyles too). The values goatty exports are remembered in `_goatty_fzf_<NAME>`, so `goatty-reload`, whose new shell inherits them, replaces them instead of mistaking them for yours.

**Missing helpers.** fd, bat and eza are optional: each plugin records them in `goatty-status` as `helper:fd`, `helper:bat`, `helper:eza` (ok with the path, or `missing`; bat and eza only while previews are on). Diagnostics shows them as optional rows that don't count as missing tools, and neither `goatty-setup` nor the first-run setup installs them; the registry's fd, bat and eza plugins do.

**Atuin** was a built-in plugin until #165. It's now a registry plugin (`goattyd plugin install atuin`) that loads after fzf and takes Ctrl+R, optionally Up. A download goatty made for the old built-in (`tools/atuin/`, `bin/atuin`) is reused by that plugin at the same version, or removed with `goattyd shell uninstall atuin` (or `--all`).

## goatty's shell in every terminal (#283)

Off by default. Settings → Shell → "Use goatty's shell in every terminal" (per host; also in the welcome window, and `goattyd shell everywhere on|off|status`) makes every interactive zsh on the host run goatty's shell, not only goatty's tabs: VS Code's and other IDEs' terminals, other terminal emulators, tmux and herdr panes, SSH logins. Code: `goatty_shell::everywhere`, `zsh/entry.zsh`.

**The hook.** With the user's consent (the dialog, or the CLI's question, shows the file and these exact lines), goatty appends to the `.zshenv` a new zsh reads: `$ZDOTDIR/.zshenv` when the environment or `/etc/zshenv` sets `ZDOTDIR` (asked from `zsh -f`, which still reads `/etc/zshenv`), else `~/.zshenv`:

```zsh
# goatty: use goatty's managed zsh in every terminal (Settings → Shell). Delete these 2 lines to stop.
[[ -r ~/.local/share/goatty/current/zsh/entry.zsh ]] && source ~/.local/share/goatty/current/zsh/entry.zsh
```

The path is `~/…` when it needs no quoting (a `.zshenv` synced between machines keeps working), else quoted. The line goes at the end, so a `ZDOTDIR` the file sets is the user's; status warns about a line after it that sets `ZDOTDIR`. A `.zshenv` that's a link (a dotfiles repository) is edited where it points, through a temporary file, with its permissions; when that fails (a read-only Nix store), goatty shows the lines to add there. Turning it off removes exactly goatty's comment and line, wherever they are, and leaves the rest byte for byte (the newline goatty may have added before them stays). Both keep a backup in `~/.local/state/goatty/zshenv-backups/`. A line that mentions goatty's `entry.zsh` but isn't goatty's line (edited, commented out, copied) is reported as changed, and goatty neither adds nor removes anything until the user sorts it out. The switch shows what the file has, so a line added or removed by hand shows too.

Why not `chsh` to a wrapper: a missing or broken wrapper (uninstall, failed update) would lock the user out of login, `su` and SSH; it needs sudo for `/etc/shells`, on every remote too; and `$SHELL` would stop being zsh, which VS Code's shell detection, IDEs and scripts check. The hook fails safe: without `entry.zsh` (goatty uninstalled, a generation from before this) zsh stays plain without a word, and the login shell stays `/usr/bin/zsh`. starship, atuin, mise and direnv integrate the same way: one visible line.

**`entry.zsh`** (rendered into every generation, so a rollback or turning the managed zsh off takes effect) does nothing for non-interactive shells (`zsh -c`, scripts), for shells that are goatty's already (`_goatty_dir` is set: goattyd started it, or goatty's `.zshenv` is sourcing the user's `.zshenv` right now, so nothing loops), for the import's harvest (`GOATTY_IMPORT`, below), with `GOATTY_SHELL_MANAGED=0` (or `false`, `no`, `off`: plain zsh for one shell, the escape hatch), and while `shell.managed` is off or goatty's shell isn't zsh. Otherwise it sets `GOATTY_ORIG_ZDOTDIR` to the user's `ZDOTDIR`, points `ZDOTDIR` at goatty's directory and sources goatty's `.zshenv` right there, in hook mode (`_goatty_hooked`: it doesn't source the user's `.zshenv` again, which is running). From then on it's the same startup as a goatty session:

```
/etc/zshenv
~/.zshenv (the user's)        ... their lines ..., then goatty's line → entry.zsh → goatty's .zshenv (settings.zsh; ZDOTDIR = goatty's)
/etc/zprofile                 (login shells)
goatty's .zprofile            the user's .zprofile with their ZDOTDIR      (login shells)
/etc/zshrc
goatty's .zshrc               as in a goatty session; ZDOTDIR is the user's again at its end
the user's .zlogin            (login shells)
```

Nested shells and `exec zsh` from a hooked shell are goatty's again (that's the point); `goatty-reload` works as in goatty. Running shells don't reload when settings change (only goattyd's sessions get `SIGURG`): new ones pick it up.

**Inside goatty or not.** goattyd's PTYs set `GOATTY_TTY` to the pane's terminal (`goatty_term::pty::TTY_ENV`), next to `TERM_PROGRAM=goatty`. The `.zshrc` counts as inside goatty only when both match `$TTY` and goatty: programs started from a goatty tab (an IDE, tmux) inherit the variables, but their shells run on other terminals. Remote goattyd sessions set them on the remote. Outside goatty:

- OSC 7337 and the command-line widgets (`^[[7337;N~`) stay out. OSC 133 and OSC 7 still go out, unless the terminal brought its own shell integration (VS Code's `VSCODE_SHELL_INTEGRATION`, kitty's `KITTY_SHELL_INTEGRATION`, Ghostty's `_ghostty_deferred_init`): then it marks the prompts and goatty's marks would double them.
- The prompt uses `starship-plain.toml`: text symbols instead of Nerd Font icons, thin separators and flat ends instead of Powerline shapes, and the user's own symbols that are Nerd Font glyphs back to the defaults (`PromptSettings::plain`). goatty draws those glyphs with its bundled font; other terminals (VS Code's default font) would show boxes. `prompt.icons_elsewhere` (Settings → Prompt → "Icons in other terminals") keeps them.

**Freshness.** goatty's sessions rebuild the generation and redo the harvest when they start; nobody does that for shells elsewhere. So `entry.zsh` compares the user's and the system's startup files (zsh's and bash's), `host.json` and the goattyd binary against `~/.local/state/goatty/shell-refreshed` (a few `-nt` tests, no process). When one is newer, it runs `goattyd shell refresh --wait 2`: the refresh runs in a process of its own (its own session, so closing the terminal doesn't stop it), and the shell waits for it at most 2 s, then goes on with the cached configuration (the next shell gets the new one). An installer that just appended to `.zshrc` (rustup, nvm, bun, conda) works in the next terminal, as long as one run of the user's startup files takes less than that. The refresh asks the running goattyd when there is one, so the harvest runs with goattyd's environment, which the import's fingerprint compares against; without goattyd it runs in that process, without `ZDOTDIR` (the asking shell is in the middle of `.zshenv`), and the next goatty session may harvest once more because its environment differs. goatty never starts goattyd from a shell's startup: it would inherit that terminal's environment. The stamp gets the time the check *started*, so a file changed while it ran counts as newer. `Launcher::launch` (every goatty session) and `goattyd shell refresh` both touch it.

**The harvest bypass.** The import runs the user's login shell to see what their files set up; with the hook it would harvest goatty's own config. It sets `GOATTY_IMPORT=1`, so `entry.zsh` returns at once (`tests/zsh_startup.rs::harvest_with_the_hook_reads_the_users_own_files`).

**What changes for the user** (the dialog says so): `~/.zshrc` no longer runs in those shells, as in goatty's tabs. PATH folders and variables still arrive through the import; functions, aliases and key bindings go in `~/.config/goatty/zshrc`, or `shell.zsh.user_rc` runs `.zshrc` guarded (the dialog has that switch). Prompt icons fall back to text (above).

**Interop**, checked against the real scripts in `tests/zsh_startup.rs` when they're installed:

- **VS Code** injects its integration by setting `ZDOTDIR` to a directory of its own and `USER_ZDOTDIR` to the user's (or `$HOME`); its `.zshenv` sources `$USER_ZDOTDIR/.zshenv` (ours runs, `ZDOTDIR` becomes goatty's), remembers that as `USER_ZDOTDIR` and reads goatty's `.zprofile` and `.zshrc` through it. Both load (OSC 633 command detection and decorations, goatty's plugins and prompt). Three things needed handling: VS Code sets `HISTFILE=$USER_ZDOTDIR/.zsh_history`, inside goatty's generation, so the `.zshrc` resets a `HISTFILE` in goatty's directory to the usual one; after startup VS Code sets `ZDOTDIR=$USER_ZDOTDIR` (goatty's), which the first prompt hands back; and its `.zlogin` sources `$USER_ZDOTDIR/.zlogin`, so goatty's directory has a `.zlogin` that runs the user's.
- **kitty** (and **Ghostty**, which copied its approach) restores `ZDOTDIR`, sources the user's `.zshenv` at top level and loads its integration deferred to the first prompt. Both load; goatty's OSC 133 marks stay out.
- **macOS Terminal**: `/etc/zshrc_Apple_Terminal` keeps session files in `${ZDOTDIR:-$HOME}/.zsh_sessions`, read while `ZDOTDIR` is goatty's; the `.zshrc` moves `SHELL_SESSION_DIR`/`SHELL_SESSION_FILE` back like `HISTFILE` (macOS's `/etc/zshrc` sets `HISTFILE` that way too, also in goatty's own sessions).
- **JetBrains** terminals, **tmux** and **herdr** panes, `ssh` into the host, `su - user` and `sudo -i -u user` start ordinary interactive (often login) shells that read `.zshenv` (please verify JetBrains by hand: its injection wasn't available to test).

## Shell integration

Outside goatty (above), only OSC 133 and OSC 7 go out, and OSC 133 only when the terminal has no shell integration of its own. `integration.zsh` emits OSC 133 `A` (prompt start) and `B` (input start) inside `PS1`, re-added on every prompt if a theme rebuilt it. It emits `C` from `preexec`, then `D;<status>` and OSC 7 (`file://host/percent-encoded/path`) from `precmd`. zsh restores `$?` for every hook, so hook order doesn't affect the status. rio-vt parses OSC 7; goatty-term applies the OSC 133 marks itself: they mark prompt rows (so the output viewer finds each command's output and Ctrl+Shift+Up/Down jump between prompts), say whether a command runs, and finished commands show as tab activity.

The right-click menu (#186) reads and changes the line typed at the prompt through zle widgets bound to private key sequences (`goatty_term::input::LineEdit`, `ESC [ 7337 ; n ~`, which no keyboard sends; bound in the emacs, viins and vicmd keymaps): `1` reports `$BUFFER` as `OSC 7337;line=<percent-encoded bytes>` (`UiEvent::CommandLine`; goatty acts on it only while it asked), `2` puts `sudo ` in front or takes it away (an empty line gets the previous command first), `3` runs zsh's `edit-command-line` (`$VISUAL`/`$EDITOR`) between `zle=0` and `zle=1`, since the editor owns the terminal meanwhile, `4` is `kill-buffer`. `precmd` prints `OSC 7337;zle=1` so goatty knows the widgets are there; `C` clears that, and the app also holds back while the pane's foreground program isn't a shell, so another shell or a running program never gets the keys. Attach and resync snapshots carry the flag (`zle=0|1` after the shell state). The encoding deliberately runs without `LC_ALL=C`: zle would turn characters C can't spell into `?`. Tested against real zsh in `tests/zsh_startup.rs::command_line_widgets_report_and_change_the_line`.

## Installing and updating tools

`goattyd shell install [NAME…] [-y]` (or `goatty-setup` inside a goatty shell, which passes the missing default tools and outdated managed copies of enabled built-in plugins, then reloads the shell) installs and updates the packages of built-in plugins. `goattyd shell update [NAME…] [-y]` updates only goatty's outdated managed copies (without names: all outdated managed copies; skips system copies). They plan packages like any plugin install (`Plugins::plan` on the built-in candidate: a package already on the system or already installed by goatty at the pinned version downloads nothing), show downloads/updates with sizes and ask first; without a terminal they require `--yes`. `goattyd plugin install starship` and `goattyd plugin update` do the same with the full plan (permissions, shell code). Each package:

1. Downloads the pinned release archive with `curl` (fallback `wget`). The sha256 pin is what makes an archive trusted, so goattyd needs no TLS stack of its own.
2. Checks the archive against the sha256 in the plugin's manifest before unpacking anything.
3. Unpacks into `tools/<id>/.<version>.partial-<pid>` (`goatty_plugin::archive` handles every package format, tar.gz/xz/zst, zip or a raw executable, and refuses paths outside it), checks the expected binaries or `source` script, then renames it to `tools/<id>/<version>`.
4. Switches `bin/<exe>` or `plugins/<id>` by renaming a temp symlink over the old one, then removes older versions.

No sudo, nothing outside goatty's data directory. `goattyd shell uninstall NAME…|--all` removes goatty's downloads (the plugins stay on and use system copies if there are any). For a remote goattyd this all happens on the remote host, which is also where `goatty-setup` runs.

| Built-in plugin     | Package (id)                               | Version | Builds                    | Load slot   | Installed by goatty-setup |
|---------------------|--------------------------------------------|---------|---------------------------|-------------|---------------------------|
| fzf                 | fzf (`fzf`)                                | 0.74.5  | x86_64/aarch64 Linux, macOS | line-editor | yes |
| fzf-tab             | fzf-tab (`fzf-tab`)                        | 1.3.0   | source tarball            | line-editor (after fzf) | yes |
| autosuggestions     | zsh-autosuggestions (`autosuggestions`)    | 0.7.1   | source tarball            | line-editor (after fzf, fzf-tab) | yes |
| syntax-highlighting | zsh-syntax-highlighting (`syntax-highlighting`) | 0.8.0 | source tarball          | last        | yes |
| starship            | Starship (`starship`)                      | 1.27.0  | x86_64/aarch64 Linux musl, macOS | prompt      | yes |

Pinned 2026-11-07 to releases at least two weeks old (fzf-tab 2026-11-13: v1.3.0 is from 2026-03-16; the macOS builds of the same fzf and Starship releases 2026-11-17). Checksums were computed from the downloads and matched upstream's published `.sha256`/checksums files where they exist (Starship, fzf); GitHub's source tarballs (fzf-tab, the zsh plugins) have none, so their checksum was taken from two separate downloads. fzf-tab's system copies: Arch's AUR `fzf-tab-git`, NixOS/Nix `zsh-fzf-tab`. The package ids are the tool ids from before #98, so managed installs made then (`tools/<id>/`, `bin/` and `plugins/` links) are used as they are. zsh itself isn't installed: if it's missing, goatty starts `$SHELL` and `goattyd shell status` says so.

### Bumping a tool pin (#45)

When a built-in tool's upstream release is at least two weeks old:

1. Download the release archive(s) and compute each archive's sha256 and byte size (or run `goatty-plugin hash <URL>...`, which fetches the URLs, computes `sha256` and `size`, and lists executables).
2. Compare the computed `sha256` with upstream's published checksums file where available (Starship, fzf). For source tarballs without published checksums (`fzf-tab`, `autosuggestions`, `syntax-highlighting`), compare two separate downloads to catch transient corruption.
3. Update the built-in manifest in `crates/goatty-shell/plugins/<id>/plugin.toml`: set `[[package]].version`, update each `[[package.asset]]`'s `url`, `sha256`, `size` (and `root` if the unpacked directory name embeds the version, e.g. `fzf-tab-<version>`). Update the pin table and date note above in `docs/managed-zsh.md`.
4. Verify the manifests and links with `cargo test -p goatty-shell` (and `goatty-plugin check-downloads crates/goatty-shell/plugins/<id>` to re-fetch and verify every asset URL, size and checksum).

## First run and the GUI

On the very first start (no `settings.json`, no `~/.local/state/goatty/welcome-shown`), goatty opens a **welcome window** with three steps: a theme (or follow the desktop) and the text size; the built-in tools of the window's host, the missing or outdated managed ones preselected and installed one at a time with progress, the import, goatty's shell in every terminal (off; the file and the exact lines are shown above the switch, so turning it on is the consent), plus the prompt style; then the shortcuts worth knowing. Everything else keeps its defaults. The command palette's "Welcome and setup" opens it again.

When a tab connects to another host, goatty offers to bring this computer's whole shell setup there, tools included ([`profiles.md`](profiles.md#the-same-shell-on-every-host-159)). When a tab connects to this computer, or to a host set to never sync, and its managed shell misses default tools or has outdated managed copies, a sticky toast offers to install/update them: Install / Update / Not now / Don't ask again. That happens once per host per run, never while the welcome window is open, and never after "Don't ask again" (the same marker as `goatty-setup --dismiss`). The in-shell hint stays quiet in sessions of a goatty window that offers the setup itself (goattyd sets `GOATTY_GUI_SETUP=1` for clients with protocol v9, `goatty --local` always; the shell unsets it after startup), and for missing tools on hosts where a window already offered the install (`shell-setup-offered`); outdated managed copies still show the in-shell hint unless dismissed (`goatty-setup --dismiss`). Installing or updating reloads the host's running shells, so an idle prompt has the new tools at once.

The GUI talks to the host through `goatty_shell::setup` (protocol v9 `Shell`, JSON like the plugin operations; with `--local` in-process): `status`, `install` (built-in tool ids), `offered`, `dismiss`. A goattyd older than v9 gets no offer and the Diagnostics page says to restart it.

## Diagnostics

Settings → Diagnostics (or `goatty --settings=diagnostics`, or "Shell diagnostics" in the palette) shows the same report as `goattyd shell status` for the window's host: the program and configuration, the startup time, each feature with where it came from (system path or goatty's copy and its version), what the startup printed to stderr, the user's own startup files, an Install button for missing tools, and an Update button when goatty's managed copy differs from the pinned version. "Copy report" puts it on the clipboard for a bug report.

`goattyd shell status` prints the configuration, then starts the managed shell the way a session would (login + interactive, no TTY), runs `goatty-status`, and lists each feature with where it came from, flagging outdated managed copies (`⚠ managed <Name> <old> → <pin>: goattyd shell update <id>`), and the startup time. Anything the startup printed to stderr is shown. Inside a shell, `goatty-status` prints the same feature table, and new interactive shells print a hint when default tools are missing or managed copies are outdated.

## PATH

Settings → PATH (`goatty --settings=path`) edits `shell.path` (add, browse on this computer, reorder, remove) and shows the PATH a new shell gets, each folder with where it came from. Interactive startups note `$PATH` after each step in `_goatty_path_marks` (`step<TAB>PATH`): `etc-zshenv`, `zshenv`, `etc-zprofile`, `zprofile`, `etc-zshrc`, `imported` (with `shell.import` on), `settings` (`shell.path`), `tools` (goatty's `bin/`), `plugins`, `zshrc`, `additions`. `goatty_shell::path_report` (the `path` request) starts the managed shell like Diagnostics does, prints the marks and the final PATH, and gives each folder the first step whose PATH has it; folders already there before the first step are inherited from goattyd's environment. It also checks each `shell.path` folder on the host: used, missing or not a folder (skipped), already on PATH from an earlier step (the setting changes nothing), or removed again by something later. A goattyd from before this gets an "update it" note instead.

## Startup cost

Measured 2026-11-07 on the dev machine (CachyOS, zsh 5.9.2), `zsh -l -i -c exit` with no TTY, median of 21 runs, sandboxed `$HOME`, completion dump already built:

| Configuration                                        | ms   |
|------------------------------------------------------|------|
| `zsh -f -i` (no startup files at all)                | 2    |
| plain login shell (system files only)                | 32   |
| goatty base: options, keys, compinit, integration    | 37   |
| + autosuggestions                                    | 42   |
| + syntax highlighting                                | 44   |
| + Starship init                                      | 59   |
| + Atuin init (everything on, Atuin built in then)    | 60–67 |

Moving the tools to built-in plugins (#98) didn't change it: 61 ms median on both `main` and the branch (2026-11-08, same machine, sandboxed `$HOME` with the host's Starship, fzf and distro zsh plugins, Atuin missing; `goattyd shell status` built the generation, then 21 runs of `zsh -l -i -c exit` each, two rounds).

fzf and fzf-tab by default (#165), measured 2026-11-13 on the same machine: sandboxed `$HOME` with the host's fzf 0.74.5, fd, bat, eza, Starship and distro zsh plugins, goatty's fzf-tab 1.3.0, median of 21 runs of `zsh -l -i -c exit`, two rounds. Without a TTY fzf's key bindings aren't loaded, so the TTY column (through `script`, which adds its own ~10 ms) is the one that counts:

| Configuration                        | no TTY ms | TTY ms |
|--------------------------------------|-----------|--------|
| fzf and fzf-tab off                  | 59.5–59.7 | 72.2–72.5 |
| + fzf (`fzf --zsh` and its eval, fd/bat/eza lookups) | 58.7–59.7 | 77.1–80.1 |
| + fzf-tab (sourcing it, zstyles)     | 62.5–64.4 | 83.6–84.3 |

So fzf cost about 6 ms (one process spawn for `fzf --zsh`) and fzf-tab about 5 ms.

goatty's part was about 30 ms, roughly half of it `starship init` (spawning `starship` for the init script and `PROMPT2`) and `fzf --zsh`. The first start after a zsh upgrade or on a fresh machine builds the completion dump (178 ms once in the real home dir; 47 ms after that, measured with `goattyd shell status`).

**Init script caching (#41).** Starship (`starship init zsh --print-full-init`, with `PROMPT2` pre-evaluated via `STARSHIP_SHELL=zsh starship prompt --continuation` when `$TERM != dumb`) and fzf (`fzf --zsh`) are cached and byte-compiled without alias expansion (`zcompile -U`) under `~/.cache/goatty/init/<name>-<hash>.zsh{,.zwc}` (`starship-dumb` when `TERM=dumb`) via `_goatty_cached_init`. Each cached file starts with a key header embedding `ZSH_VERSION`, `$commands[<cmd>]`, its resolved realpath (`:A`), subsecond `zstat` metadata (`device:inode:size:mtime:ctime`), plugin-specific inputs (for Starship: resolved `STARSHIP_CONFIG` or `$HOME/.config/starship.toml` path + `zstat` metadata and `TERM == dumb`), and command arguments. On a cache hit, zero subprocesses spawn and the caller runs `builtin source` at top level so `typeset` stays global. Regeneration triggers whenever the selected executable or any other cache-key input changes (binary update, PATH change that selects a different executable, `starship.toml` edit, or zsh upgrade); `goatty_shell::install` also drops `init/<name>*.zsh{,.zwc}` on package install/uninstall. Writes serialize on a kernel-released `zsystem flock` advisory lock, cache entries are verified to be non-symlinks owned by `$EUID` without group/other write bits, and contention, unsafe permissions, or non-writable directories fall back to in-memory `eval`.

Measured 2026-11-18 on the same dev machine (CachyOS, zsh 5.9.2, Starship 1.27.0, fzf 0.75.0, fd, bat, eza, distro zsh plugins, managed fzf-tab 1.3.0; sandboxed `$HOME` with `shell.import` off, `goattyd` from `main` vs cached):

| Measurement (median) | Before | After | Delta |
|---|---|---|---|
| `starship` init + `PROMPT2` (`zsh -f -i`, 23 warm runs, PTY) | 7.40 ms | 0.64 ms | -6.76 ms (11.6×) |
| `fzf --zsh` (`zsh -f -i`, 23 warm runs, PTY) | 6.93 ms | 1.44 ms | -5.49 ms (4.8×) |
| Combined init cost (23 warm runs, PTY) | 14.24 ms | 2.10 ms | -12.14 ms (6.8×) |
| Full `zsh -l -i -c exit` (all defaults, 41 runs, PTY) | 71.70 ms | 61.54 ms | -10.16 ms (-14.2%) |
| Full `zsh -l -i -c exit` (all defaults, 2×21 runs, no TTY) | 66.3–68.5 ms | 55.4–59.9 ms | ~-9.7 ms |

**goatty's shell in every terminal (#283).** Measured 2026-11-23 on the same machine (zsh 5.9.2, debug goattyd, sandboxed `$HOME` without the host's tools, `shell.import` off): `zsh -i -c exit` with no TTY, 81 runs interleaved, median. The hook's normal path (no refresh due) costs about 0.3 ms: 25.2 ms through the line in `.zshenv` against 24.9 ms for a shell started the way goattyd starts it (`ZDOTDIR` set). The 25 `-nt` checks of the startup files take 0.03 ms of that. A refresh, when a startup file changed, waits at most 2 s once.

## Expansions

`shell.expansions` (on by default) sources `expand.zsh` right after `compinit` and before the line-editor plugins, so autosuggestions and syntax highlighting wrap its widgets and the user's `.zshrc` or additions can still rebind Space, Enter and Backspace. `| first 20 lines` + Space becomes `| head -n 20`; Enter on a line ending in a rule expands it and stops; Backspace right after undoes it; nothing changes inside quotes or after a paste. `goatty-status` shows `expansions ok <n> rules`. Design, rules and the cost per keypress (≈40 µs on a plain word, under 0.5 ms with 300 rules; 0.56 ms at startup with Basics): [`snippets.md`](snippets.md).

## Plugins

Installed plugins (Stage 5, [`plugins.md`](plugins.md)) render into `plugins-early.zsh`, `plugins.zsh` and `plugins/<id>/` of each generation, so previews, `zsh -n` validation and rollback cover them like everything else. `goatty-status` lists them as `plugin:<id>`.

## Not done yet

Installs for other architectures and a distro-specific hint when zsh is missing (#42), an interactive GUI smoke test (#47), and the terminal side of shell integration (#28).

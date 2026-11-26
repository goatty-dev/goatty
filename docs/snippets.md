# Snippets and expansions (#89)

A guided command helper for people who don't know a command by heart yet.

- **Snippets**: you search for what you want to do ("find big files", "extract", "stop a program"), goatty asks for the missing values one at a time (with suggestions such as your running programs or the repository's branches), shows the finished command and puts it on your prompt to read and run. Nothing runs until you press Enter.
- **Expansions**: plain words turn into a command while you type. `| first 20 lines` followed by Space becomes `| head -n 20`.

Plugins ship both as data (`[[snippet]]`, `[[expansion]]`); the user keeps their own in `host.json`. goatty comes with the **Basics** pack (`crates/goatty-shell/plugins/basics/plugin.toml`): about 35 everyday snippets and a dozen expansions, on by default, turned off like any built-in plugin.

## Using it

- **Ctrl+Shift+S** (action `snippets`; palette: "Snippets: Find and fill in a command") opens the picker over the panes: fuzzy search over titles (highlighted), descriptions, tags, keywords and the command itself; topic chips (Tab / Shift+Tab step through them); recently used snippets first; snippets whose program the host doesn't have go last, dimmed ("Needs ip, which this host doesn't have"). A row is the title and one line of description; a plugin's snippets name the plugin on the right (yours say "Yours", the Basics nothing), and a snippet that deletes data has a small red trash icon (its tooltip says what that means). Topic tags and the "Makes changes" safety level aren't shown in the list: tags are the chips above it, and nearly every snippet changes something. The selected one's command is shown below with its blanks (`ping -c 4 ‹Computer or website›`, defaults filled in), where it comes from, how many blanks are left and, for a destructive one, "Deletes data, there's no undo".
- **Enter** fills it in, in the command palette's input mode (#91): one field per blank, the finished line above the fields updating as you type (the blank being asked highlighted), Enter next, Esc back (on the first field: back to the picker, where you were), Tab fills in a suggestion. The last Enter puts the line on the prompt of the focused pane, at the cursor, without pressing Enter. It goes through the paste check like any paste, so a `curl … | sh` still asks first. When another program runs in the pane or the tab is on another host than the snippet's, goatty offers to copy the line instead (`typing.rs`, shared with plugin palette commands).
- **Settings → Snippets**: your snippets (edit, delete), the plugins' grouped and folded ("Make my own copy" opens one in the editor), the expansions with the `shell.expansions` switch, a live "Try it" field, and yours to add, edit, switch off or delete. Edits are host settings: they go into the draft and are applied with Preview/Apply like the other shell pages.
- **The editor**: title, the command line (typing `{name}` into it adds an input row: question, type, starting value, choices / "yes adds" / a suggestions command, optional), description, tags, and what running it does (goatty's guess, or yours). The finished line with its blanks is shown as you type; problems (a placeholder inside quotes, an unused input) show before saving.
- **"Snippets: Save the last command as a snippet…"** (palette) takes the focused pane's last command line (from the OSC 133 prompt marks, prompt stripped) into a new snippet in the editor, where `{name}`s turn parts of it into blanks.
- **CLI** on the host: `goattyd snippet list | show ID [--plugin P] | add TITLE LINE [-d DESC] [-t TAG]… | rm ID… | export [FILE] | import FILE [--force]`. Export/import use the manifest's format (`[[snippet]]`, `[[expansion]]`), so a pack moves between hosts and into a plugin unchanged. Profile export and shell sync (#159) carry `snippets` and `expansions` with the other host settings.

## The format

```toml
[[snippet]]
id          = "switch-branch"             # lowercase letters, digits, - (the key in host.json for your own)
title       = "Switch to a branch"
description = "Moves your working copy to another branch."
tags        = ["git", "branch"]           # topic chips in the picker
keywords    = ["checkout"]                # other words people search for
safety      = "changes"                   # safe | changes | destructive; without it goatty guesses from the line
line        = "git switch {branch}"       # one line of shell text; {input} placeholders are the only variable parts

[[snippet.input]]                          # asked in order; the [[command.input]] fields (#91):
id    = "branch"                           # label, placeholder, help, type (text, integer, number, path,
label = "Branch"                           # folder, choice, flag), default, optional, min/max, pattern
strict = true                              # + pattern_help, choices, must_exist; and:
# multiple = true                          # several values, each its own quoted word (`git add {files}`)
# type = "flag", on = "-r"                 # yes/no: "yes" inserts the text, "no" nothing

[snippet.input.source]                     # suggestions, run on the tab's host
run     = ["git", "for-each-ref", "--format=%(refname:short)\t%(contents:subject)", "refs/heads"]
split   = "\t"                             # columns: a literal string, or " " for runs of spaces
column  = 1                                # the value (default 1)
# detail = 2                               # shown dimmed (default: the other columns)
# label = 2                                # shown and searched instead of the value (a name for a pid)
# skip = 1                                 # header lines (docker ps)
# cwd = "home"                             # default "pane": the focused pane's directory
on_fail = "Not in a git repository"        # under the field when it fails or prints nothing
# timeout = 2                              # seconds (default 2, at most 10)

[[expansion]]
say         = "| first {n} lines"          # at least two words; {name} is one word you type
to          = "| head -n {n}"
description = "Keep only the first lines of the output"
input.n.type = "integer"                   # word (default: anything without spaces) | integer
```

Your own in `host.json` have the same shape without `id` (the key is the id): `"snippets": {"find-big": {"title": …, "line": …, "input": [ … ]}}`, `"expansions": {"first": {"say": …, "to": …, "enabled": false}}`. They may use a shell pipeline as a source, `source = { sh = "git branch | cut -c3-" }` (you wrote it); plugins can't.

### Rendering a line (`goatty_plugin::snippets`)

- `parse_line` is a small shell lexer (quotes, `$'…'`, backslashes, operators). `{name}` is a placeholder only when `name` is an input id: outside quotes it must be an input (else "no input …", a typo), **inside quotes it's an error** ("put {x} outside quotes, goatty quotes it"), and braces around anything else are the shell's (`{}`, `{a,b}`, `${HOME}`, `awk '{print $1}'`). `{{`/`}}` are literal braces. Quotes must balance, the line is one line without control characters, every input must be used, and a `multiple` placeholder must be a word of its own.
- `Snippet::render`: the author's text stays as written; each value becomes **one quoted word** (`zsh::word`), a `multiple` value several; a path's `~/` stays expandable at the start of a word (`"$HOME"` inside one); an empty optional whole-word value drops out with its space. Only `choice` and `flag` values, which come from the author's own list, go in as written. Hostile values run back unchanged through real `sh` and `zsh` (`snippets::tests::hostile_values_survive_a_real_shell`).
- `Snippet::preview`: the same with `‹label›` blanks, for the picker and the fill-in header.
- `guess_safety`: `rm`, `dd`, `mkfs`, `git reset --hard`, `find -delete`, `docker rm` … destructive; `mv`, `cp`, `chmod`, `tar -x`/`-c`, package installs, `kill`, `sudo`, a `>` into a file … changes; everything else safe. Only a guess for snippets that don't say; the editor shows it.
- `programs`: the first word of each command (after `sudo`, `env`, `VAR=…`), for "Needs docker".

### Suggestion sources: the host runs its own definition

```
picker field ── Request::Suggest { owner, snippet, input, values, cwd } ──► goattyd / local HostService
                                                                          looks the snippet up in ITS lock-verified manifest
◄── Reply::Suggestions { items: [{value, label, detail}], note, truncated }   or host.json; runs it (no shell, no stdin)
```

- **A client never sends a command**, only ids (`goatty_plugin::api::Request::Suggest`, no protocol bump; a plugin palette command's `[command.input.source]` goes the same way as `Request::CommandSuggest { plugin, command, input, values, cwd }`, `goatty_shell::snippets::suggest_command`, argv only). The host finds the source in the enabled plugin's verified manifest (`plugins::active`, which reads the lock without taking the install lock) or in the user's `host.json`, puts the earlier answers in as single argv entries, and runs it (`goatty_shell::snippets::suggest`).
- A plugin's `run[0]` must be one of its package executables or `requires.commands` (validation), resolved on goatty's managed PATH (import folders, goattyd's PATH, `shell.path`, goatty's `bin/`). `sh` sources are refused for plugins at validation and again at run time.
- The program runs in the pane's directory (on that host; the home directory when it's unknown or gone), in its own process group, with stdin closed, `GIT_TERMINAL_PROMPT=0` and pagers off, killed with its whole group at the timeout (2 s default, 10 s max) and when it finishes. stdout is capped at 64 KiB / 1000 items ("Showing the first ones"), stderr's first line becomes the note when it fails (or the source's `on_fail`).
- A `{other}` in `run` makes the source depend on an earlier input ("Fill in Branch first" until it's answered); validation refuses a later one.
- In the app a field's suggestions load off the UI thread when it's asked (spinner, cached per field and earlier answers, stale results dropped), are filtered with the palette's fuzzy matcher by label, value and detail, and Tab/Enter fill them in. An older goattyd answers `Suggest` with an error and the field says so and takes typed text.

### Expansions in the shell

- `snippets::compile` turns each rule into data: an ERE for the end of the text left of the cursor (`(^|[[:space:]])` + the escaped literal words joined by `[[:space:]]+`, `([0-9]+)` / `([^[:space:]]+)` per placeholder, `$`) and `to` with each placeholder as `\x01N\x01` (its group). `render_table` writes `expansions.zsh` into the generation: arrays of quoted strings, indexed by each rule's last word (and, for a rule ending in a placeholder, the word before it). No author-written regex or code reaches zsh; hostile `say`/`to` text stays data (`snippets::tests::the_table_is_quoted_data`).
- `goatty-shell/zsh/expand.zsh` (static) binds Space, Enter and Backspace in the emacs and viins keymaps, sourced right after `compinit` (so autosuggestions and syntax highlighting wrap the widgets, and your own `.zshrc` or additions can still rebind the keys). Space looks up the last word (one hash lookup), tries those rules only, replaces the matched words and inserts the space. Enter on a line that ends in a rule expands it and stops: the command is shown before it runs; Enter again runs it. Backspace right after an expansion undoes it (`zle undo` to the change before), bringing back what you typed. `zle -M` shows `| first 20 lines → | head -n 20 (Backspace: undo)`. Never inside quotes (quote parity of the text left of the cursor), never right after a paste. With no rules the keys stay unbound.
- The user's enabled expansions come first, then the plugins' in load order; the same words twice: the first wins. `shell.expansions` (default on) leaves the table empty; `goatty-status` shows `expansions ok <n> rules` or `off`.

## Permissions

- `snippets`: the plugin offers snippets or expansions. Nothing runs; text is only inserted at the prompt or rewritten as you type (shown before it runs, Backspace undoes). No reason needed; the install plan lists every line and every rewrite.
- `snippet-sources`: runs the listed programs with the listed arguments when a field opens, without asking again. Reason required, like `binaries`; the plan lists each program with its arguments.

## Measurements

2026-11-16, dev machine (CachyOS, zsh 5.9.2):

| What | Rules | Cost |
|------|-------|------|
| Space on a plain word (`git log … main`) | 0 / 50 / 300 | 40 / 41 / 42 µs |
| Space when the last word is a rule's key but nothing matches | 50 / 300 | 0.19 / 0.49 ms |
| Space that expands | 50 / 300 | 0.28 / 0.29 ms |
| Sourcing `expand.zsh` + `expansions.zsh` at startup | 0 / 12 (Basics) / 300 | 0.32 / 0.56 / 1.67 ms |

Measured by calling `_goatty_exp_apply` 20 000 times in `zsh -f` with tables generated by `goatty-plugin render` (300 rules over 40 last words), and sourcing the files 200 times. A keypress costs less than a millisecond even with hundreds of rules; plain typing pays one function call.

## Later

- Import navi `.cheat` files and tldr pages as packs (`sh` sources for user-imported ones).
- Open the picker from the shell with the half-typed line as the query (a zle widget asking goatty over an OSC request), and an fzf fallback for sessions without the GUI.
- Snippet contexts (`in_project = ".git"`), multi-line and run-immediately snippets.

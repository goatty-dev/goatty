# Coding agents

goatty shows which coding agent runs in a tab, what it's doing, and tells the user when it needs them or finished (#205). It uses what agents already send to terminals; nothing has to be installed or configured on the agent's side.

## What goatty reads

| Sequence | What | Where |
|---|---|---|
| `OSC 7501 ; key=value[:…] ST` | Program Status Protocol: `state` (`idle`, `working`, `blocked`, `done`, `error`, `clear`), `kind` (`permission`, `question`, `auth`), `progress`, `id` (hierarchical records), `app`, `title`/`msg` (base64) | `goatty-term/src/status.rs` |
| `OSC 7501 ; ? ST` | the probe: goatty answers `OSC 7501 ; ?:states=idle,working,blocked,done,error:kinds=permission,question,auth ST` | the daemon's terminal only (`answer_queries`) |
| `OSC 9 ; 4 ; state ; percent` | ConEmu progress bar (parsed by rio-vt) | `listener.rs` → `Store::set_progress` |
| `OSC 9 ; text`, `OSC 777 ; notify ; title ; body` | a program asks for a desktop notification | `listener.rs` → `UiEvent::Notification` |

Agents send the probe followed by `CSI c` and treat OSC 7501 as supported only if its answer arrives before the DA1 reply. `Terminal::feed` applies each OSC 7501 sequence where it ends in the stream (the mark scanner splits the chunk there, as for OSC 133), so the probe's answer is queued on the same reply channel ahead of rio-vt's DA1 answer.

Limits follow the protocol: 4096 bytes per sequence, 16-byte keys, `app` `[A-Za-z0-9_.+-]{1,32}`, `id` up to 8 segments of 32 bytes (128 in all), `title` 256 base64 / 192 decoded bytes, `msg` 2732 / 2048. A report that breaks one, has a duplicate key, invalid base64, invalid UTF-8 or a control character in its text is dropped whole. Bidi controls and zero-width characters are removed from text, which is only ever shown as plain text and never written back to the PTY. Unknown keys and unknown `kind` values are ignored (forward compatible); `kind` and `progress` are ignored where they don't apply. A terminal keeps at most 128 records (the least recently reported goes first).

Lifetime: a report replaces the record its `id` names (`app` is inherited from the nearest ancestor, the root being every record's ancestor); `state=clear` removes a record and its children, or everything without an `id`. A new prompt (`OSC 133 ; A`) drops `idle`, `working` and `blocked` records and the progress bar and keeps `done`/`error`; RIS drops everything; the alternate screen and DECSTR keep them. The reattach, resync and restore snapshots replay the records after the screen rows (whose prompt marks would otherwise drop them) with `goatty=restore`, so a reattached window doesn't announce them again; resync clears the client's first.

## Which agents send what

Checked against the binaries installed on a development machine (Nov 2026):

| Agent | Version | OSC 7501 | Notes |
|---|---|---|---|
| Claude Code | 2.1.326 | yes, after a probe answered before DA1 | `app=claude-code`, `id`, `kind`, `progress`, `title`/`msg`; `state=clear` when it exits; accepts any reply starting with `?` |
| Codex CLI | 0.169.0 | yes | `app=codex`; `state=clear` on exit |
| OpenCode | 2.2.7 | yes | probes; carries the exact capability reply goatty sends |
| Antigravity (`agy`) | — | yes | probes; `AGY_CLI_OSC7501=on\|off`, `AGY_CLI_OSC7501_MESSAGES=brief` |
| Goose | — | not found in the binary | logo from the process name |
| GitHub Copilot CLI | 1.0.112 | not found in the binary | logo from the process name |

Reported by the agents' docs but not verified here: Pi (`PI_PROGRAM_STATUS`), Gemini CLI (OSC 9 / 777 via `general.notificationMethod`, no OSC 7501), Cursor Agent (no OSC 7501).

## Which agent runs in a pane

`agent::Identity::of`: the `app` of the pane's records (works over plain SSH and whatever runtime the agent runs on), else the foreground program (`Agent::from_process`). `goatty_term::process::display_name` names a runtime (`node`, `bun`, `python`, …) that runs a known agent package by the agent (`cursor-agent` runs as `node …/cursor-agent/…/index.js`). After an agent exits, its `done`/`error` record stays, but once the shell is back in the foreground the tab shows the shell's icon again.

Known agents get their logo (`assets/agents/`, from LobeHub Icons, MIT; see its README): in their colors, or tinted with the tab's text color through `svg()` when the mark is black. An unknown `app` gets a generic bot.

Colored logos are rasterized at exactly the size they're drawn in device pixels and cached (`agent::rasterized`, an `img()` source with the window's scale factor). `img()` from an asset path would rasterize an SVG once at twice its intrinsic size and shrink it on the GPU without mipmaps, which leaves stray pixels at tab sizes. Pixel-art marks are drawn at a whole number of pixels per cell, the largest that fits the box (OpenCode, 12 cells: 12 px in a 14 px box; Pi, redrawn on its 4×4 grid: 12 px), or one pixel per cell, overflowing it a little (Claude Code, 16 cells, redrawn on its exact grid: 16 px). `svg()` rasterizes tinted marks at their exact size itself.

## How it shows

- **Tab icon**: the focused pane's agent's logo (with `tabs.icons`).
- **Tab mark** (instead of the activity marker): a spinner while working; a pulsing badge in the warning color while blocked, with an icon for the kind (shield, question, key, hand); a check or a cross for done and failed. Done and failed show until the tab has been on screen in a focused window (`Pane::seen`, the newest record `seq` seen). A split tab shows its most important pane's state (blocked > error > working > done). The tooltip says who, what, and the agent's own title and message.
- **Background tab waiting for the user**: tinted in the warning color (all styles, the rail tile too); a folded host group shows a warning dot.
- **Vertical tabs**: the second line says what the agent does ("Waiting for permission · Bash: cargo test") while it works or waits.
- **Progress**: a thin line along the tab's bottom: the agent's `progress`, else the pane's OSC 9;4 bar (red on error, warning color when paused, a moving segment when indeterminate).
- **Split panes**: a pane whose agent waits gets a warning ring and a pill naming what it waits for.

## Notifications

`notifications.in_app` and `notifications.system` (Settings → Workspace → Notifications) each pick never, when not looking (the pane isn't on screen: another tab, or goatty isn't focused; the default in goatty), when goatty isn't focused (the default on the desktop), or always. `agent_waiting`, `agent_done` and `programs` pick the events.

- A pane notifies on a *change* into blocked, done or error (`agent::is_news`: another record, state or kind than last time), only for reports the program sent (not lifecycle cleanups or snapshot replays), so repeated identical reports and reattaching stay quiet.
- One toast per pane (keyed): blocked stays until dealt with; it goes when the agent moves on (back to work, cleared) or the user looks at the pane. "Show" brings the pane forward.
- Desktop notifications carry the tag `goatty-pane:<window>:<pane>`; clicking one activates that window, tab and pane. At most one per pane every 3 s.
- A program notification (OSC 9 / 777) within 5 s after the same pane's agent notified is the same news and is dropped. ConEmu's numbered OSC 9 subcommands (`9;1;…`) aren't notifications.
- On Linux, GPUI posts through `org.freedesktop.Notifications` and can't retract a notification once shown; it ages out of the shade.

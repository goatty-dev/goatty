# Stage 0 findings: GPUI + rio-vt spike

**Date:** 2026-11-07 · **Machine:** CachyOS, niri (Wayland), RTX 4070 Laptop (Vulkan), 28 threads, 144 Hz panel

**Verdict:** GPUI + rio-vt is fast and correct enough. These are the measurements from the first spike and the reasoning behind the renderer and session model. They are a record of 2026-11-07, not a spec: numbers are from that machine.

---

## Measurements

### VT core (headless, `cargo run --release -p goatty-term --example vtbench`)

| Workload | Throughput |
|---|---|
| Plain ASCII | ~487 MB/s |
| Heavy SGR (256 + truecolor) | ~176 MB/s |
| CJK / emoji / combining | ~84 MB/s |
| Frame snapshot (200×60, under lock) | ~0.24 ms |

rio-vt is not the bottleneck: these numbers are above what a PTY delivers.

`Terminal::feed` also scans for OSC 133 marks, which rio-vt ignores (#122). Medians of 10 interleaved runs on today's hardware, without and with the scan:

| Workload | Without | With |
|---|---|---|
| Plain ASCII | 674 MB/s | 624 MB/s |
| Heavy SGR | 263 MB/s | 255 MB/s |
| CJK / emoji / combining | 131 MB/s | 130 MB/s |

The ASCII gap is mostly run-to-run noise (single runs range from 620 to 710 MB/s either way). The scan only runs `memchr` between escape sequences.

The output viewer reads a `Transcript` (every cell of the scrollback and screen, joined into logical lines and resolved to colors) with the terminal locked, once when it opens and at most every 250 ms while it follows live output. `vtbench`, a full 10,000-row scrollback 200 columns wide with a prompt mark every 64 KiB:

| Workload | Per transcript |
|---|---|
| Plain ASCII | 11.3 ms |
| Heavy SGR | 12.8 ms |

Resolving each cell's colors was most of it (32 ms before runs of the same style shared one lookup). A live view waits at least ten times the last read's cost between reads, so it holds the lock no more than a tenth of the time.

### End-to-end in the window (`scripts/bench.sh`, same workload in every terminal)

| Terminal | `seq 1 3000000` (23 MB) | Colored flood (100 MB) |
|---|---|---|
| **goatty** (glyph-cache renderer) | **1.28 s** | **1.00 s → 100 MB/s** |
| alacritty (user config) | 1.32 s | 1.35 s → 74 MB/s |
| kitty (user config) | 1.66 s | 1.23 s → 82 MB/s |

These are single runs, so expect about ±15% noise. The other terminals run the user's configs. goatty does less work than they do so far (no ligatures, no URL detection), so read this as "same league", not "faster".

### Frame cost during the flood (`GOATTY_STATS=1`)

(Measured on gpui 0.2.2/blade. After the switch to gpui git/wgpu, throughput was re-checked at 113 MB/s; on gpui-pre 0.3.14 (2026-11-26, `--local`, 52 MB colored flood) it was 122–132 MB/s. fps still needs a focused re-run.)

| Renderer | Prepaint | Paint | fps while ingesting 60–90 MB/s |
|---|---|---|---|
| Per-segment line shaping (`GOATTY_RENDER=shaped`) | 10–17 ms | 0.4 ms | 43–51 |
| **Per-cell glyph cache (default)** | **0.3–0.5 ms** | 0.3–1.6 ms | **123–145 (vsync-capped)** |

An idle terminal renders 0 frames. With htop refreshing, the GUI uses about 3% CPU.

### Reconnect

Automated tests (`cargo test -p goatty-daemon`) and a manual GUI check both pass:
- Close the GUI, and the shell keeps running (same PID). Output written while detached appears after reattach. Shell variables survive.
- A full-screen app (htop) is restored exactly on reattach, including alt-screen and DECCKM state.
- The GUI auto-starts `goattyd` (detached with `setsid`) when it isn't running.

---

## Decisions the spike led to

### 1. Renderer: use a per-cell glyph cache, not text shaping
Shaping row text with gpui's `shape_line` works, but every scrolled line misses the cache. That costs 10–17 ms per frame, which is too slow for 120/144 Hz. Instead, resolve each `(char, bold, italic)` to `(font_id, glyph_id)` once (gpui's shaper still handles font fallback) and call `window.paint_glyph` at grid positions. Prepaint then drops to under 0.5 ms.
- **Trade-off:** no programming ligatures in that mode. Keep line shaping as an opt-in "ligatures" setting (already implemented as `GOATTY_RENDER=shaped`).
- **Next:** draw box-drawing and block elements (U+2500–259F) procedurally. With font glyphs they misalign at a 1.25 line height (visible in the smoke test). Every serious terminal does this.

### 2. Session model: authoritative VT in the daemon, mirror VT in the client
The daemon and the GUI **both run rio-vt**.
- The **daemon's** VT is authoritative and is the **only one that answers terminal queries** (DA, DSR, OSC 4/10/11). If both answered, the shell would see every reply twice. This is the `answer_queries` flag.
- On attach, the daemon sends a **VT snapshot**: replayed scrollback, alt-screen switch, screen contents, and input modes (DECCKM, bracketed paste, mouse modes, keypad, title). After that it streams raw bytes, with no gap or overlap: snapshot and subscribe happen under one lock.
- The client feeds bytes into its own VT and renders from it. So `--local` (in-process PTY) and daemon mode share 100% of the UI code.
- **Cost:** bytes get parsed twice. At over 175 MB/s per parse, that doesn't matter.
- **Rejected alternative:** the daemon sends rendered grid diffs (tmux control-mode or zellij style). That means more protocol, client-side scrollback becomes awkward, and nothing is gained locally.

### 3. Survive daemon upgrades (built in #131)

A systemd fd store could hand the PTYs over (`goatty-daemon/src/restore.rs`, [`remote-architecture.md`](remote-architecture.md) §5c): an upgrade doesn't need systemd at all. `goattyd restart` (protocol `Restart`, v10) re-executes the daemon in the same process with the PTYs and the socket inherited, so the shells stay its children and keep their exit codes. The fd store covers what a re-exec can't: crashes and `systemctl --user restart`. The screens carry over as VT bytes (`Terminal::restore_snapshot`: the attach snapshot, plus the primary screen and its scrollback while a full-screen program shows the alternate one), so up to `MAX_REPLAY_HISTORY` (5000) lines of scrollback.

### 4. Have a VT fallback ready: `alacritty_terminal` is the better candidate than `wezterm-term`
Zed's own terminal is GPUI + `alacritty_terminal`, the most battle-tested combination for exactly this stack, and it's published on crates.io. `wezterm-term` is only available as third-party forks on crates.io. rio-vt has been fine so far and its API is very close to alacritty's (Crosswords is derived from it), so swapping behind `goatty-term` would be cheap.

### 5. Threads, not async
One reader thread per session and one per connection is simple and fast. tokio adds nothing until the remote API arrives.

---

## Notes from the spike

- crates.io gpui 0.2.2 (blade renderer) never recreated the Vulkan swapchain after `VK_ERROR_OUT_OF_DATE_KHR`, so the window froze under niri. That is why gpui comes from a recent Zed `main` (`gpui_wgpu` renderer): first a pinned git rev, now Longbridge's `gpui-pre` crates.io snapshots, pinned exactly; bump it on purpose (see `CLAUDE.md`).
- rio-vt's `contents_formatted()` covers only the visible screen and drops combining marks, so goatty's snapshot adds scrollback and modes. The hidden primary screen under the alt screen can't be restored (rio keeps it private).

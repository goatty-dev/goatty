# goatty

A fast (GPU-rendered via GPUI), batteries-included, opinionated terminal: managed zsh with curated defaults, a GUI for every setting, declarative plugins, and a GUI that is always a client of a `goattyd` (local, or remote over SSH). goatty never takes over the host.

Read the relevant doc before larger work: `docs/architecture.md` (crate rules and **where new work goes**), `docs/goatty-project-plan.md` (stages, roadmap), `docs/remote-architecture.md`, `docs/tailnet.md`, `docs/managed-zsh.md`, `docs/profiles.md`, `docs/plugins.md`, `docs/snippets.md`, `docs/coding-agents.md`, `docs/stage0-findings.md`, `docs/releasing.md`. Per-crate conventions load from `.claude/rules/` when you work in that crate. Progress lives in GitHub issues/milestones and `git log`, not here.

## Workflow

- Repo https://github.com/goatty-dev/goatty, default branch `main`. Do feature work on branches and open PRs. CI (`.github/workflows/ci.yml`) must be green. Releases: `just bump X.Y.Z` opens a version-bump PR, and merging it tags `vX.Y.Z` and publishes packages (`docs/releasing.md`). The version lives only in the root `Cargo.toml`.
- Keep this file and `.claude/rules/*` current when conventions change. Don't add status or progress notes.
- Back performance claims with measurements and record the numbers in the relevant doc: `cargo run --release -p goatty-term --example vtbench` (VT parsing), `cargo run --release -p goatty-daemon --example netbench [proxy | slow 2m | ssh HOST]` (latency, flood), `cargo run --release -p goatty-daemon --example snapbench` (snapshot/history size, compression, history insert), `FLOOD=big.txt scripts/bench.sh goatty ./target/release/goatty --local` (GUI throughput).
- The test build compiles gpui and is slow the first time, so run long cargo commands in the background. `scripts/ci-deps.sh` installs the GPUI system libraries on Debian/Ubuntu.
Dev environment: `nix develop` (or direnv, `.envrc` is `use flake`) gives the Rust toolchain, `just` and the GPUI system libraries from `flake.nix`; without nix, install them with `scripts/ci-deps.sh` and rustup. The `justfile` wraps the commands below (`just` lists the recipes; `just ci` runs what CI checks). Keep `flake.nix` in sync with `scripts/ci-deps.sh`.

## GitHub issues

The issue list is the backlog, so only file issues that are worth reading.

- File an issue only for something important that would otherwise get lost: a bug you found, a real product gap or follow-up, or tech debt that will cause problems. Search first (`gh issue list --search …`). If an issue already exists, comment on it.
- Don't file issues for:
  - manual checks for the user ("smoke-test X interactively", "try it on a real host", "measure Y once"). List these in your reply under "Please verify".
  - speculative ideas or nice-to-haves nobody asked for
  - cosmetic nits
  - anything you can fix in the current change
  - duplicates
- Aim for zero to two new issues per task. If you think more are needed, list them in your reply and let the user choose which to file.
- Don't leave a TODO only in code or chat. Fix it, file it if it meets the bar above, or drop it.
- Use `gh issue create --title … --body-file … --label … --milestone …` with existing labels (`gh label list`) and milestones (`Stage 1`, `Stage 3`–`5`, `R1`, `R2 (later)`, `R3 (later)`, `later`). Reference issues as `#N`. Close them with `Fixes #N` or `gh issue close N --comment "…"`.
- End your reply by listing the issues you created or closed, or "none".

## Safety

- Tests that start a daemon or run the CLIs use their own `GOATTY_SOCKET` and a sandboxed `HOME`. Tests that run the managed zsh use sandboxed `$HOME`/XDG dirs (`Dirs::under`). **Never touch the user's running daemon** at `$XDG_RUNTIME_DIR/goatty/goattyd.sock`: it holds their live sessions.
- After a protocol change, a running goattyd outside the new client's version range can't talk to it. Tell the user instead of killing it.
- goatty writes under its own XDG dirs. It never edits the contents of the user's dotfiles. The one exception is opt-in: with the user's consent it adds one guarded line to `.zshenv` (shown to them first, and removed when they turn the setting off) so every interactive zsh uses goatty's managed config (`goatty_shell::everywhere`). It never changes the login shell or `/etc/shells`, never touches the global PATH, and never uses sudo without asking. OSC 52 clipboard reads are denied. The daemon socket is 0600 in a 0700 dir and never exposed on the network.
- Never put a registry signing key in a repo or CI.
- Never change the user's Tailscale (Serve routes, operator, login) from tests or by hand while working: the tailnet tests use the fake `tailscale` in `crates/goatty-net/tests/`.

## Conventions for all crates

- **The daemon owns sessions and the GUI is always a client.** The daemon's VT is authoritative and the only one that answers terminal queries. Clients mirror it by feeding the same bytes into their own `Terminal` (`answer_queries = false`).
- **Never block the UI thread on I/O.** Writes go through the client's writer thread. The UI is woken through the coalescing `Waker` (one notify per frame).
- **Use threads, not async**, until a QUIC transport needs tokio.
- **Settings go through `goatty-config`.** Don't read `GOATTY_*` env vars anywhere else. The exceptions are `GOATTY_SOCKET` in `goatty_proto::socket_path` and `GOATTY_ASKPASS` in `goatty_client::askpass`.
- Every `unsafe` block needs a `// SAFETY:` comment. Lints live in the root `[workspace.lints]`, and every crate has `[lints] workspace = true`.
- rio-vt is pinned to an exact version. gpui comes from Zed's git repo at a pinned rev: to bump it, change `rev` on purpose, then run `cargo update -p gpui`.

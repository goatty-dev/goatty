# Workspaces

A workspace is a named set of tabs (each with its split tree) that a goattyd keeps, shared by every client window that shows it (#215). It's goatty's version of a tmux session. tmux's "window" is a goatty **tab**, and goatty uses "workspace" so the name doesn't clash with OS windows or with tmux's windows.

```
goattyd
 └─ workspace   (named, lives on the daemon, joinable by any client)
     └─ tab
         └─ pane (split tree)
```

Not to be confused with `crates/goatty-app/src/workspace/`, the GUI window's root view (`Workspace`). That module shows workspaces: `sync.rs` is the window's side of them, and `switcher.rs` is their UI.

## The model

- Every goattyd has at least one workspace. A fresh daemon makes one named **Main** the first time a client asks. People who never make a second workspace never see any workspace UI.
- Extra workspaces are only made on purpose ("New workspace"). Nothing else makes one: not `--new`, not a second window, not a reconnect.
- A workspace ends with its last tab. If the window has other workspaces on that host, it shows the most recently active one instead. If it was the host's only workspace, closing its last tab works as before (the window closes with its last tab). A workspace whose tabs are all detached keeps existing, and so does one no window shows while it still has running sessions. One whose sessions all ended while no window showed it is dropped.
- **Shared** between every window that shows a workspace: its tabs (order, names, which session is where), each tab's split tree, and its detached tabs.
- **Each window's own**, never shared: the active tab, the focused pane, zoom, scroll position, the window's size, which workspace it shows on each host, and activity marks.
- A window can hold tabs from several hosts. It shows **one workspace per host**. The switcher lists the workspaces of the active tab's host, and switching swaps only that host's tabs.
- **Which workspace a window joins:** the one it last showed on that host (from `windows.json`) if it still exists, else the host's most recently active one. A first-time client lands in Main. Connecting never asks which one.

## Lifecycle and windows

| Action | What happens |
|---|---|
| New window (`new_window`, Ctrl+Shift+N) | Another window on the **same workspace** (the active tab's host), at the same tab. It doesn't make a workspace. |
| `--new`, `workspace.on_launch = new` | A window on the most recently active workspace, with a fresh tab added to it. |
| New workspace | An empty workspace with one fresh shell in the focused pane's directory. This window switches to it, and other windows stay where they are. It's named "Workspace 2", "Workspace 3", …, and the name is editable right away in the switcher. |
| Open in new window (switcher menu) | That workspace in a new window, so two workspaces are on screen at once. |
| Close a workspace | An empty or detached one goes without asking. One with running shells asks once: **Move N tabs to *other*** (the default) or **Close them (ends N shells)**. Nothing is killed without that second choice. The host's only workspace can't be closed. Closing never ends sessions on the daemon side: the client moves or ends them first, then `CloseWorkspace` forgets the workspace. |
| Detach some of a host's tabs | They become **detached tabs** of the workspace (every window showing it loses them) and their shells keep running. Ctrl+Shift+R, the "Undo" toast and the sessions picker bring them back (`AttachMode::Undetach`). Tabs detached together come back together. |
| Detach all of a host's tabs (detach host, last tab) | The window stops showing that workspace (`LeaveWorkspace`). The workspace stays on the host as it is, for any window to show. |
| Close a window | Like detaching all of its hosts: its workspaces stay. |

## UI

- **One workspace:** no chip. "New workspace" and "Switch workspace…" are in the command palette and in the new-tab menu's "Workspace" submenu. There's no tutorial popup.
- **Two or more:** a chip at the left of the tab bar (at the top of the sidebar with vertical tabs) names the active tab's host's workspace. With two workspaces the tab bar shows even with one tab. The chip carries a dot when something happened in a workspace this window keeps in the background (the tabs' bell and activity marks rolled up, #122). Clicking it opens the switcher, and double-clicking renames.
- **The switcher** lists the host's workspaces with their tab counts, the rolled-up activity mark, a window icon when another window shows one, and a "New workspace" row. Arrows and Enter switch, F2 (or a double-click on the current workspace's row or on the chip) renames in place, Delete closes, and right-click opens Switch / Rename / Open in new window / Close.
- **Keys:** `next_workspace` / `previous_workspace` (Ctrl+Alt+PageDown / PageUp), `switch_workspace` (Ctrl+Alt+W), and `new_workspace`, `rename_workspace`, `close_workspace` (unbound). All of them can be rebound on the Shortcuts page ("Workspaces" group). Next/previous pass the key through to the shell when the host has only one workspace.
- **Settings → Workspace → When goatty starts** (`workspace.on_launch`): "Restore every window" (each window joins its last workspaces on every host), "Restore this computer's tabs" (this computer's most recently active workspace), "Start a new session" (a new window on the most recently active workspace, with a fresh tab).
- **Sessions picker** (`list_sessions`): each workspace on the host (show it), its detached tabs ("Reattach N tabs detached from …"), then a new session, every detached session that no workspace shows, and the sessions one by one.
- **CLI:** `goattyd workspaces` lists the daemon's workspaces with their tabs, sessions and the windows showing them.

## Sync

The layout is opaque to the daemon: client JSON (`workspace/saved.rs`, format version 2) with a stable random id per tab. The daemon keeps a workspace's name, JSON, sessions, tab count, a **revision**, an activity order, and the windows (`(connection, view)`) that show it (`goatty-daemon/src/workspaces.rs`).

- **Optimistic, revisioned saves.** `SaveWorkspace` carries the revision the edit was made on. If the daemon has a newer one, the reply is `Conflict` with the newest layout. The window merges its own edit into it and saves again. Edits happen at human speed, so this is rare.
- **Merging** (`saved::merge`) works per tab id, from three layouts: the base the window last heard, its own, and the daemon's. A change on one side wins over no change on the other. A tab closed on either side is gone. Tabs added on either side are kept next to the tab before them. Mine's order wins if mine reordered. When both sides changed a tab's splits, mine wins, and the panes theirs added or closed are added or removed in it. Each session ends up in one tab. Float noise in split sizes doesn't count as a change (`same_tree`), so saves don't ping-pong.
- **Live sync.** After every accepted save, the daemon sends `LayoutChanged { by, layout }` once to every connection with another window on that workspace. That includes the saver's own connection when another window of the same process shows it. `by` keeps it from going back to the saver. The window merges it the same way and applies it with `Workspace::reconcile`: panes are kept by session id (also ones a parked workspace of this window still holds), new sessions are attached off the UI thread (no save while they attach, so their tabs don't look closed; after a dropped link or a timeout they're attached again rather than taken for closed), and removed ones are let go. Its active tab, focus, zoom and activity marks stay. A change waits while a divider is being dragged, and split sizes are saved when the drag ends.
- **List changes** (`WorkspacesChanged`) go to every connection that listed or joined workspaces whenever one is made, renamed, closed, ended, or shown or left by a window. Every list carries a version (`seq`, bumped by every change to it), and a list older than one already heard is ignored: it was sent before this window joined a workspace made just now. When the workspace a window shows is gone from the list (closed elsewhere), the window switches to the host's most recently active other workspace, or drops that host when none is left.
- **Reconnect.** `Connection` remembers each view's workspace and joins it again after the link comes back (`WorkspaceEvent::Rejoined`). The window then merges and saves what it changed while offline.
- **Background workspaces stay subscribed.** Switching away parks the left workspace's tabs (`sync::Parked`): their sessions stay attached and keep feeding their mirror terminals, without rendering, so bells, finished commands and output still mark the switcher. Coming back is a join round trip, with no re-attach. A parked workspace never takes a session's size, because its panes aren't laid out. Size control is taken as usual once its tabs are visible again.
- **Two windows of one goatty process on one workspace** share each session's client-side `Session` (one subscription per session and connection): each view has its own wake-up, UI-event cursor and CWD-change count, and the view that last had focus sets the size (`Session::may_resize`; a parked view gives it up). A session comes with one view claimed for whoever attached or looked it up (under the lock that guards detaching), a pane takes that claim over, and the session is detached when its last view lets go (`Session::release_view`).

## Compatibility

Workspaces replaced the exclusive window layouts of protocol v8/v16. This was a breaking change on purpose (nobody had installed goatty yet):

- `MIN_PROTOCOL_VERSION` is 17. The v8/v16 layout messages are retired in place (`Retired…` variants keep their slots), so `Hello`, `ListSessions`, `Restart` and `Shutdown` keep their index. A new client can therefore still restart or stop a daemon down to v10 (`ADMIN_MIN_PROTOCOL_VERSION`, `Control::connect_admin`). The connect screen restarts an older local daemon right away, because its sessions carry over, and offers "Update and restart" for a remote one, without the "ends the sessions" warning.
- `windows.json` is version 2. A version-1 file is dropped.
- The restore state (`<socket>.restore/state.json`) carries workspaces in a new `workspaces` field. It writes an empty `layouts` array so that older builds can still read it, and it drops what an older build wrote there.

## Cost (`netbench`, i7-14700HX)

Measured with `cargo run --release -p goatty-daemon --example netbench [proxy | delay 50]`. The `SYNC_TABS` and `BG_PANES` env vars set the size of each phase.

| | local socket | `goattyd proxy` | 50 ms RTT |
|---|---|---|---|
| Save, 20 tabs (1.1 KB): round trip p50 / p90 | 0.050 / 0.231 ms | 0.040 / 0.051 ms | 51.0 / 51.3 ms |
| …another window has it, p50 / p90 | 0.043 / 0.112 ms | 0.037 / 0.048 ms | 50.9 / 51.2 ms |
| Save, 200 tabs (10.9 KB): round trip / other window, p50 | 0.082 / 0.081 ms | | |
| 8 background panes printing a line every 100 ms | 4.7 KB/s, 0.3 % of a core | 4.7 KB/s, 0.3 % | 4.7 KB/s, 0.2 % |
| 32 such panes | 18.8 KB/s, 1.2 % of a core | | |

A save costs about one round trip, and the other window hears it in the same round trip, since the broadcast leaves before the reply. A busy background workspace costs what its mirror terminals cost to parse. A flooding one is bounded like any other subscription by flow control (§4 of `remote-architecture.md`: the contended-echo numbers there are the worst case).

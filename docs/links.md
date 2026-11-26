# `goatty://` links

Deep links for documentation and the plugin registry website to open a specific settings page, setting row, or plugin review in a running goatty window (#305).

## Format

```
goatty://<target>[/<path>][?<query>]
```

- The scheme is fixed and lowercase (`goatty://`).
- Targets, paths, page aliases, query parameter names, setting keys, plugin IDs, and registry names are matched case-insensitively.
- The parser (`crates/goatty-app/src/links.rs`) is strict and caps links at **2 KiB** (2048 bytes). Userinfo (`@`), ports (`:`), fragments (`#`), control characters, whitespace, path traversal (`..`), and malformed percent-encoding are rejected.

| Link | Opens |
|---|---|
| `goatty://settings` | The settings window where it was |
| `goatty://settings/<page>` | That page (`Page::from_name`, including aliases such as `goatty://settings/ssh` or `goatty://settings/keybindings`) |
| `goatty://settings/<page>?setting=<dotted.key>` | That page, scrolled to and highlighting the row that edits `<dotted.key>` (e.g. `goatty://settings/terminal?setting=links.open_on`) |
| `goatty://plugins` | Settings → Plugins → Browse |
| `goatty://plugins/<id>[?version=<v>&registry=<name>]` | The plugin's install review, or its detail page when already installed |

### Unknown targets, pages, keys, and plugins

Syntactically valid links that name a target, page, setting key, or plugin this build doesn't recognize never fail silently: goatty lands on the closest page it can (the settings window, the requested page without a highlighted row, or Settings → Plugins → Browse) and shows a toast:

> `goatty doesn't know this link. Maybe it needs an update.`

Newer docs linking an older goatty build therefore still land somewhere useful.

## Security: a link only navigates

Any web page can trigger a `goatty://` link (browsers prompt before handing off to an external app, but users click through). Therefore:

- **Navigate only.** A link never changes a setting, runs or types a command, connects to a host, switches hosts, or adds a plugin registry.
- **Explicit approval for plugins.** `goatty://plugins/<id>` only opens the existing install review with permissions and shell code shown, exactly like clicking from Browse. Installing still requires the user's explicit confirmation.
- **Configured registries only.** Plugins resolve only against registries the host already has. An unknown `registry=` shows a toast and is never added (adding a registry requires its signing key on purpose).
- **No host switching.** Plugin links always apply to the host the settings window follows (the active tab's host), which the review names.
- **No interpolation.** Values from links (setting keys, plugin IDs, versions, registry names) are matched against the settings index and the host's plugin catalog. They are never used in filesystem paths, commands, or shell code.

## Delivery and handoff

### `goatty open <URL>`

Every platform's handler can invoke `goatty open <URL>`, and a bare `goatty goatty://…` argument is accepted as shorthand (used by `Exec=goatty %U` in the Linux `.desktop` entry).

1. Parse the URL strictly with `links::parse`. Invalid input prints the error and exits 1.
2. If a goatty GUI process is running for the current user, send the URL over its per-process app socket and exit 0.
3. Otherwise launch the GUI normally (reattaching or joining the workspace as plain `goatty` does) and apply the link once the initial window's host is ready.

### Per-process app socket (`app-<pid>.sock`) and `app-active`

Each goatty GUI process is its own process with its own windows, so link handoff uses a GUI-to-GUI local endpoint (separate from `goattyd`, so `goattyd` and `goatty-proto` don't change):

- Each GUI process binds `<runtime_dir>/app-<pid>.sock` (on Unix: mode `0600` in the `0700` runtime directory; on Windows: a Named Pipe via `goatty_client::ipc` with a marker file for discovery).
- The wire protocol is a single line of JSON: `{"open":"<url>"}\n`, answered with `{"ok":true}\n` or `{"ok":false,"error":"..."}\n`.
- Whenever a terminal or settings window gains focus, the process records its PID in `<runtime_dir>/app-active` off the UI thread.
- `goatty open` tries the `app-active` PID first, then any other live `app-*.sock` (newest first), removing stale sockets along the way. Peer ownership is verified via `LocalStream::peer_owner_pid`.
- On the receiving process, a listener thread parses incoming links into `LinkInbox` and wakes the UI thread through the coalescing `Waker` so the UI thread never blocks on I/O.
- On **macOS**, `gpui::Application::on_open_urls` feeds the exact same `LinkInbox`, and the OS handles single-instance URL delivery.

### Scheme registration

- **Linux:** `packaging/linux/dev.goatty.goatty.desktop` (and `scripts/install.sh`) sets `Exec=goatty %U` and `MimeType=x-scheme-handler/goatty;`.
- **macOS:** `CFBundleURLTypes` with the `goatty` scheme belongs in `Info.plist` once a macOS app bundle is packaged.
- **Windows:** `HKCU\Software\Classes\goatty` (`URL Protocol`, `shell\open\command` = `"…\goatty.exe" open "%1"`) belongs in the installer once one exists.

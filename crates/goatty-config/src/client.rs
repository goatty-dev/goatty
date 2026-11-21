//! Client settings (`settings.json` on the machine running the window):
//! how goatty looks and behaves locally, whichever host it's connected to.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::keys;
use crate::store::{Document, Issue, Migration};
use crate::theme::{DEFAULT_LIGHT_THEME, DEFAULT_THEME, ThemeSettings};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ClientSettings {
    pub font: FontSettings,
    pub terminal: TerminalSettings,
    pub theme: ThemeSettings,
    /// Keystroke → action name, or `null` to unbind a default
    /// (see [`keys::effective_bindings`]).
    pub keybindings: BTreeMap<String, Option<String>>,
    pub workspace: WorkspaceSettings,
    pub window: WindowSettings,
    pub tabs: TabSettings,
    pub panes: PaneSettings,
    pub links: LinkSettings,
    pub paste: PasteSettings,
    pub selection: SelectionSettings,
    pub mouse: MouseSettings,
    pub keyboard: KeyboardSettings,
    pub ssh: SshSettings,
    pub tailnet: TailnetSettings,
    pub plugins: PluginNotifySettings,
    pub notifications: NotificationSettings,
    pub debug: DebugSettings,
}

/// When goatty tells about something that happened in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyWhen {
    Never,
    /// Only while goatty's window isn't the focused one.
    Unfocused,
    /// While the pane isn't on screen: its tab is in the background, or the
    /// window isn't focused.
    Background,
    Always,
}

/// Notifications about panes (#205): coding agents that wait for the user
/// or finished (OSC 7501), and programs asking for one (OSC 9, OSC 777).
/// In-app notifications and the desktop's are set separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    /// goatty's own notifications, in the window.
    pub in_app: NotifyWhen,
    /// The desktop's notifications (the system's notification center).
    pub system: NotifyWhen,
    /// When a coding agent waits for you: a permission, an answer, a login.
    pub agent_waiting: bool,
    /// When a coding agent finishes its task or fails.
    pub agent_done: bool,
    /// When a program asks for a notification (OSC 9 / OSC 777).
    pub programs: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self { in_app: NotifyWhen::Background, system: NotifyWhen::Unfocused, agent_waiting: true, agent_done: true, programs: true }
    }
}

/// What the app tells about plugins (#95).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginNotifySettings {
    /// Show a notification when a host has newer versions of installed
    /// plugins. Updates still need a review; nothing installs by itself.
    pub notify_updates: bool,
}

impl Default for PluginNotifySettings {
    fn default() -> Self {
        Self { notify_updates: true }
    }
}

/// Other computers on the user's tailnet that run goatty (#15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TailnetSettings {
    /// List the tailnet's computers that run goatty (and the paired ones)
    /// in the host switcher. Reads `tailscale status` and probes goatty's
    /// port on online peers when the switcher opens.
    pub discover: bool,
    /// Also other people's and tagged devices, not only the user's own.
    pub all_devices: bool,
}

impl Default for TailnetSettings {
    fn default() -> Self {
        Self { discover: true, all_devices: false }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontSettings {
    /// Explicit family. `None`: the first installed entry of `fallbacks`.
    pub family: Option<String>,
    /// Preferred monospace families, first installed one wins.
    pub fallbacks: Vec<String>,
    /// Explicit family for goatty's own windows (tabs, settings, command
    /// palette, dialogs). `None`: the first installed entry of `ui_fallbacks`.
    pub ui_family: Option<String>,
    /// Preferred proportional families for goatty's own windows, first
    /// installed one wins.
    pub ui_fallbacks: Vec<String>,
    /// Size in logical pixels.
    pub size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
}

impl Default for FontSettings {
    fn default() -> Self {
        let fallbacks = [
            "JetBrainsMono Nerd Font",
            "JetBrains Mono",
            "FiraCode Nerd Font Mono",
            "FiraCode Nerd Font",
            "CommitMono Nerd Font Mono",
            "Fira Code",
            "DejaVu Sans Mono",
            "Adwaita Mono",
            "Noto Sans Mono",
            "Liberation Mono",
        ];
        Self {
            family: None,
            fallbacks: fallbacks.map(String::from).to_vec(),
            ui_family: None,
            ui_fallbacks: Self::DEFAULT_UI_FALLBACKS.iter().map(|s| (*s).to_string()).collect(),
            size: 14.0,
            line_height: 1.25,
        }
    }
}

impl FontSettings {
    pub const MIN_SIZE: f32 = 6.0;
    pub const MAX_SIZE: f32 = 48.0;
    pub const MIN_LINE_HEIGHT: f32 = 1.0;
    pub const MAX_LINE_HEIGHT: f32 = 2.0;

    /// The font goatty ships and registers at start (`goatty-app`'s
    /// `assets/fonts`): Nerd Font icons work without installing anything.
    pub const BUNDLED: &str = "JetBrainsMono Nerd Font";

    /// Default proportional families for goatty's own windows.
    pub const DEFAULT_UI_FALLBACKS: &'static [&'static str] =
        &["Inter", "Cantarell", "Noto Sans", "Ubuntu", "Open Sans", "DejaVu Sans", "Liberation Sans"];

    /// The family to use, given the fonts installed on the system (without
    /// the bundled one):
    /// 1. the explicit `family`;
    /// 2. the first of `fallbacks` that is an installed Nerd Font, so the
    ///    prompt's icons draw in the user's own Nerd Font;
    /// 3. the first of `fallbacks` that is installed, counting
    ///    [`Self::BUNDLED`] as installed;
    /// 4. [`Self::BUNDLED`].
    pub fn resolve_family(&self, system: &[String]) -> String {
        let installed = |f: &&String| system.iter().any(|i| i == *f);
        self.family
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(String::from)
            .or_else(|| self.fallbacks.iter().filter(installed).find(|f| is_nerd_font(f)).cloned())
            .or_else(|| self.fallbacks.iter().find(|f| installed(f) || *f == Self::BUNDLED).cloned())
            .unwrap_or_else(|| Self::BUNDLED.into())
    }

    /// The proportional family for goatty's own windows, given the
    /// installed fonts:
    /// 1. the explicit `ui_family`;
    /// 2. the first of `ui_fallbacks` that is installed;
    /// 3. `"sans-serif"`.
    pub fn resolve_ui_family(&self, fonts: &[String]) -> String {
        let installed = |f: &&String| fonts.iter().any(|i| i == *f);
        self.ui_family
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(String::from)
            .or_else(|| self.ui_fallbacks.iter().find(installed).cloned())
            .unwrap_or_else(|| "sans-serif".into())
    }
}

/// Whether a family is patched with the Nerd Font symbols, by its name
/// ("JetBrainsMono Nerd Font", "FiraCode Nerd Font Mono", "Hack NF").
fn is_nerd_font(family: &str) -> bool {
    family.contains("Nerd Font") || family.ends_with(" NF") || family.ends_with(" NFM") || family.ends_with(" NFP")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Renderer {
    /// Per-cell glyph cache: fastest, no ligatures.
    #[default]
    Glyphs,
    /// Per-segment line shaping: ligatures, slower when many lines change.
    Shaped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    #[default]
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    pub renderer: Renderer,
    /// Space around the grid in logical pixels, per side.
    pub padding: Padding,
    /// Cursor shape when the application doesn't ask for one.
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    /// Opacity of the terminal background (0.1–1.0). Text and colored
    /// cell backgrounds stay opaque. Below 1 the window is transparent,
    /// which needs a compositor.
    pub opacity: f32,
    /// Blur what shows through a transparent background, where the
    /// desktop supports it (KDE, macOS, Windows).
    pub blur: bool,
}

impl TerminalSettings {
    pub const MAX_PADDING: f32 = 48.0;
    pub const MIN_OPACITY: f32 = 0.1;

    /// Whether the window needs to be transparent.
    pub fn is_transparent(&self) -> bool {
        self.opacity < 1.0
    }
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            renderer: Renderer::Glyphs,
            padding: Padding::all(6.0),
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            opacity: 1.0,
            blur: false,
        }
    }
}

/// Space around the terminal grid, per side. Written as one number when
/// every side is the same (`"padding": 6`), else as an object
/// (`{"top": 4, "right": 10, "bottom": 4, "left": 10}`; missing sides are 6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Padding {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Padding {
    pub const DEFAULT: f32 = 6.0;

    pub const fn all(v: f32) -> Self {
        Self { top: v, right: v, bottom: v, left: v }
    }

    pub fn is_uniform(&self) -> bool {
        self.top == self.right && self.right == self.bottom && self.bottom == self.left
    }

    /// The sides in CSS order (top, right, bottom, left), for editing one by index.
    pub fn sides_mut(&mut self) -> [&mut f32; 4] {
        [&mut self.top, &mut self.right, &mut self.bottom, &mut self.left]
    }

    /// One side by its index in [`Padding::sides`].
    pub fn side_mut(&mut self, i: usize) -> &mut f32 {
        match i {
            0 => &mut self.top,
            1 => &mut self.right,
            2 => &mut self.bottom,
            _ => &mut self.left,
        }
    }

    pub fn sides(&self) -> [f32; 4] {
        [self.top, self.right, self.bottom, self.left]
    }
}

impl Default for Padding {
    fn default() -> Self {
        Self::all(Self::DEFAULT)
    }
}

impl Serialize for Padding {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.is_uniform() {
            return s.serialize_f32(self.top);
        }
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("Padding", 4)?;
        m.serialize_field("top", &self.top)?;
        m.serialize_field("right", &self.right)?;
        m.serialize_field("bottom", &self.bottom)?;
        m.serialize_field("left", &self.left)?;
        m.end()
    }
}

impl<'de> Deserialize<'de> for Padding {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        fn side() -> f32 {
            Padding::DEFAULT
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Sides {
            #[serde(default = "side")]
            top: f32,
            #[serde(default = "side")]
            right: f32,
            #[serde(default = "side")]
            bottom: f32,
            #[serde(default = "side")]
            left: f32,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            All(f32),
            Sides(Sides),
        }
        match Either::deserialize(d).map_err(|_| serde::de::Error::custom("expected a number or {top, right, bottom, left}"))? {
            Either::All(v) => Ok(Padding::all(v)),
            Either::Sides(s) => Ok(Padding { top: s.top, right: s.right, bottom: s.bottom, left: s.left }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnLaunch {
    /// Reopen every window that was open when goatty quit, with its tabs
    /// and splits on every host: this computer and the SSH and tailnet
    /// hosts it had tabs on (#252). Without such a window, restore this
    /// computer's last window (else reattach the newest detached session,
    /// else start one).
    #[default]
    Reattach,
    /// Restore only this computer's last window (as `reattach` did before
    /// #252): other hosts' tabs stay detached until you connect there.
    ReattachLocal,
    /// Always start a new session.
    New,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkspaceSettings {
    pub on_launch: OnLaunch,
}

/// The terminal window itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WindowSettings {
    pub title_bar: TitleBar,
}

/// Who draws the window's title bar (#120).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TitleBar {
    /// The desktop's, above the tabs, where it draws one. Tiling
    /// compositors (niri, Hyprland, Sway) draw none and goatty adds none;
    /// where the desktop leaves it to apps (GNOME on Wayland), goatty draws
    /// its own anyway.
    #[default]
    System,
    /// goatty's: the tabs share the bar with the window buttons, and the
    /// desktop draws none (where it lets apps do that).
    Goatty,
}

/// How the tab bar looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabStyle {
    /// Browser-like tabs with rounded tops, icons and close buttons.
    #[default]
    Modern,
    /// One line of small text, like kitty's tab bar.
    Compact,
    /// A sidebar on the left with a tab per row, like Arc or Zen; it can
    /// fold into a rail of icons (`sidebar_collapsed`).
    Vertical,
}

/// When the tab bar shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabBarShow {
    #[default]
    Always,
    /// Only with two tabs or more.
    Multiple,
}

/// What marks a background tab as having new activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabActivity {
    /// A command finished (OSC 133, from shell integration). The mark
    /// shows whether it failed.
    #[default]
    Finished,
    /// Any new output.
    Output,
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabSettings {
    pub style: TabStyle,
    pub show: TabBarShow,
    /// Icon of the running program (Nerd Font glyphs, when a Nerd Font is
    /// installed).
    pub icons: bool,
    /// Mark a background tab whose program rang the bell.
    pub mark_bell: bool,
    /// When to mark a background tab as having new activity.
    pub mark_activity: TabActivity,
    /// Turning the mouse wheel over the tabs switches to the next or
    /// previous tab (instead of scrolling the tabs).
    pub scroll_to_switch: bool,
    /// Vertical tabs: the sidebar's width in logical pixels.
    pub sidebar_width: f32,
    /// Vertical tabs: the sidebar is folded into a rail of icons.
    pub sidebar_collapsed: bool,
}

impl TabSettings {
    pub const MIN_SIDEBAR_WIDTH: f32 = 140.0;
    pub const MAX_SIDEBAR_WIDTH: f32 = 480.0;
    pub const DEFAULT_SIDEBAR_WIDTH: f32 = 220.0;
}

impl Default for TabSettings {
    fn default() -> Self {
        Self {
            style: TabStyle::Modern,
            show: TabBarShow::Always,
            icons: true,
            mark_bell: true,
            mark_activity: TabActivity::Finished,
            scroll_to_switch: false,
            sidebar_width: Self::DEFAULT_SIDEBAR_WIDTH,
            sidebar_collapsed: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneSettings {
    /// How much panes without focus fade (0 = not at all), when a tab is split.
    pub dim_inactive: f32,
    /// Ask before closing a pane or tab whose shell is running a program
    /// (a build, an editor, an SSH session).
    pub confirm_close: bool,
}

impl PaneSettings {
    pub const MAX_DIM: f32 = 0.6;
}

impl Default for PaneSettings {
    fn default() -> Self {
        Self { dim_inactive: 0.2, confirm_close: true }
    }
}

/// How links in the terminal (URLs, file paths, OSC 8 hyperlinks) open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LinkClick {
    /// Ctrl+click (Cmd+click on macOS); a plain click still selects.
    #[default]
    CtrlClick,
    /// A click without dragging opens it.
    Click,
    /// Links aren't detected.
    Off,
}

/// How links to files and directories on remote hosts open (#110).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFileOpen {
    /// Run `remote_file_opener` when set for an SSH host, otherwise fetch a
    /// read-only local copy over the session link.
    #[default]
    Auto,
    /// Fetch a read-only local copy over the session link.
    Fetch,
    /// Run `remote_file_opener` (falls back to fetching if unset or no SSH target).
    RemoteOpener,
    /// Copy the remote path to the clipboard.
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkSettings {
    pub open_on: LinkClick,
    /// Command that opens files, e.g. `code --goto {path}:{line}:{column}`.
    /// `{path}`, `{line}` and `{column}` are filled in per argument (no
    /// shell involved); without `{path}` the path is appended. `None`:
    /// the desktop's default application.
    pub file_opener: Option<String>,
    /// How links to files and directories on remote hosts open (#110).
    pub remote_files: RemoteFileOpen,
    /// Command template for opening remote files directly in a local editor,
    /// e.g. `code --remote ssh-remote+{host} --goto {path}:{line}:{column}`.
    /// Tokens: `{host}`, `{user}`, `{hostname}`, `{path}`, `{line}`, `{column}`.
    pub remote_file_opener: Option<String>,
    /// Maximum remote file size (in MiB) fetched automatically (`1`–`60`).
    pub remote_max_size_mb: u32,
}

impl Default for LinkSettings {
    fn default() -> Self {
        Self {
            open_on: LinkClick::default(),
            file_opener: None,
            remote_files: RemoteFileOpen::default(),
            remote_file_opener: None,
            remote_max_size_mb: Self::DEFAULT_REMOTE_MAX_SIZE_MB,
        }
    }
}

impl LinkSettings {
    pub const DEFAULT_REMOTE_MAX_SIZE_MB: u32 = 10;
    pub const MIN_REMOTE_MAX_SIZE_MB: u32 = 1;
    pub const MAX_REMOTE_MAX_SIZE_MB: u32 = 60;

    /// The opener's argv for a file, or `None` to use the desktop default.
    pub fn file_command(&self, path: &str, line: Option<u32>, column: Option<u32>) -> Option<Vec<String>> {
        let template = self.file_opener.as_deref().map(str::trim).filter(|t| !t.is_empty())?;
        let words = split_words(template)?;
        let (line, column) = (line.unwrap_or(1).to_string(), column.unwrap_or(1).to_string());
        let subs = [("{path}", path), ("{line}", &line), ("{column}", &column)];
        let mut has_path = false;
        let mut argv: Vec<String> = words.iter().map(|w| substitute_tokens(w, &subs, &mut has_path)).collect();
        if !has_path {
            argv.push(path.to_string());
        }
        Some(argv)
    }

    /// The remote opener's argv for a file/directory, or `None` when unset or invalid.
    pub fn remote_file_command(
        &self,
        host: &str,
        user: &str,
        hostname: &str,
        path: &str,
        line: Option<u32>,
        column: Option<u32>,
    ) -> Option<Vec<String>> {
        let template = self.remote_file_opener.as_deref().map(str::trim).filter(|t| !t.is_empty())?;
        let words = split_words(template)?;
        let (line, column) = (line.unwrap_or(1).to_string(), column.unwrap_or(1).to_string());
        let subs =
            [("{host}", host), ("{user}", user), ("{hostname}", hostname), ("{path}", path), ("{line}", &line), ("{column}", &column)];
        let mut has_path = false;
        let mut argv: Vec<String> = words.iter().map(|w| substitute_tokens(w, &subs, &mut has_path)).collect();
        if !has_path {
            argv.push(path.to_string());
        }
        Some(argv)
    }

    /// Automatic remote fetch limit in bytes.
    pub fn remote_max_bytes(&self) -> u64 {
        (self.remote_max_size_mb.clamp(Self::MIN_REMOTE_MAX_SIZE_MB, Self::MAX_REMOTE_MAX_SIZE_MB) as u64) * 1024 * 1024
    }
}

/// Replace `{token}` placeholders in a single pass so values containing literal
/// placeholder text (e.g. a path named `{line}`) are never re-substituted.
fn substitute_tokens(word: &str, subs: &[(&str, &str)], saw_path: &mut bool) -> String {
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while !rest.is_empty() {
        if let Some((token, val)) = subs.iter().find(|(t, _)| rest.starts_with(t)) {
            if *token == "{path}" {
                *saw_path = true;
            }
            out.push_str(val);
            rest = &rest[token.len()..];
        } else {
            let skip = rest.chars().next().map_or(0, char::len_utf8);
            let next = rest[skip..].find('{').map_or(rest.len(), |i| skip + i);
            out.push_str(&rest[..next]);
            rest = &rest[next..];
        }
    }
    out
}

/// Split a command line into words: whitespace separates, '…' and "…"
/// quote, backslash escapes. `None` for an unterminated quote.
fn split_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => cur.push(chars.next()?),
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

/// When pasting asks first (see `goatty_term::paste::check`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PasteConfirm {
    /// Never ask.
    Never,
    /// When the paste would run commands right away, hides control or
    /// invisible characters, pipes a download into a shell, or is huge.
    #[default]
    Suspicious,
    /// Also for every paste of more than one line.
    Multiline,
}

/// What a middle click pastes (in programs without mouse reporting, or
/// with Shift held).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MiddleClick {
    /// The primary selection: the text last selected in any app (Linux).
    /// Where there's no primary selection, the clipboard.
    #[default]
    Primary,
    /// The clipboard.
    Clipboard,
    /// Nothing.
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PasteSettings {
    pub confirm: PasteConfirm,
    pub middle_click: MiddleClick,
}

/// What happens to text you select with the mouse (#266).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SelectionSettings {
    /// Copy to the clipboard as soon as a mouse selection ends, and say so.
    pub auto_copy: bool,
    /// After an auto-copy, unselect the text, as Ctrl+C does. Off keeps it
    /// highlighted.
    pub clear_after_copy: bool,
    /// Also put selected text in the primary selection, which middle click
    /// pastes in any app (Linux, FreeBSD; ignored elsewhere).
    pub primary: bool,
}

impl Default for SelectionSettings {
    fn default() -> Self {
        Self { auto_copy: false, clear_after_copy: true, primary: true }
    }
}

/// What a right click on the terminal does (#186). Programs that use the
/// mouse (vim, tmux, htop with mouse support on) get the click unless the
/// setting says otherwise, and Shift+right-click always does the other
/// thing: goatty's menu where the program would get the click, and the
/// program where the menu would open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RightClick {
    /// goatty's menu; programs that use the mouse get the click.
    #[default]
    Menu,
    /// goatty's menu, in programs that use the mouse too.
    Always,
    /// Copy the selection, or paste when nothing is selected (Windows
    /// Terminal, PuTTY).
    Paste,
    /// Nothing (programs that use the mouse still get the click).
    Off,
}

impl RightClick {
    /// Whether a right click opens goatty's menu, given whether the
    /// program uses the mouse and Shift is held. Otherwise the click goes
    /// to the program, or (without one) does the setting's own thing.
    pub fn opens_menu(self, program_uses_mouse: bool, shift: bool) -> bool {
        match self {
            RightClick::Always => !(shift && program_uses_mouse),
            RightClick::Menu => shift || !program_uses_mouse,
            RightClick::Paste | RightClick::Off => shift,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MouseSettings {
    pub right_click: RightClick,
}

/// What `Ctrl+Backspace` sends in legacy (non-kitty) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CtrlBackspace {
    /// Escape + DEL (`\x1b\x7f`): deletes a word backward in bash, zsh, fish,
    /// readline, and over SSH without host-side shell configuration.
    #[default]
    EscBackspace,
    /// `Ctrl+W` (`^W`): deletes whitespace-delimited words in shells, vim
    /// insert mode, and raw TTY line input.
    CtrlW,
    /// `^H`: xterm default (deletes a word in goatty's managed zsh, one
    /// character in bash and plain zsh).
    CtrlH,
}

/// What `Ctrl+Delete` sends in legacy (non-kitty) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CtrlDelete {
    /// `CSI 3;5~`: xterm default (deletes a word forward in goatty's managed
    /// zsh, fish, and most TUIs).
    #[default]
    Standard,
    /// `Alt+D` (`\x1bd`): deletes a word forward in bash and plain zsh without
    /// extra keybindings.
    AltD,
}

/// Terminal key encoding options (applies locally and to SSH hosts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KeyboardSettings {
    pub ctrl_backspace: CtrlBackspace,
    pub ctrl_delete: CtrlDelete,
}

/// The hosts goatty offers to connect to (command palette, settings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshSettings {
    /// Also list the hosts in `~/.ssh/config` (and the files it includes).
    pub discover: bool,
    /// Hosts added by hand, and the choices made for hosts from
    /// `~/.ssh/config` (matched by `host`).
    pub hosts: Vec<SshHost>,
    /// Where a host picked in the palette or the settings opens.
    pub open_in: OpenIn,
    /// Bringing this computer's shell setup (host settings, plugins,
    /// tools) to a host goatty connects to, for hosts without their own
    /// choice (`hosts[].sync`).
    pub sync: SyncMode,
    /// Show typed characters before the host echoes them (#16).
    pub local_echo: LocalEcho,
}

impl Default for SshSettings {
    fn default() -> Self {
        Self { discover: true, hosts: Vec::new(), open_in: OpenIn::Window, sync: SyncMode::Ask, local_echo: LocalEcho::Auto }
    }
}

/// Local echo prediction: draw what is typed (underlined) right away and
/// check it against the host's echo, mosh-style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LocalEcho {
    /// When the host is slow enough to notice: its round trip or the
    /// time it takes to echo a key is above [`LocalEcho::AUTO_RTT`].
    #[default]
    Auto,
    /// However fast the host answers.
    Always,
    Never,
}

impl LocalEcho {
    /// Round trips above this are slow enough to feel when typing.
    pub const AUTO_RTT: std::time::Duration = std::time::Duration::from_millis(30);

    /// Whether to show predictions for a host that answers in this time
    /// (`None`: not measured yet).
    pub fn wanted(self, rtt: Option<std::time::Duration>) -> bool {
        match self {
            LocalEcho::Auto => rtt.is_some_and(|r| r > Self::AUTO_RTT),
            LocalEcho::Always => true,
            LocalEcho::Never => false,
        }
    }
}

/// Whether goatty copies this computer's shell setup to a host when it
/// connects and the host's differs (#159).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Ask once for each change made here.
    #[default]
    Ask,
    /// Copy it without asking.
    Always,
    /// Leave the host's setup alone.
    Never,
}

/// Where a connection to another host opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenIn {
    /// A terminal window of its own.
    #[default]
    Window,
    /// A new tab of the window it was picked from.
    Tab,
}

impl OpenIn {
    /// The other one (Shift+Enter in the session picker).
    pub fn flip(self) -> Self {
        match self {
            OpenIn::Window => OpenIn::Tab,
            OpenIn::Tab => OpenIn::Window,
        }
    }
}

impl SshSettings {
    pub fn get(&self, host: &str) -> Option<&SshHost> {
        self.hosts.iter().find(|h| h.host == host)
    }

    /// Whether `host` connects with plain SSH.
    pub fn plain(&self, host: &str) -> bool {
        self.get(host).is_some_and(|h| h.plain)
    }

    /// Whether to bring the shell setup to `host`: its own choice, else
    /// the default.
    pub fn sync_mode(&self, host: &str) -> SyncMode {
        self.get(host).and_then(|h| h.sync).unwrap_or(self.sync)
    }

    /// Change the entry for `host` (created if missing). An entry left
    /// without a name or a choice is removed when `keep` is false: hosts
    /// from `~/.ssh/config` need no entry of their own.
    pub fn edit(&mut self, host: &str, keep: bool, f: impl FnOnce(&mut SshHost)) {
        let i = match self.hosts.iter().position(|h| h.host == host) {
            Some(i) => i,
            None => {
                self.hosts.push(SshHost { host: host.into(), ..Default::default() });
                self.hosts.len() - 1
            }
        };
        f(&mut self.hosts[i]);
        if !keep && self.hosts[i].name.is_none() && !self.hosts[i].plain && self.hosts[i].sync.is_none() {
            self.hosts.remove(i);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SshHost {
    /// What ssh is given: an alias from `~/.ssh/config`, `host` or `user@host`.
    pub host: String,
    /// Shown instead of `host`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Connect with plain SSH (`ssh host` in a local session) instead of
    /// goatty's daemon on the host. Nothing is installed there.
    pub plain: bool,
    /// Bring this computer's shell setup here; `None`: `ssh.sync`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncMode>,
}

impl SshHost {
    /// The name to show.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.host)
    }
}

/// Why `host` can't be given to ssh as a destination, if it can't.
pub fn ssh_host_problem(host: &str) -> Option<&'static str> {
    if host.is_empty() {
        Some("it's empty")
    } else if host.starts_with('-') {
        Some("it can't start with -")
    } else if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("it can't contain spaces")
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DebugSettings {
    /// Print per-second frame statistics to stderr.
    pub stats: bool,
}

impl Document for ClientSettings {
    const NAME: &'static str = "settings";
    const VERSION: u32 = 3;
    const OPEN_MAPS: &'static [&'static str] = &["keybindings", "theme.overrides"];

    fn migrations() -> &'static [Migration] {
        &[(1, global_theme_overrides_became_per_theme), (2, copy_on_select_became_auto_copy)]
    }

    fn default_version(user: &serde_json::Value) -> u32 {
        let has_v1_overrides = user.pointer("/theme/overrides").and_then(serde_json::Value::as_object).is_some_and(looks_like_v1_overrides);
        if has_v1_overrides {
            1
        } else if user.pointer("/selection/copy_on_select").is_some() {
            2
        } else {
            Self::VERSION
        }
    }

    fn validate(&mut self) -> Vec<Issue> {
        let mut issues = Vec::new();
        let f = &mut self.font;
        clamp(&mut f.size, FontSettings::MIN_SIZE, FontSettings::MAX_SIZE, "font.size", &mut issues);
        clamp(&mut f.line_height, FontSettings::MIN_LINE_HEIGHT, FontSettings::MAX_LINE_HEIGHT, "font.line_height", &mut issues);
        let pad = &mut self.terminal.padding;
        if pad.is_uniform() {
            let mut v = pad.top;
            clamp(&mut v, 0.0, TerminalSettings::MAX_PADDING, "terminal.padding", &mut issues);
            *pad = Padding::all(v);
        } else {
            for (side, name) in pad.sides_mut().into_iter().zip(["top", "right", "bottom", "left"]) {
                clamp(side, 0.0, TerminalSettings::MAX_PADDING, &format!("terminal.padding.{name}"), &mut issues);
            }
        }
        clamp(&mut self.panes.dim_inactive, 0.0, PaneSettings::MAX_DIM, "panes.dim_inactive", &mut issues);
        clamp(
            &mut self.tabs.sidebar_width,
            TabSettings::MIN_SIDEBAR_WIDTH,
            TabSettings::MAX_SIDEBAR_WIDTH,
            "tabs.sidebar_width",
            &mut issues,
        );
        clamp(&mut self.terminal.opacity, TerminalSettings::MIN_OPACITY, 1.0, "terminal.opacity", &mut issues);
        // Theme names are checked against the theme library (user themes
        // live in their own files): `ThemeLibrary::resolve`.
        for (id, overrides) in &mut self.theme.overrides {
            if overrides.ansi.len() > 16 {
                issues.push(Issue::new(format!("theme.overrides.{id}.ansi"), "only 16 ANSI colors exist; the rest are ignored"));
                overrides.ansi.truncate(16);
            }
        }
        self.theme.overrides.retain(|_, o| !o.is_empty());
        // Old spellings (`ctrl-shift-+`) become the ones GPUI reports (`ctrl-+`).
        if let Some(opener) = &self.links.file_opener {
            if opener.trim().is_empty() {
                self.links.file_opener = None;
            } else if split_words(opener).is_none() {
                issues.push(Issue::new("links.file_opener", "unterminated quote; opening files with the desktop default"));
                self.links.file_opener = None;
            }
        }
        if let Some(opener) = &self.links.remote_file_opener {
            if opener.trim().is_empty() {
                self.links.remote_file_opener = None;
            } else if split_words(opener).is_none() {
                issues.push(Issue::new("links.remote_file_opener", "unterminated quote; falling back to local read-only copies"));
                self.links.remote_file_opener = None;
            }
        }
        let max_mb = self.links.remote_max_size_mb.clamp(LinkSettings::MIN_REMOTE_MAX_SIZE_MB, LinkSettings::MAX_REMOTE_MAX_SIZE_MB);
        if max_mb != self.links.remote_max_size_mb {
            issues.push(Issue::new(
                "links.remote_max_size_mb",
                format!(
                    "{} is outside {}–{}; using {max_mb}",
                    self.links.remote_max_size_mb,
                    LinkSettings::MIN_REMOTE_MAX_SIZE_MB,
                    LinkSettings::MAX_REMOTE_MAX_SIZE_MB
                ),
            ));
            self.links.remote_max_size_mb = max_mb;
        }
        let mut seen = std::collections::HashSet::new();
        let mut i = 0;
        self.ssh.hosts.retain_mut(|h| {
            let path = format!("ssh.hosts.{i}");
            i += 1;
            h.host = h.host.trim().to_string();
            h.name = h.name.take().map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            if let Some(why) = ssh_host_problem(&h.host) {
                issues.push(Issue::new(path, format!("host {:?}: {why}; ignored", h.host)));
                return false;
            }
            if !seen.insert(h.host.clone()) {
                issues.push(Issue::new(path, format!("{} is listed twice; the first entry counts", h.host)));
                return false;
            }
            true
        });
        for (key, problem) in keys::canonicalize_overrides(&mut self.keybindings) {
            issues.push(Issue::new(format!("keybindings.{key}"), problem));
        }
        for (key, action) in &self.keybindings {
            if let Some(a) = action
                && !keys::is_valid_action(a)
            {
                issues.push(Issue::new(format!("keybindings.{key}"), format!("unknown action {a:?}")));
            }
        }
        issues
    }
}

const V1_OVERRIDE_SLOTS: &[&str] =
    &["foreground", "background", "cursor", "selection_foreground", "selection_background", "accent", "ansi"];

fn looks_like_v1_overrides(overrides: &serde_json::Map<String, serde_json::Value>) -> bool {
    !overrides.is_empty() && overrides.iter().any(|(k, v)| V1_OVERRIDE_SLOTS.contains(&k.as_str()) || !v.is_object())
}

/// Version 2 → 3 (#266): `selection.copy_on_select` (`primary`,
/// `clipboard` or `off`) became the `auto_copy` and `primary` switches.
fn copy_on_select_became_auto_copy(doc: &mut serde_json::Value) {
    use serde_json::Value;
    let Some(selection) = doc.as_object_mut().and_then(|m| m.get_mut("selection")).and_then(Value::as_object_mut) else {
        return;
    };
    match selection.remove("copy_on_select").as_ref().and_then(Value::as_str) {
        Some("clipboard") => {
            selection.entry("auto_copy").or_insert(Value::Bool(true));
        }
        Some("off") => {
            selection.entry("primary").or_insert(Value::Bool(false));
        }
        _ => {}
    }
}

/// Version 1 → 2 (#246): `theme.overrides` became a map keyed by theme id
/// instead of one global override applied to whichever theme is active.
fn global_theme_overrides_became_per_theme(doc: &mut serde_json::Value) {
    use serde_json::{Map, Value};
    let Some(theme) = doc.as_object_mut().and_then(|m| m.get_mut("theme")).and_then(Value::as_object_mut) else {
        return;
    };
    let Some(Value::Object(old)) = theme.get("overrides").cloned() else {
        return;
    };
    if old.is_empty() {
        theme.remove("overrides");
        return;
    }
    // Only migrate flat v1 overrides; leave already-nested objects alone.
    if !looks_like_v1_overrides(&old) {
        return;
    }
    let follow = theme.get("follow_system").and_then(Value::as_bool).unwrap_or(false);
    let pick_id = |key: &str, default: &str| {
        theme.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(default).to_string()
    };
    let mut per_theme = Map::new();
    if follow {
        let dark = pick_id("dark", DEFAULT_THEME);
        let light = pick_id("light", DEFAULT_LIGHT_THEME);
        per_theme.insert(dark, Value::Object(old.clone()));
        per_theme.entry(light).or_insert_with(|| Value::Object(old));
    } else {
        let name = pick_id("name", DEFAULT_THEME);
        per_theme.insert(name, Value::Object(old));
    }
    theme.insert("overrides".into(), Value::Object(per_theme));
}

/// Clamp `v` into `[lo, hi]`, reporting a change. NaN becomes `lo`.
pub(crate) fn clamp(v: &mut f32, lo: f32, hi: f32, path: &str, issues: &mut Vec<Issue>) {
    let c = if v.is_nan() { lo } else { v.clamp(lo, hi) };
    if c != *v {
        issues.push(Issue::new(path, format!("{v} is outside {lo}–{hi}; using {c}")));
        *v = c;
    }
}

impl ClientSettings {
    /// Apply `GOATTY_*` overrides read through `var`. Unparsable values are
    /// ignored so a typo never prevents the terminal from starting.
    pub fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) {
        if let Some(f) = var("GOATTY_FONT").map(|f| f.trim().to_string()).filter(|f| !f.is_empty()) {
            self.font.family = Some(f);
        }
        if let Some(f) = var("GOATTY_UI_FONT").map(|f| f.trim().to_string()).filter(|f| !f.is_empty()) {
            self.font.ui_family = Some(f);
        }
        if let Some(size) = var("GOATTY_FONT_SIZE").and_then(|s| s.parse::<f32>().ok()) {
            self.font.size = size.clamp(FontSettings::MIN_SIZE, FontSettings::MAX_SIZE);
        }
        match var("GOATTY_RENDER").as_deref() {
            Some("shaped") => self.terminal.renderer = Renderer::Shaped,
            Some("glyphs") => self.terminal.renderer = Renderer::Glyphs,
            _ => {}
        }
        if let Some(t) = var("GOATTY_THEME").filter(|t| !t.is_empty()) {
            self.theme.name = t;
            self.theme.follow_system = false;
        }
        if var("GOATTY_STATS").is_some() {
            self.debug.stats = true;
        }
    }

    /// Names of the `GOATTY_*` variables [`apply_env`](Self::apply_env)
    /// accepts. One it ignores (an empty `GOATTY_THEME`, a size that isn't
    /// a number) overrides nothing, so its rows stay editable.
    pub fn env_overrides(var: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
        let accepted = |name: &str| {
            let v = var(name);
            match name {
                "GOATTY_FONT_SIZE" => v.is_some_and(|s| s.parse::<f32>().is_ok()),
                "GOATTY_RENDER" => matches!(v.as_deref(), Some("shaped" | "glyphs")),
                "GOATTY_STATS" => v.is_some(),
                "GOATTY_FONT" | "GOATTY_UI_FONT" => v.is_some_and(|s| !s.trim().is_empty()),
                _ => v.is_some_and(|s| !s.is_empty()),
            }
        };
        Self::ENV_KEYS.iter().map(|(name, _)| *name).filter(|k| accepted(k)).collect()
    }

    /// Each `GOATTY_*` variable [`apply_env`](Self::apply_env) reads, with
    /// the settings it replaces (the settings window marks their rows).
    pub const ENV_KEYS: &'static [(&'static str, &'static [&'static str])] = &[
        ("GOATTY_FONT", &["font.family"]),
        ("GOATTY_UI_FONT", &["font.ui_family"]),
        ("GOATTY_FONT_SIZE", &["font.size"]),
        ("GOATTY_RENDER", &["terminal.renderer"]),
        ("GOATTY_THEME", &["theme.name", "theme.follow_system"]),
        ("GOATTY_STATS", &["debug.stats"]),
    ];

    /// The settings a `GOATTY_*` variable called `name` replaces.
    pub fn env_keys(name: &str) -> &'static [&'static str] {
        Self::ENV_KEYS.iter().find(|(n, _)| *n == name).map_or(&[], |(_, k)| k)
    }

    /// Theme colors in the settings file `text` that read with a loss: an
    /// ignored alpha, or a value outside sRGB that was clamped or
    /// gamut-mapped. They load fine (and are written back as `#rrggbb`);
    /// `goatty settings validate` points them out.
    pub fn lossy_colors(text: &str) -> Vec<Issue> {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
        let Some(overrides) = v.pointer("/theme/overrides") else { return Vec::new() };
        crate::colorspace::lossy_colors(overrides, "theme.overrides").into_iter().map(|(path, what)| Issue::new(path, what)).collect()
    }

    /// Checks that need this machine (installed fonts, programs on PATH),
    /// made after the `GOATTY_*` overrides: things that parse but won't
    /// work as written. Nothing is changed; goatty already falls back.
    pub fn check_environment(&self, fonts: &[String], has_program: impl Fn(&str) -> bool) -> Vec<Issue> {
        let mut issues = Vec::new();
        let installed = |name: &str| fonts.iter().any(|f| f.eq_ignore_ascii_case(name));
        // Fallbacks are matched exactly, as `FontSettings::resolve_family` does.
        let fallback_installed = |name: &str| fonts.iter().any(|f| f == name);
        match self.font.family.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) if !GENERIC_FONTS.contains(&f) && !fonts.is_empty() && !installed(f) => {
                issues.push(Issue::new("font.family", format!("{f:?} isn't installed; the system picks another font")));
            }
            None if !fonts.is_empty() && !self.font.fallbacks.is_empty() && !self.font.fallbacks.iter().any(|f| fallback_installed(f)) => {
                issues.push(Issue::new(
                    "font.fallbacks",
                    format!("none of these fonts is installed; using the bundled {}", FontSettings::BUNDLED),
                ));
            }
            _ => {}
        }
        let custom_ui_fallbacks = self.font.ui_fallbacks.iter().map(String::as_str).ne(FontSettings::DEFAULT_UI_FALLBACKS.iter().copied());
        match self.font.ui_family.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) if !GENERIC_FONTS.contains(&f) && !fonts.is_empty() && !installed(f) => {
                issues.push(Issue::new("font.ui_family", format!("{f:?} isn't installed; the system picks another font")));
            }
            None if !fonts.is_empty()
                && !self.font.ui_fallbacks.is_empty()
                && custom_ui_fallbacks
                && !self.font.ui_fallbacks.iter().any(|f| fallback_installed(f)) =>
            {
                issues.push(Issue::new("font.ui_fallbacks", "none of these fonts is installed; using sans-serif"));
            }
            _ => {}
        }
        if let Some(program) = self.links.file_opener.as_deref().and_then(split_words).and_then(|w| w.into_iter().next())
            && !has_program(&program)
        {
            issues.push(Issue::new("links.file_opener", format!("{program:?} isn't installed or not on PATH; opening files will fail")));
        }
        if let Some(program) = self.links.remote_file_opener.as_deref().and_then(split_words).and_then(|w| w.into_iter().next())
            && !has_program(&program)
        {
            issues.push(Issue::new(
                "links.remote_file_opener",
                format!("{program:?} isn't installed or not on PATH; opening remote files will fail"),
            ));
        }
        issues
    }
}

/// Font names that pick a font by kind rather than by family.
const GENERIC_FONTS: &[&str] = &["monospace", "sans-serif", "serif", "system-ui"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_echo_auto_waits_for_a_slow_measured_link() {
        use std::time::Duration;
        let ms = |n| Some(Duration::from_millis(n));
        assert!(!LocalEcho::Auto.wanted(None));
        assert!(!LocalEcho::Auto.wanted(ms(5)));
        assert!(LocalEcho::Auto.wanted(ms(80)));
        assert!(LocalEcho::Always.wanted(None));
        assert!(!LocalEcho::Never.wanted(ms(500)));
        let s: SshSettings = serde_json::from_str(r#"{"local_echo": "always"}"#).unwrap();
        assert_eq!(s.local_echo, LocalEcho::Always);
    }

    #[test]
    fn title_bar_setting() {
        use crate::store;
        assert_eq!(ClientSettings::default().window.title_bar, TitleBar::System);
        let l = store::parse::<ClientSettings>(r#"{"window": {"title_bar": "goatty"}}"#);
        assert!(l.issues.is_empty(), "{:?}", l.issues);
        assert_eq!(l.value.window.title_bar, TitleBar::Goatty);
        let l = store::parse::<ClientSettings>(r#"{"window": {"title_bar": "fancy"}}"#);
        assert_eq!(l.value.window.title_bar, TitleBar::System, "a bad value keeps the default");
        assert!(!l.issues.is_empty());
    }

    #[test]
    fn theme_colors_take_css_and_are_written_as_hex() {
        use crate::store;
        use crate::theme::{Color, ColorSlot};
        let l = store::set_key::<ClientSettings>("", "theme.overrides.nord.foreground", "rgb(255 128 0)").unwrap();
        assert_eq!(l.value.theme.overrides_for("nord").and_then(|o| o.get(ColorSlot::Foreground)), Some(Color(0xff8000)));
        assert!(store::to_json(&l.value, &l.unknown).contains("\"#ff8000\""));
        let l = store::set_key::<ClientSettings>("", "theme.overrides.nord.background", "oklch(0 0 0)").unwrap();
        assert_eq!(l.value.theme.overrides_for("nord").and_then(|o| o.get(ColorSlot::Background)), Some(Color(0)));
        assert!(
            store::set_key::<ClientSettings>("", "theme.overrides.nord.background", "255 128 0").is_err(),
            "bare numbers are ambiguous"
        );
        assert!(store::set_key::<ClientSettings>("", "theme.overrides.nord.background", "reddish").is_err());

        let text = r##"{"theme": {"overrides": {"nord": {"foreground": "#ff800080", "cursor": "hsl(30, 100%, 50%)", "ansi": ["oklch(0.7 0.4 150)"]}}}}"##;
        let l = store::parse::<ClientSettings>(text);
        assert!(l.issues.is_empty(), "{:?}", l.issues);
        assert_eq!(l.value.theme.overrides_for("nord").and_then(|o| o.get(ColorSlot::Cursor)), Some(Color(0xff8000)));
        let mut lossy: Vec<String> = ClientSettings::lossy_colors(text).into_iter().map(|i| i.path).collect();
        lossy.sort();
        assert_eq!(lossy, ["theme.overrides.nord.ansi.0", "theme.overrides.nord.foreground"]);
    }

    #[test]
    fn version_1_theme_overrides_migrate_to_per_theme() {
        use crate::store;
        use crate::theme::{Color, ColorSlot};
        let v1 = r##"{"version": 1, "theme": {"name": "nord", "overrides": {"background": "#112233", "ansi": [null, "#445566"]}}}"##;
        let loaded = store::parse::<ClientSettings>(v1);
        assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
        let nord = loaded.value.theme.overrides_for("nord").expect("migrated to nord");
        assert_eq!(nord.get(ColorSlot::Background), Some(Color(0x112233)));
        assert_eq!(nord.get(ColorSlot::Ansi(1)), Some(Color(0x445566)));
        assert!(loaded.value.theme.overrides_for("dracula").is_none());

        let v1_follow = r##"{"version": 1, "theme": {"follow_system": true, "light": "solarized-light", "dark": "dracula", "overrides": {"accent": "#abcdef"}}}"##;
        let loaded = store::parse::<ClientSettings>(v1_follow);
        assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
        assert_eq!(loaded.value.theme.overrides_for("dracula").and_then(|o| o.get(ColorSlot::Accent)), Some(Color(0xabcdef)));
        assert_eq!(loaded.value.theme.overrides_for("solarized-light").and_then(|o| o.get(ColorSlot::Accent)), Some(Color(0xabcdef)));

        // Unversioned files with flat v1 overrides still take the v1 migration path,
        // while unversioned per-theme overrides load directly.
        let unversioned_v1 = r##"{"theme": {"overrides": {"background": "#112233"}}}"##;
        let loaded = store::parse::<ClientSettings>(unversioned_v1);
        assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
        assert_eq!(loaded.value.theme.overrides_for(DEFAULT_THEME).and_then(|o| o.get(ColorSlot::Background)), Some(Color(0x112233)));

        // set_key / unset_key migrate v1 files before applying edits.
        let edited = store::set_key::<ClientSettings>(v1, "theme.overrides.nord.background", "#445566").unwrap();
        assert!(edited.issues.is_empty(), "{:?}", edited.issues);
        assert_eq!(edited.value.theme.overrides_for("nord").and_then(|o| o.get(ColorSlot::Background)), Some(Color(0x445566)));
        assert_eq!(edited.value.theme.overrides_for("nord").and_then(|o| o.get(ColorSlot::Ansi(1))), Some(Color(0x445566)));

        let unset = store::unset_key::<ClientSettings>(v1, "theme.overrides.nord.background").unwrap();
        assert!(unset.issues.is_empty(), "{:?}", unset.issues);
        assert_eq!(unset.value.theme.overrides_for("nord").and_then(|o| o.get(ColorSlot::Background)), None);
        assert_eq!(unset.value.theme.overrides_for("nord").and_then(|o| o.get(ColorSlot::Ansi(1))), Some(Color(0x445566)));
    }

    #[test]
    fn version_2_copy_on_select_migrates_to_auto_copy() {
        use crate::store;
        let sel = |text: &str| {
            let loaded = store::parse::<ClientSettings>(text);
            assert!(loaded.issues.is_empty(), "{text}: {:?}", loaded.issues);
            loaded.value.selection
        };
        let on = SelectionSettings { auto_copy: true, ..SelectionSettings::default() };
        let no_primary = SelectionSettings { primary: false, ..SelectionSettings::default() };
        assert_eq!(sel(r#"{"version": 2, "selection": {"copy_on_select": "clipboard"}}"#), on);
        assert_eq!(sel(r#"{"version": 2, "selection": {"copy_on_select": "off"}}"#), no_primary);
        assert_eq!(sel(r#"{"version": 2, "selection": {"copy_on_select": "primary"}}"#), SelectionSettings::default());
        // Unversioned files with the old key take the migration too.
        assert_eq!(sel(r#"{"selection": {"copy_on_select": "clipboard"}}"#), on);
        // The new keys load as they are.
        let keep = r#"{"selection": {"auto_copy": true, "clear_after_copy": false}}"#;
        assert_eq!(sel(keep), SelectionSettings { auto_copy: true, clear_after_copy: false, primary: true });

        // set_key migrates the old key before applying the edit.
        let edited =
            store::set_key::<ClientSettings>(r#"{"version": 2, "selection": {"copy_on_select": "off"}}"#, "selection.auto_copy", "true")
                .unwrap();
        assert!(edited.issues.is_empty(), "{:?}", edited.issues);
        assert_eq!(edited.value.selection, SelectionSettings { auto_copy: true, clear_after_copy: true, primary: false });
    }

    fn with(vars: &[(&str, &str)]) -> ClientSettings {
        let mut s = ClientSettings::default();
        s.apply_env(|k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string()));
        s
    }

    #[test]
    fn env_overrides() {
        let s = with(&[
            ("GOATTY_FONT", "Iosevka"),
            ("GOATTY_UI_FONT", "Cantarell"),
            ("GOATTY_FONT_SIZE", "16"),
            ("GOATTY_RENDER", "shaped"),
            ("GOATTY_STATS", "1"),
        ]);
        assert_eq!(s.font.family.as_deref(), Some("Iosevka"));
        assert_eq!(s.font.ui_family.as_deref(), Some("Cantarell"));
        assert_eq!(s.font.size, 16.0);
        assert_eq!(s.terminal.renderer, Renderer::Shaped);
        assert!(s.debug.stats);
        assert_eq!(with(&[("GOATTY_THEME", "nord")]).theme.name, "nord");
        assert!(!with(&[("GOATTY_THEME", "nord")]).theme.follow_system, "an explicit theme wins over following the system");
    }

    #[test]
    fn bad_env_values_keep_defaults() {
        let s = with(&[("GOATTY_FONT_SIZE", "huge"), ("GOATTY_RENDER", "fancy"), ("GOATTY_FONT", ""), ("GOATTY_UI_FONT", "  ")]);
        assert_eq!(s, ClientSettings::default());
        assert_eq!(with(&[("GOATTY_FONT_SIZE", "500")]).font.size, FontSettings::MAX_SIZE);
    }

    #[test]
    fn vertical_tabs() {
        let t = TabSettings::default();
        assert_eq!((t.style, t.sidebar_width, t.sidebar_collapsed, t.scroll_to_switch), (TabStyle::Modern, 220.0, false, false));
        let t: TabSettings =
            serde_json::from_str(r#"{"style": "vertical", "sidebar_width": 300, "sidebar_collapsed": true, "scroll_to_switch": true}"#)
                .unwrap();
        assert_eq!((t.style, t.sidebar_width, t.sidebar_collapsed, t.scroll_to_switch), (TabStyle::Vertical, 300.0, true, true));

        let mut s = ClientSettings::default();
        s.tabs.sidebar_width = 9000.0;
        let issues = s.validate();
        assert_eq!(s.tabs.sidebar_width, TabSettings::MAX_SIDEBAR_WIDTH);
        assert_eq!(issues.iter().map(|i| i.path.as_str()).collect::<Vec<_>>(), ["tabs.sidebar_width"]);
        s.tabs.sidebar_width = 10.0;
        s.validate();
        assert_eq!(s.tabs.sidebar_width, TabSettings::MIN_SIDEBAR_WIDTH);
    }

    #[test]
    fn tab_markers() {
        let t = TabSettings::default();
        assert!(t.mark_bell);
        assert_eq!(t.mark_activity, TabActivity::Finished);
        let t: TabSettings = serde_json::from_str(r#"{"mark_bell": false, "mark_activity": "output"}"#).unwrap();
        assert_eq!((t.mark_bell, t.mark_activity), (false, TabActivity::Output));
    }

    #[test]
    fn font_resolution() {
        let f = FontSettings::default();
        let system = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        // Nothing (or no Nerd Font) installed: the bundled one, ahead of
        // plain fonts that come later in the list.
        assert_eq!(f.resolve_family(&[]), FontSettings::BUNDLED);
        assert_eq!(f.resolve_family(&system(&["Fira Code", "DejaVu Sans Mono"])), FontSettings::BUNDLED);
        // An installed Nerd Font from the list wins over the bundled one.
        assert_eq!(f.resolve_family(&system(&["DejaVu Sans Mono", "FiraCode Nerd Font"])), "FiraCode Nerd Font");
        // A list without the bundled font is taken in order.
        let plain = FontSettings { fallbacks: system(&["Fira Code", "DejaVu Sans Mono"]), ..f.clone() };
        assert_eq!(plain.resolve_family(&system(&["DejaVu Sans Mono", "Fira Code"])), "Fira Code");
        assert_eq!(plain.resolve_family(&system(&["Hack"])), FontSettings::BUNDLED);
        let explicit = FontSettings { family: Some("Iosevka".into()), ..f.clone() };
        assert_eq!(explicit.resolve_family(&[]), "Iosevka");
        assert!(is_nerd_font("Hack NF") && is_nerd_font("JetBrainsMono Nerd Font Mono") && !is_nerd_font("Fira Code"));

        // UI font: explicit family, then first installed fallback, then sans-serif.
        assert_eq!(f.resolve_ui_family(&[]), "sans-serif");
        assert_eq!(f.resolve_ui_family(&system(&["Noto Sans", "Inter"])), "Inter");
        let ui_explicit = FontSettings { ui_family: Some("Atkinson Hyperlegible".into()), ..f };
        assert_eq!(ui_explicit.resolve_ui_family(&[]), "Atkinson Hyperlegible");
    }

    #[test]
    fn file_opener_commands() {
        let with = |t: &str| LinkSettings { file_opener: Some(t.into()), ..Default::default() };
        assert_eq!(LinkSettings::default().file_command("/a b", None, None), None);
        assert_eq!(with("code --goto {path}:{line}:{column}").file_command("/a b", Some(3), None).unwrap(), ["code", "--goto", "/a b:3:1"]);
        assert_eq!(with("'my editor' -n").file_command("/x;rm", None, None).unwrap(), ["my editor", "-n", "/x;rm"]);
        assert_eq!(with(r#"sh -c "e \"$1\"" _ {path}"#).file_command("/f", None, None).unwrap(), ["sh", "-c", "e \"$1\"", "_", "/f"]);
        assert_eq!(with("  ").file_command("/f", None, None), None);
        assert_eq!(split_words("a 'b"), None);

        let mut s = ClientSettings::default();
        s.links.file_opener = Some("vim 'oops".into());
        let issues = s.validate();
        assert_eq!(issues[0].path, "links.file_opener");
        assert_eq!(s.links.file_opener, None);
    }

    #[test]
    fn remote_file_settings_and_commands() {
        let mut links = LinkSettings::default();
        assert_eq!(links.remote_files, RemoteFileOpen::Auto);
        assert_eq!(links.remote_max_size_mb, 10);
        assert_eq!(links.remote_max_bytes(), 10 * 1024 * 1024);
        assert_eq!(links.remote_file_command("box", "alice", "box.local", "/a b", None, None), None);

        links.remote_file_opener = Some("code --remote ssh-remote+{host} --goto {path}:{line}:{column}".into());
        assert_eq!(
            links.remote_file_command("box", "alice", "box.local", "/a b", Some(12), Some(4)).unwrap(),
            ["code", "--remote", "ssh-remote+box", "--goto", "/a b:12:4"]
        );

        links.remote_file_opener = Some("zed ssh://{user}@{hostname}{path}:{line}".into());
        assert_eq!(
            links.remote_file_command("box", "alice", "box.local", "/srv/app.rs", Some(7), None).unwrap(),
            ["zed", "ssh://alice@box.local/srv/app.rs:7"]
        );

        links.remote_file_opener = Some("nvim".into());
        assert_eq!(links.remote_file_command("box", "alice", "box.local", "/etc/hosts", None, None).unwrap(), ["nvim", "/etc/hosts"]);

        let mut s = ClientSettings::default();
        s.links.remote_file_opener = Some("code 'oops".into());
        s.links.remote_max_size_mb = 500;
        let issues = s.validate();
        let paths: Vec<_> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["links.remote_file_opener", "links.remote_max_size_mb"]);
        assert_eq!(s.links.remote_file_opener, None);
        assert_eq!(s.links.remote_max_size_mb, LinkSettings::MAX_REMOTE_MAX_SIZE_MB);

        s.links.remote_max_size_mb = 0;
        assert_eq!(s.validate().len(), 1);
        assert_eq!(s.links.remote_max_size_mb, LinkSettings::MIN_REMOTE_MAX_SIZE_MB);

        s.links.remote_max_size_mb = 61;
        assert_eq!(s.validate().len(), 1);
        assert_eq!(s.links.remote_max_size_mb, LinkSettings::MAX_REMOTE_MAX_SIZE_MB);

        // Values containing literal placeholder syntax or UTF-8 must not be re-substituted.
        links.remote_file_opener = Some("ed --h={hostname} --u={user} --p={path}:{line}:{column}".into());
        assert_eq!(
            links.remote_file_command("b{user}", "u{path}", "h{path}ä", "/tmp/{line}/{column}.rs", Some(9), Some(2)).unwrap(),
            ["ed", "--h=h{path}ä", "--u=u{path}", "--p=/tmp/{line}/{column}.rs:9:2"]
        );

        let parsed: ClientSettings = serde_json::from_str(r#"{"links":{"remote_files":"fetch","remote_max_size_mb":25}}"#).unwrap();
        assert_eq!(parsed.links.remote_files, RemoteFileOpen::Fetch);
        assert_eq!(parsed.links.remote_max_size_mb, 25);
    }

    #[test]
    fn hosts_open_in_a_window_unless_set_to_tabs() {
        assert_eq!(ClientSettings::default().ssh.open_in, OpenIn::Window);
        let s: ClientSettings = serde_json::from_str(r#"{"ssh":{"open_in":"tab"}}"#).unwrap();
        assert_eq!((s.ssh.open_in, s.ssh.discover), (OpenIn::Tab, true));
        assert_eq!(OpenIn::Tab.flip(), OpenIn::Window);
    }

    #[test]
    fn ssh_hosts_are_checked_and_edited() {
        let mut s = ClientSettings::default();
        let h = |host: &str, name: Option<&str>| SshHost { host: host.into(), name: name.map(Into::into), ..Default::default() };
        s.ssh.hosts =
            vec![h(" box ", Some("  ")), h("-oProxyCommand=x", None), h("a b", None), h("box", Some("Box")), h("pi", Some(" Pi "))];
        let issues = s.validate();
        let paths: Vec<_> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["ssh.hosts.1", "ssh.hosts.2", "ssh.hosts.3"]);
        assert_eq!(s.ssh.hosts, [h("box", None), h("pi", Some("Pi"))]);
        assert_eq!(s.ssh.hosts[1].label(), "Pi");

        // A discovered host's choice comes and goes without leaving an empty entry.
        s.ssh.edit("nas", false, |h| h.plain = true);
        assert!(s.ssh.plain("nas"));
        s.ssh.edit("nas", false, |h| h.plain = false);
        assert!(s.ssh.get("nas").is_none());
        // Syncing the shell setup: the host's own choice, else the default.
        assert_eq!(s.ssh.sync_mode("nas"), SyncMode::Ask);
        s.ssh.sync = SyncMode::Never;
        s.ssh.edit("nas", false, |h| h.sync = Some(SyncMode::Always));
        assert_eq!((s.ssh.sync_mode("nas"), s.ssh.sync_mode("other")), (SyncMode::Always, SyncMode::Never));
        s.ssh.edit("nas", false, |h| h.sync = None);
        assert!(s.ssh.get("nas").is_none());
        s.ssh.sync = SyncMode::Ask;
        // A host added by hand stays.
        s.ssh.edit("box", true, |h| h.plain = false);
        assert!(s.ssh.get("box").is_some());
    }

    #[test]
    fn padding_is_a_number_or_per_side() {
        use crate::store;
        let parse = |json: &str| store::parse::<ClientSettings>(json);
        assert_eq!(parse(r#"{"terminal": {"padding": 10}}"#).value.terminal.padding, Padding::all(10.0));
        let l = parse(r#"{"terminal": {"padding": {"left": 12, "right": 12}}}"#);
        assert!(l.issues.is_empty(), "{:?}", l.issues);
        assert_eq!(l.value.terminal.padding, Padding { top: 6.0, right: 12.0, bottom: 6.0, left: 12.0 });
        // Per side: each one is clamped on its own.
        let l = parse(r#"{"terminal": {"padding": {"top": 99}}}"#);
        assert_eq!(l.value.terminal.padding.top, TerminalSettings::MAX_PADDING);
        assert_eq!(l.issues.iter().map(|i| i.path.as_str()).collect::<Vec<_>>(), ["terminal.padding.top"]);
        // Garbage falls back to the default.
        let l = parse(r#"{"terminal": {"padding": "wide"}}"#);
        assert_eq!(l.value.terminal.padding, Padding::default());
        assert_eq!(l.issues.len(), 1);
        // Uniform padding is saved as one number; mixed as an object.
        let mut s = ClientSettings::default();
        s.terminal.padding = Padding::all(8.0);
        assert!(store::to_json(&s, &[]).contains(r#""padding": 8.0"#));
        s.terminal.padding.left = 20.0;
        let json = store::to_json(&s, &[]);
        assert_eq!(parse(&json).value.terminal.padding, s.terminal.padding, "{json}");
    }

    #[test]
    fn environment_checks() {
        let fonts: Vec<String> = ["DejaVu Sans Mono", "Fira Code"].map(String::from).to_vec();
        let ok = |_: &str| true;
        let paths = |s: &ClientSettings, has: &dyn Fn(&str) -> bool| {
            s.check_environment(&fonts, has).into_iter().map(|i| i.path).collect::<Vec<_>>()
        };
        let mut s = ClientSettings::default();
        assert!(paths(&s, &ok).is_empty());
        s.font.family = Some("fira code".into());
        assert!(paths(&s, &ok).is_empty(), "case doesn't matter");
        s.font.family = Some("monospace".into());
        assert!(paths(&s, &ok).is_empty(), "generic names are fine");
        s.font.family = Some("Iosevka".into());
        let issues = s.check_environment(&fonts, ok);
        assert_eq!(issues[0].path, "font.family");
        assert!(issues[0].message.contains("the system picks another font"), "{}", issues[0]);
        assert!(s.check_environment(&[], ok).is_empty(), "no font list: nothing to check against");

        s.font.family = None;
        s.font.fallbacks = vec!["Nope Mono".into()];
        assert_eq!(paths(&s, &ok), ["font.fallbacks"]);
        s.font.fallbacks = vec!["fira code".into()];
        assert_eq!(paths(&s, &ok), ["font.fallbacks"], "fallbacks are matched exactly, like the resolver does");
        s.font.fallbacks = vec!["Fira Code".into()];
        assert!(paths(&s, &ok).is_empty());

        s.font.ui_family = Some("Nope Sans".into());
        assert_eq!(paths(&s, &ok), ["font.ui_family"]);
        s.font.ui_family = Some("sans-serif".into());
        assert!(paths(&s, &ok).is_empty());
        s.font.ui_family = None;
        s.font.ui_fallbacks = vec!["Nope Sans".into()];
        assert_eq!(paths(&s, &ok), ["font.ui_fallbacks"]);

        let mut s = ClientSettings::default();
        s.links.file_opener = Some("'my editor' --goto {path}".into());
        assert_eq!(paths(&s, &|p| p != "my editor"), ["links.file_opener"]);
        assert!(paths(&s, &ok).is_empty());
    }

    #[test]
    fn env_keys_name_real_settings() {
        for (var, keys) in ClientSettings::ENV_KEYS {
            for k in *keys {
                assert!(crate::store::is_known_key::<ClientSettings>(k), "{var}: {k}");
            }
        }
        assert!(ClientSettings::env_overrides(|k| (k == "GOATTY_THEME").then(String::new)).is_empty(), "an empty value is ignored");
        let vars = [("GOATTY_FONT_SIZE", "huge"), ("GOATTY_RENDER", "glyphs"), ("GOATTY_THEME", "nord"), ("GOATTY_STATS", "")];
        let var = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert_eq!(ClientSettings::env_overrides(var), ["GOATTY_RENDER", "GOATTY_THEME", "GOATTY_STATS"]);
    }

    #[test]
    fn validation_clamps_and_reports() {
        let mut s = ClientSettings::default();
        s.font.size = 400.0;
        s.terminal.padding = Padding::all(-3.0);
        s.terminal.opacity = f32::NAN;
        s.theme.name = "nope".into();
        s.keybindings.insert("ctrl-x".into(), Some("explode".into()));
        let issues = s.validate();
        assert_eq!(s.font.size, FontSettings::MAX_SIZE);
        assert_eq!(s.terminal.padding, Padding::all(0.0));
        assert_eq!(s.terminal.opacity, TerminalSettings::MIN_OPACITY);
        let paths: Vec<_> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["font.size", "terminal.padding", "terminal.opacity", "keybindings.ctrl-x"]);
        assert!(ClientSettings::default().validate().is_empty());
    }

    #[test]
    fn plugin_keybindings_validation() {
        let mut s = ClientSettings::default();
        s.keybindings.insert("ctrl-alt-e".into(), Some("plugin.docker.exec".into()));
        s.keybindings.insert("ctrl-alt-l".into(), Some("plugin.a-b.x".into()));
        assert!(s.validate().is_empty());

        s.keybindings.insert("ctrl-alt-bad".into(), Some("plugin.d.exec".into()));
        s.keybindings.insert("ctrl-alt-bad2".into(), Some("plugin.docker.Bad".into()));
        let issues = s.validate();
        let paths: Vec<_> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["keybindings.ctrl-alt-bad", "keybindings.ctrl-alt-bad2"]);
    }

    #[test]
    fn shift_right_click_does_the_other_thing() {
        use RightClick::*;
        // (setting, program uses the mouse, shift) → menu?
        for (rc, mouse, shift, menu) in [
            (Menu, false, false, true),
            (Menu, true, false, false),
            (Menu, true, true, true),
            (Always, true, false, true),
            (Always, true, true, false),
            (Always, false, true, true),
            (Paste, false, false, false),
            (Paste, false, true, true),
            (Off, true, false, false),
            (Off, false, true, true),
        ] {
            assert_eq!(rc.opens_menu(mouse, shift), menu, "{rc:?} mouse={mouse} shift={shift}");
        }
        let s: ClientSettings = serde_json::from_str(r#"{"mouse":{"right_click":"paste"}}"#).unwrap();
        assert_eq!(s.mouse.right_click, Paste);
    }

    #[test]
    fn keyboard_word_delete_settings() {
        use crate::store;
        let def = ClientSettings::default();
        assert_eq!(def.keyboard.ctrl_backspace, CtrlBackspace::EscBackspace);
        assert_eq!(def.keyboard.ctrl_delete, CtrlDelete::Standard);

        let l = store::parse::<ClientSettings>(r#"{"keyboard":{"ctrl_backspace":"ctrl_w","ctrl_delete":"alt_d"}}"#);
        assert!(l.issues.is_empty(), "{:?}", l.issues);
        assert_eq!(l.value.keyboard.ctrl_backspace, CtrlBackspace::CtrlW);
        assert_eq!(l.value.keyboard.ctrl_delete, CtrlDelete::AltD);

        let l = store::parse::<ClientSettings>(r#"{"keyboard":{"ctrl_backspace":"bogus"}}"#);
        assert_eq!(l.value.keyboard.ctrl_backspace, CtrlBackspace::EscBackspace);
        assert!(!l.issues.is_empty());
    }
}

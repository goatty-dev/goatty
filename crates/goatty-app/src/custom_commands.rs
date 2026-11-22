//! User-defined custom commands from `settings.json` (`commands.custom`, #242).
//!
//! Custom commands live in client settings alongside `keybindings`, appear in
//! the Command Palette and Settings → Shortcuts, and work on whichever host
//! the active tab is connected to (local or remote `goattyd`, or `--local`):
//!
//! * **Run a program/command**: in a split (`split_right`, `split_down`,
//!   `split_left`, `split_up`) or a new `tab` (launched directly on the host's
//!   managed `PATH` in the focused pane's working directory, closing cleanly
//!   when the program exits), or in the `current` pane (typed at an idle
//!   shell prompt).
//! * **Send keystrokes**: encode named keys (`escape`, `enter`, `tab`,
//!   `ctrl-c`, …) and literal text into the focused pane's terminal using its
//!   active encoding mode (legacy xterm or kitty keyboard protocol), even
//!   while a TUI or foreground program is running.
//! * **Optional `when_program` filter**: limits when the command appears in
//!   the Command Palette and lets bound shortcuts pass through to the terminal
//!   when another program is running.

use goatty_config::keys::{custom_command_action, parse_custom_action};
use goatty_config::{CommandOpenIn, KeyboardSettings, UserCommand};
use goatty_term::TermMode;
use goatty_term::input::{CtrlBackspace, CtrlDelete, Key, KeyEvent, KeyOptions, Mods, encode_key_event_with};
use gpui::{App, SharedString, Window};

use crate::command_palette::registry::{Command, CommandRegistry, Source};
use crate::settings::AppSettings;
use crate::workspace::{LaunchCommand, Workspace};

/// Keep the palette's custom commands in step with `AppSettings`.
pub fn init(cx: &mut App) {
    cx.observe_global::<AppSettings>(sync).detach();
    sync(cx);
}

/// Replace the registry's custom commands with `AppSettings`'s.
pub fn sync(cx: &mut App) {
    let custom = cx.global::<AppSettings>().client.commands.custom.clone();
    let commands: Vec<Command> = custom.iter().map(|(id, cmd)| build_command(id, cmd)).collect();
    let reg = cx.default_global::<CommandRegistry>();
    reg.unregister_custom();
    for c in commands {
        reg.register(c);
    }
}

fn build_command(id: &str, cmd: &UserCommand) -> Command {
    let action_id = custom_command_action(id);
    let run_id = action_id.clone();
    let mut out = Command::handler(&action_id, "Custom", &cmd.title, move |window, cx| {
        run_by_id(&run_id, false, window, cx);
    })
    .with_key_action(action_id);
    let mut words: Vec<&str> = cmd.keywords.iter().map(String::as_str).collect();
    words.push(id);
    if let Some(line) = &cmd.run {
        words.push(line.as_str());
    }
    for p in &cmd.when_program {
        words.push(p.as_str());
    }
    out = out.keywords(&words);
    out.detail = Some(command_detail(cmd).into());
    out.when_program = cmd.when_program.iter().map(|p| SharedString::from(p.clone())).collect();
    out.source = Source::Custom;
    out
}

/// Short muted description for the palette and Settings → Shortcuts.
pub fn command_detail(cmd: &UserCommand) -> String {
    let mut parts = Vec::new();
    if cmd.is_send_keys() {
        let preview = cmd.send_keys.iter().map(|s| pretty_step(s)).collect::<Vec<_>>().join(" ");
        parts.push(format!("send {preview}"));
    } else if let Some(line) = &cmd.run {
        parts.push(line.clone());
        if cmd.open_in != CommandOpenIn::Current {
            parts.push(cmd.open_in.short_label().to_string());
        }
    }
    if !cmd.when_program.is_empty() {
        parts.push(format!("when {}", cmd.when_program.join(", ")));
    }
    parts.join(" · ")
}

/// Whether custom command `action_id` (`custom.<id>`) matches the workspace's
/// currently focused foreground program. Returns `true` for unknown commands
/// (so a toast can explain it) or commands without a `when_program` filter.
pub fn matches_focused_program(action_id: &str, program: Option<&str>, cx: &App) -> bool {
    let Some(id) = parse_custom_action(action_id) else {
        return true;
    };
    let Some(cmd) = cx.global::<AppSettings>().client.commands.custom.get(id) else {
        return true;
    };
    cmd.matches_program(program)
}

/// Run the custom command `action_id` (`custom.<id>`).
/// `from_shortcut`: when true and `when_program` doesn't match, silently do
/// nothing (shortcut dispatch already checks `matches_focused_program` to
/// propagate the keystroke).
pub fn run_by_id(action_id: &str, from_shortcut: bool, window: &mut Window, cx: &mut App) {
    let Some(id) = parse_custom_action(action_id) else {
        return;
    };
    let Some(cmd) = cx.global::<AppSettings>().client.commands.custom.get(id).cloned() else {
        crate::toast::show_in(
            window,
            cx,
            crate::toast::Toast::info("Custom command not found")
                .body(format!("{action_id} is no longer defined in settings.json."))
                .key("custom.command"),
        );
        return;
    };
    if cmd.is_send_keys() {
        run_send_keys(&cmd, from_shortcut, window, cx);
        return;
    }
    let Some(line) = cmd.run.clone() else {
        return;
    };
    if cmd.open_in == CommandOpenIn::Current {
        let line_clone = line.clone();
        window.defer(cx, move |window, cx| {
            if let Some(view) = crate::typing::target_focused(&line_clone, crate::typing::What::Command, window, cx) {
                view.update(cx, |v, cx| v.run_line(&line_clone, cx));
            }
        });
        return;
    }
    let Some(raw_argv) = cmd.argv() else {
        crate::toast::show_in(
            window,
            cx,
            crate::toast::Toast::error(format!("Can't run \"{}\"", cmd.title))
                .body("The command has an unterminated quote.")
                .key("custom.command"),
        );
        return;
    };
    let needs_home = raw_argv.iter().skip(1).any(|a| a == "~" || a.starts_with("~/"));
    let launch = LaunchCommand {
        label: raw_argv[0].clone(),
        open_in: cmd.open_in,
        origin: None,
        needs_home,
        build_argv: Box::new(move |home| Ok(expand_tilde_argv(raw_argv, home))),
        fallback_line: line,
    };
    window.defer(cx, move |window, cx| {
        if let Some(Some(ws)) = window.root::<Workspace>() {
            ws.update(cx, |ws, cx| ws.open_command(launch, window, cx));
        }
    });
}

fn run_send_keys(cmd: &UserCommand, from_shortcut: bool, window: &mut Window, cx: &mut App) {
    let keys = cmd.send_keys.clone();
    let when = cmd.when_program.clone();
    let title = cmd.title.clone();
    window.defer(cx, move |window, cx| {
        let Some(Some(ws)) = window.root::<Workspace>() else { return };
        let Some(view) = ws.read(cx).focused_terminal() else {
            crate::toast::show_in(
                window,
                cx,
                crate::toast::Toast::warning("No terminal pane focused").body(format!("Couldn't run \"{title}\".")).key("custom.command"),
            );
            return;
        };
        if !when.is_empty() {
            let prog = view.read(cx).process().map(str::to_string);
            let dummy = UserCommand { when_program: when.clone(), ..Default::default() };
            if !dummy.matches_program(prog.as_deref()) {
                if !from_shortcut {
                    crate::toast::show_in(
                        window,
                        cx,
                        crate::toast::Toast::info(format!("\"{title}\" only applies in {}", when.join(", "))).key("custom.command"),
                    );
                }
                return;
            }
        }
        let kb = cx.global::<AppSettings>().client.keyboard.clone();
        let mode = view.read(cx).session().terminal.mode();
        let bytes = encode_send_keys(&keys, mode, key_options(&kb));
        if !bytes.is_empty() {
            view.read(cx).session().write(&bytes);
        }
    });
}

fn key_options(kb: &KeyboardSettings) -> KeyOptions {
    KeyOptions {
        ctrl_backspace: match kb.ctrl_backspace {
            goatty_config::CtrlBackspace::EscBackspace => CtrlBackspace::EscBackspace,
            goatty_config::CtrlBackspace::CtrlW => CtrlBackspace::CtrlW,
            goatty_config::CtrlBackspace::CtrlH => CtrlBackspace::CtrlH,
        },
        ctrl_delete: match kb.ctrl_delete {
            goatty_config::CtrlDelete::Standard => CtrlDelete::Standard,
            goatty_config::CtrlDelete::AltD => CtrlDelete::AltD,
        },
    }
}

/// Expand leading `~` or `~/` in arguments (`argv[1..]`) when `home` is known.
pub fn expand_tilde_argv(mut argv: Vec<String>, home: Option<&str>) -> Vec<String> {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return argv;
    };
    let home = home.trim_end_matches('/');
    for arg in argv.iter_mut().skip(1) {
        if arg == "~" {
            *arg = home.to_string();
        } else if let Some(rest) = arg.strip_prefix("~/") {
            *arg = format!("{home}/{rest}");
        }
    }
    argv
}

/// One parsed step of a `send_keys` sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStep {
    /// A named key or modifier combination (encoded with the terminal's mode).
    Key { key: OwnedKey, mods: Mods },
    /// Literal text typed into the terminal.
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedKey {
    Char(String),
    Enter,
    Backspace,
    Tab,
    Escape,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

impl OwnedKey {
    fn as_key(&self) -> Key<'_> {
        match self {
            Self::Char(s) => Key::Text(s.as_str()),
            Self::Enter => Key::Enter,
            Self::Backspace => Key::Backspace,
            Self::Tab => Key::Tab,
            Self::Escape => Key::Escape,
            Self::Space => Key::Space,
            Self::Up => Key::Up,
            Self::Down => Key::Down,
            Self::Left => Key::Left,
            Self::Right => Key::Right,
            Self::Home => Key::Home,
            Self::End => Key::End,
            Self::PageUp => Key::PageUp,
            Self::PageDown => Key::PageDown,
            Self::Insert => Key::Insert,
            Self::Delete => Key::Delete,
            Self::F(n) => Key::F(*n),
        }
    }
}

/// Parse one element of `send_keys` into a [`KeyStep`].
///
/// * `text:<literal>` always sends `<literal>` verbatim (lets users send words
///   like `"enter"` or `"tab"` literally).
/// * Named keys (`escape`/`esc`, `enter`/`return`, `tab`, `backspace`,
///   `delete`/`del`, `space`, `up`, `down`, `left`, `right`, `home`, `end`,
///   `pageup`, `pagedown`, `insert`, `f1`..`f12`) and modifier combos
///   (`ctrl-c`, `ctrl+w`, `alt-x`, `shift-tab`, `ctrl-shift-a`, …) become
///   [`KeyStep::Key`].
/// * Everything else (like `":wq"`, `"dd"`, `"git status"`) is [`KeyStep::Text`].
pub fn parse_send_key(raw: &str) -> KeyStep {
    if let Some(lit) = raw.strip_prefix("text:") {
        return KeyStep::Text(lit.to_string());
    }
    if let Some((key, mods)) = parse_key_combo(raw) {
        return KeyStep::Key { key, mods };
    }
    KeyStep::Text(raw.to_string())
}

fn named_key(lower: &str) -> Option<OwnedKey> {
    Some(match lower {
        "enter" | "return" => OwnedKey::Enter,
        "backspace" | "bs" => OwnedKey::Backspace,
        "tab" => OwnedKey::Tab,
        "escape" | "esc" => OwnedKey::Escape,
        "space" => OwnedKey::Space,
        "up" => OwnedKey::Up,
        "down" => OwnedKey::Down,
        "left" => OwnedKey::Left,
        "right" => OwnedKey::Right,
        "home" => OwnedKey::Home,
        "end" => OwnedKey::End,
        "pageup" | "pgup" => OwnedKey::PageUp,
        "pagedown" | "pgdn" => OwnedKey::PageDown,
        "insert" | "ins" => OwnedKey::Insert,
        "delete" | "del" => OwnedKey::Delete,
        k if k.len() >= 2 && k.starts_with('f') => {
            let n: u8 = k[1..].parse().ok()?;
            if (1..=24).contains(&n) { OwnedKey::F(n) } else { return None }
        }
        _ => return None,
    })
}

fn parse_key_combo(raw: &str) -> Option<(OwnedKey, Mods)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return None;
    }
    let lower = trimmed.to_lowercase();
    if let Some(k) = named_key(&lower) {
        return Some((k, Mods::default()));
    }
    // Split modifier prefix on `-` or `+` (handling trailing `--` or `++`).
    let sep = if lower.contains('+') { '+' } else { '-' };
    let (mods_part, key_part) = if let Some(rest) = lower.strip_suffix(&format!("{sep}{sep}")) {
        (rest, if sep == '+' { "+" } else { "-" })
    } else {
        lower.rsplit_once(sep)?
    };
    if mods_part.is_empty() || key_part.is_empty() {
        return None;
    }
    let mut mods = Mods::default();
    for m in mods_part.split(sep) {
        match m {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "option" | "meta" => mods.alt = true,
            "shift" => mods.shift = true,
            _ => return None,
        }
    }
    if !(mods.ctrl || mods.alt || mods.shift) {
        return None;
    }
    let key = if let Some(k) = named_key(key_part) {
        k
    } else if key_part.chars().count() == 1 {
        let ch = if mods.shift && !mods.ctrl { key_part.to_uppercase() } else { key_part.to_string() };
        OwnedKey::Char(ch)
    } else {
        return None;
    };
    Some((key, mods))
}

/// Encode a `send_keys` sequence into terminal input bytes for `mode`.
pub fn encode_send_keys(steps: &[String], mode: TermMode, opts: KeyOptions) -> Vec<u8> {
    let mut out = Vec::new();
    for raw in steps {
        match parse_send_key(raw) {
            KeyStep::Key { key, mods } => {
                let base = match &key {
                    OwnedKey::Char(s) => s.chars().next().map(|c| c.to_ascii_lowercase()),
                    _ => None,
                };
                let ev = KeyEvent { key: key.as_key(), mods, kind: goatty_term::input::KeyKind::Press, base };
                if let Some(bytes) = encode_key_event_with(&ev, mode, opts) {
                    out.extend_from_slice(&bytes);
                }
            }
            KeyStep::Text(text) => {
                if mode.kitty_flags() != 0 {
                    let mut buf = [0u8; 4];
                    for ch in text.chars() {
                        let s = ch.encode_utf8(&mut buf);
                        let ev = KeyEvent::press(Key::Text(s), Mods::default());
                        if let Some(bytes) = encode_key_event_with(&ev, mode, opts) {
                            out.extend_from_slice(&bytes);
                        }
                    }
                } else {
                    out.extend_from_slice(text.as_bytes());
                }
            }
        }
    }
    out
}

/// Human-friendly label for one `send_keys` step (used in previews and subtitles).
pub fn pretty_step(raw: &str) -> String {
    match parse_send_key(raw) {
        KeyStep::Text(t) => t,
        KeyStep::Key { key, mods } => {
            let mut parts = Vec::new();
            if mods.ctrl {
                parts.push("Ctrl".to_string());
            }
            if mods.alt {
                parts.push("Alt".to_string());
            }
            if mods.shift {
                parts.push("Shift".to_string());
            }
            let k = match key {
                OwnedKey::Char(s) => s.to_uppercase(),
                OwnedKey::Enter => "Enter".into(),
                OwnedKey::Backspace => "Backspace".into(),
                OwnedKey::Tab => "Tab".into(),
                OwnedKey::Escape => "Esc".into(),
                OwnedKey::Space => "Space".into(),
                OwnedKey::Up => "Up".into(),
                OwnedKey::Down => "Down".into(),
                OwnedKey::Left => "Left".into(),
                OwnedKey::Right => "Right".into(),
                OwnedKey::Home => "Home".into(),
                OwnedKey::End => "End".into(),
                OwnedKey::PageUp => "PageUp".into(),
                OwnedKey::PageDown => "PageDown".into(),
                OwnedKey::Insert => "Insert".into(),
                OwnedKey::Delete => "Delete".into(),
                OwnedKey::F(n) => format!("F{n}"),
            };
            parts.push(k);
            parts.join("+")
        }
    }
}

/// Parse comma-separated key steps from the Settings UI input.
///
/// Escapes supported: `\,` (literal comma), `\\` (literal backslash), and
/// `\ ` (literal space preserved at step boundaries). Unescaped whitespace
/// immediately before or after a comma separator is treated as formatting.
pub fn parse_key_steps(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    // Byte length of `cur` up through the last non-whitespace or escaped char.
    let mut trimmed_len = 0;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some(',' | '\\' | ' ')) {
            let escaped = chars.next().unwrap();
            started = true;
            cur.push(escaped);
            trimmed_len = cur.len();
        } else if c == ',' {
            cur.truncate(trimmed_len);
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            started = false;
            trimmed_len = 0;
        } else if !started && c.is_whitespace() {
            // Skip formatting whitespace after a comma or at the start.
        } else {
            started = true;
            cur.push(c);
            if !c.is_whitespace() {
                trimmed_len = cur.len();
            }
        }
    }
    cur.truncate(trimmed_len);
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Format stored `send_keys` steps back into the Settings UI's comma-separated
/// representation, escaping literal commas, backslashes, and leading/trailing
/// spaces so editing never strips literal whitespace.
pub fn format_key_steps(steps: &[String]) -> String {
    steps
        .iter()
        .map(|s| {
            let mut escaped = String::with_capacity(s.len());
            let chars: Vec<char> = s.chars().collect();
            let first_non_ws = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(chars.len());
            let last_non_ws = chars.iter().rposition(|c| !c.is_whitespace());
            for (idx, ch) in chars.into_iter().enumerate() {
                let at_edge = idx < first_non_ws || last_non_ws.is_some_and(|last| idx > last);
                match ch {
                    '\\' => escaped.push_str(r"\\"),
                    ',' => escaped.push_str(r"\,"),
                    ' ' if at_edge => escaped.push_str(r"\ "),
                    _ => escaped.push(ch),
                }
            }
            escaped
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_encodes_send_keys_sequences() {
        assert_eq!(parse_send_key("escape"), KeyStep::Key { key: OwnedKey::Escape, mods: Mods::default() });
        assert_eq!(
            parse_send_key("Ctrl+W"),
            KeyStep::Key { key: OwnedKey::Char("w".into()), mods: Mods { ctrl: true, ..Default::default() } }
        );
        assert_eq!(parse_send_key(":wq"), KeyStep::Text(":wq".into()));
        assert_eq!(parse_send_key("text:enter"), KeyStep::Text("enter".into()));

        let steps = vec!["escape".into(), ":wq".into(), "enter".into()];
        let bytes = encode_send_keys(&steps, TermMode::empty(), KeyOptions::default());
        assert_eq!(bytes, b"\x1b:wq\r");

        let ctrl_c = vec!["ctrl-c".into()];
        assert_eq!(encode_send_keys(&ctrl_c, TermMode::empty(), KeyOptions::default()), b"\x03");

        assert_eq!(pretty_step("escape"), "Esc");
        assert_eq!(pretty_step("ctrl-shift-a"), "Ctrl+Shift+A");
        assert_eq!(pretty_step(":wq"), ":wq");
    }

    #[test]
    fn key_steps_round_trip_with_escaped_commas() {
        let steps = vec!["escape".into(), "a,b".into(), "enter".into()];
        let formatted = format_key_steps(&steps);
        assert_eq!(formatted, r"escape, a\,b, enter");
        assert_eq!(parse_key_steps(&formatted), steps);

        let spaced = vec!["text:git ".into(), "text: status".into(), " ".into(), "enter".into()];
        let formatted_spaced = format_key_steps(&spaced);
        assert_eq!(parse_key_steps(&formatted_spaced), spaced);
    }

    #[test]
    fn expands_tilde_in_arguments_only() {
        let argv = vec!["lazygit".into(), "-p".into(), "~/repo".into(), "~".into(), "plain".into()];
        assert_eq!(expand_tilde_argv(argv, Some("/home/alice/")), ["lazygit", "-p", "/home/alice/repo", "/home/alice", "plain"]);
    }

    #[test]
    fn command_detail_summarizes_placement_and_filter() {
        let cmd =
            UserCommand { title: "Lazygit".into(), run: Some("lazygit".into()), open_in: CommandOpenIn::SplitRight, ..Default::default() };
        assert_eq!(command_detail(&cmd), "lazygit · split right");

        let keys = UserCommand {
            title: "Save and quit".into(),
            send_keys: vec!["escape".into(), ":wq".into(), "enter".into()],
            when_program: vec!["vim".into(), "nvim".into()],
            ..Default::default()
        };
        assert_eq!(command_detail(&keys), "send Esc :wq Enter · when vim, nvim");
    }
}

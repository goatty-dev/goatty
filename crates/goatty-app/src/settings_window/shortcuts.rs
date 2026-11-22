//! The Shortcuts page: every bindable action in a table, grouped or
//! sorted, searchable by name or by keys (typed or pressed), with filters
//! for changed and unbound actions.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

use goatty_config::keys::{self, ACTIONS, GROUPS};
use goatty_config::{CommandOpenIn, CommandSettings, UserCommand, split_command_words};
use gpui::{
    AnyElement, App, Context, Entity, Focusable, FontWeight, IntoElement, KeyDownEvent, MouseButton, ParentElement, SharedString, Styled,
    Subscription, Window, div, prelude::*, px,
};

use super::SettingsWindow;
use super::kit::{self, card, chip, divider, field, modal, opt, search_field};
use crate::command_palette::registry::{CommandRegistry, Source};
use crate::custom_commands::{command_detail, format_key_steps, parse_key_steps, pretty_step};
use crate::plugin_commands;
use crate::settings::AppSettings;
use crate::ui::controls::{
    ButtonKind, Tone, badge, banner, button, button_with, icon_button, icon_button_tinted, keycaps, pretty_keys, segmented, tooltip,
};
use crate::ui::focus::{self, Ring, Scroller};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

const CUSTOM_GROUP: &str = "Custom";
const PLUGINS_GROUP: &str = "Plugins";

const QUICK_KEYS: &[(&str, &str)] =
    &[("Esc", "escape"), ("Enter", "enter"), ("Tab", "tab"), ("Ctrl+C", "ctrl-c"), ("Ctrl+W", "ctrl-w"), ("Ctrl+D", "ctrl-d")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CustomKind {
    Run,
    SendKeys,
}

struct CustomInputs {
    title: Entity<TextInput>,
    run: Entity<TextInput>,
    send_keys: Entity<TextInput>,
    when_program: Entity<TextInput>,
}

struct CustomCommandForm {
    /// `None` adds a new command; `Some(id)` edits `commands.custom[id]`.
    editing: Option<String>,
    kind: CustomKind,
    open_in: CommandOpenIn,
    /// Shortcut originally bound when the modal opened (for diffing on save).
    initial_shortcut: Option<String>,
    /// Shortcut chosen in the modal (canonical keystroke).
    shortcut: Option<String>,
    /// Waiting for a keystroke inside the modal.
    recording: bool,
    /// Conflict note if `shortcut` will be moved from another action.
    shortcut_note: Option<String>,
    /// Validation error from the last Save attempt.
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Changed,
    Unbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// By group, in [`GROUPS`] order (collapsible).
    Group,
    Name,
    Keys,
}

pub struct ShortcutsPage {
    query: Entity<TextInput>,
    filter: Filter,
    sort: Sort,
    collapsed: HashSet<&'static str>,
    /// The next keystroke becomes the search (instead of binding anything).
    pub(super) listening: bool,
    /// A pressed keystroke to search for (canonical).
    pressed: Option<String>,
    /// "Reset all" was clicked once; the second click resets.
    confirm_reset: bool,
    custom_inputs: CustomInputs,
    custom_form: Option<CustomCommandForm>,
    /// The add/edit dialog's frame: Tab stays inside it.
    pub(super) modal_focus: gpui::FocusHandle,
    /// What had focus when the dialog opened.
    opener: Option<gpui::FocusHandle>,
    /// Where focus goes on the next frame after the dialog closes.
    pub(super) restore: Option<gpui::FocusHandle>,
    _subs: Vec<Subscription>,
}

impl ShortcutsPage {
    pub fn new(cx: &mut Context<'_, SettingsWindow>) -> Self {
        let query = cx.new(|cx| TextInput::new("Search by name or keys, like ctrl+t", "", cx).bare());
        let custom_inputs = CustomInputs {
            title: cx.new(|cx| TextInput::new("Lazygit", "", cx).label("Command title")),
            run: cx.new(|cx| TextInput::new("lazygit", "", cx).label("Command to run")),
            send_keys: cx.new(|cx| TextInput::new("escape, :wq, enter", "", cx).label("Keys to send")),
            when_program: cx.new(|cx| TextInput::new("Optional: vim, nvim", "", cx).label("Only when program runs")),
        };
        let mut subs = vec![cx.subscribe(&query, |v, _, _: &TextInputEvent, cx| {
            v.shortcuts.pressed = None;
            v.sync_recording(cx);
            cx.notify();
        })];
        for input in [&custom_inputs.title, &custom_inputs.run, &custom_inputs.send_keys, &custom_inputs.when_program] {
            subs.push(cx.subscribe(input, |v, _, ev: &TextInputEvent, cx| match ev {
                TextInputEvent::Changed(_) => {
                    if let Some(form) = &mut v.shortcuts.custom_form {
                        form.error = None;
                    }
                    cx.notify();
                }
                TextInputEvent::Submitted(_) => v.save_custom_command(cx),
            }));
        }
        Self {
            query,
            filter: Filter::All,
            sort: Sort::Group,
            collapsed: HashSet::new(),
            listening: false,
            pressed: None,
            confirm_reset: false,
            custom_inputs,
            custom_form: None,
            modal_focus: cx.focus_handle(),
            opener: None,
            restore: None,
            _subs: subs,
        }
    }

    pub(super) fn is_modal_recording(&self) -> bool {
        self.custom_form.as_ref().is_some_and(|f| f.recording)
    }
}

/// One action as the table shows it.
struct Line {
    name: Cow<'static, str>,
    label: Cow<'static, str>,
    group: &'static str,
    keys: Vec<String>,
    changed: bool,
    available: bool,
    /// Muted subtitle (custom commands' command/keys/filter summary).
    detail: Option<String>,
    /// Raw id inside `commands.custom`, for Edit / Delete buttons.
    custom_id: Option<String>,
    /// For plugin commands: `Some(user_override)` (so the row can show its
    /// placement picker).
    plugin_open_in: Option<Option<CommandOpenIn>>,
}

/// `query` typed as keys: `Ctrl+Shift+T`, `ctrl t` → `ctrl-shift-t`, `ctrl-t`.
fn as_keys(query: &str) -> String {
    query.trim().to_lowercase().replace(['+', ' '], "-")
}

/// Whether `line` matches the search: every word in the name/detail, or the keys.
fn matches(query: &str, line: &Line) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    let detail = line.detail.as_deref().unwrap_or("");
    let label = format!("{} {} {} {} {}", line.label, line.name, line.name.replace(['_', '.'], " "), line.group, detail).to_lowercase();
    if q.split_whitespace().all(|w| label.contains(w)) {
        return true;
    }
    let k = as_keys(&q);
    line.keys.iter().any(|key| key.contains(&k) || pretty_keys(key).to_lowercase().contains(&q))
}

#[allow(clippy::too_many_arguments)]
fn is_row_visible(
    action: &str,
    overrides: &BTreeMap<String, Option<String>>,
    commands: &CommandSettings,
    registry: Option<&CommandRegistry>,
    filter: Filter,
    sort: Sort,
    collapsed: &HashSet<&'static str>,
    query: &str,
    pressed: Option<&str>,
) -> bool {
    let all = lines_with_commands(overrides, commands, registry);
    let Some(line) = all.iter().find(|l| l.name == action) else {
        return false;
    };
    if !line_matches_view(line, filter, query, pressed) {
        return false;
    }
    if sort == Sort::Group {
        let searching = !query.is_empty() || pressed.is_some();
        if !searching && collapsed.contains(line.group) {
            return false;
        }
    }
    true
}

fn action_is_changed(name: &str, overrides: &BTreeMap<String, Option<String>>) -> bool {
    overrides
        .iter()
        .any(|(k, v)| v.as_deref() == Some(name) || (v.is_none() && keys::DEFAULT_BINDINGS.iter().any(|(dk, da)| dk == k && *da == name)))
}

#[cfg(test)]
fn lines(overrides: &BTreeMap<String, Option<String>>, registry: Option<&CommandRegistry>) -> Vec<Line> {
    lines_with_commands(overrides, &CommandSettings::default(), registry)
}

fn lines_with_commands(
    overrides: &BTreeMap<String, Option<String>>,
    commands: &CommandSettings,
    registry: Option<&CommandRegistry>,
) -> Vec<Line> {
    let mut keys_by_action: HashMap<String, Vec<String>> = HashMap::new();
    for (k, action) in keys::effective_bindings(overrides) {
        keys_by_action.entry(action).or_default().push(k);
    }
    let mut out: Vec<Line> = Vec::new();

    // User-defined custom commands (`commands.custom`).
    for (id, cmd) in &commands.custom {
        let action = keys::custom_command_action(id);
        let keys = keys_by_action.remove(&action).unwrap_or_default();
        let detail = command_detail(cmd);
        out.push(Line {
            changed: !keys.is_empty(),
            name: Cow::Owned(action),
            label: Cow::Owned(cmd.title.clone()),
            group: CUSTOM_GROUP,
            keys,
            available: true,
            detail: (!detail.is_empty()).then_some(detail),
            custom_id: Some(id.clone()),
            plugin_open_in: None,
        });
    }

    // Built-in actions.
    out.extend(ACTIONS.iter().map(|a| Line {
        name: Cow::Borrowed(a.name),
        label: Cow::Borrowed(a.label),
        group: a.group,
        keys: keys_by_action.remove(a.name).unwrap_or_default(),
        changed: action_is_changed(a.name, overrides),
        available: true,
        detail: None,
        custom_id: None,
        plugin_open_in: None,
    }));

    // Active plugin commands on the current host.
    let mut seen = HashSet::new();
    if let Some(reg) = registry {
        for c in reg.commands() {
            if !matches!(c.source, Source::Plugin(_)) {
                continue;
            }
            let Some(action) = c.key_action() else { continue };
            if !seen.insert(action.to_string()) {
                continue;
            }
            let keys = keys_by_action.remove(action).unwrap_or_default();
            let open_in = commands.open_in.get(action).copied();
            out.push(Line {
                name: Cow::Owned(action.to_string()),
                label: Cow::Owned(c.label()),
                group: PLUGINS_GROUP,
                changed: !keys.is_empty() || open_in.is_some(),
                keys,
                available: true,
                detail: None,
                custom_id: None,
                plugin_open_in: Some(open_in),
            });
        }
    }

    // Inactive plugin commands that still have a keybinding or placement override.
    let mut extra: Vec<String> = overrides
        .values()
        .flatten()
        .chain(commands.open_in.keys())
        .filter(|a| keys::parse_plugin_action(a).is_some() && seen.insert((*a).clone()))
        .cloned()
        .collect();
    extra.sort();
    for action in extra {
        let Some((plugin, cmd)) = keys::parse_plugin_action(&action) else { continue };
        let keys = keys_by_action.remove(&action).unwrap_or_default();
        let open_in = commands.open_in.get(&action).copied();
        let changed = !keys.is_empty() || open_in.is_some();
        out.push(Line {
            label: Cow::Owned(format!("{plugin}: {cmd}")),
            name: Cow::Owned(action),
            group: PLUGINS_GROUP,
            keys,
            changed,
            available: false,
            detail: None,
            custom_id: None,
            plugin_open_in: Some(open_in),
        });
    }
    out
}

fn action_label(name: &str, cx: &App) -> String {
    if let Some(a) = keys::find_action(name) {
        return a.label.to_string();
    }
    if let Some(id) = keys::parse_custom_action(name)
        && let Some(cmd) = cx.global::<AppSettings>().client.commands.custom.get(id)
    {
        return cmd.title.clone();
    }
    if let Some(c) = cx.try_global::<CommandRegistry>().and_then(|r| r.get(name)) {
        return c.label();
    }
    if let Some((plugin, cmd)) = keys::parse_plugin_action(name) {
        return format!("{plugin}: {cmd}");
    }
    name.to_string()
}

fn parse_when_programs(raw: &str) -> Vec<String> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Validate the custom command form before saving it to `settings.json`.
fn check_custom_command_form(
    title: &str,
    kind: CustomKind,
    run: &str,
    send_keys_raw: &str,
    open_in: CommandOpenIn,
    when_raw: &str,
) -> Result<UserCommand, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("Title: give this command a name.".into());
    }
    let when_program = parse_when_programs(when_raw);
    match kind {
        CustomKind::Run => {
            let line = run.trim();
            if line.is_empty() {
                return Err("Command: what program or command should this run?".into());
            }
            match split_command_words(line) {
                Some(words) if !words.is_empty() => {}
                Some(_) => return Err("Command: what program or command should this run?".into()),
                None => return Err("Command: unterminated quote.".into()),
            }
            Ok(UserCommand {
                title: title.to_string(),
                run: Some(line.to_string()),
                open_in,
                send_keys: Vec::new(),
                when_program,
                keywords: Vec::new(),
            })
        }
        CustomKind::SendKeys => {
            let steps = parse_key_steps(send_keys_raw);
            if steps.is_empty() {
                return Err("Keys to send: enter at least one key or text step (like escape, :wq, enter).".into());
            }
            Ok(UserCommand {
                title: title.to_string(),
                run: None,
                open_in: CommandOpenIn::Current,
                send_keys: steps,
                when_program,
                keywords: Vec::new(),
            })
        }
    }
}

/// Choose a unique id in `commands.custom` derived from `title`.
fn unique_custom_id(title: &str, existing: &BTreeMap<String, UserCommand>) -> String {
    let base = keys::slugify_command_id(title);
    if !existing.contains_key(&base) {
        return base;
    }
    for n in 2..10_000 {
        let candidate = format!("{base}-{n}");
        if !existing.contains_key(&candidate) {
            return candidate;
        }
    }
    format!("{base}-new")
}

impl SettingsWindow {
    pub(super) fn shortcuts_modal_open(&self) -> bool {
        self.shortcuts.custom_form.is_some()
    }

    pub(super) fn is_recording_shortcut(&self) -> bool {
        self.recording.is_some() || self.shortcuts.listening || self.shortcuts.is_modal_recording()
    }

    /// Cancel an active recording if its row is no longer visible in the table.
    pub(super) fn sync_recording(&mut self, cx: &App) -> bool {
        let Some(id) = self.recording.as_deref() else {
            return false;
        };
        let app = &cx.global::<AppSettings>().file.value;
        let query = self.shortcuts.query.read(cx).text().trim().to_string();
        if !is_row_visible(
            id,
            &app.keybindings,
            &app.commands,
            cx.try_global::<CommandRegistry>(),
            self.shortcuts.filter,
            self.shortcuts.sort,
            &self.shortcuts.collapsed,
            &query,
            self.shortcuts.pressed.as_deref(),
        ) {
            self.recording = None;
            return true;
        }
        false
    }

    /// Show the actions matching `text` (from a settings search result).
    pub(super) fn shortcuts_search(&mut self, text: &str, cx: &mut Context<'_, Self>) {
        self.shortcuts.filter = Filter::All;
        self.shortcuts.pressed = None;
        self.shortcuts.query.update(cx, |i, cx| i.set_text(text.to_owned(), cx));
        self.sync_recording(cx);
    }

    /// Open the add/edit custom command dialog (`editing = None` creates a new one).
    fn open_custom_command_form(&mut self, editing: Option<String>, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.recording = None;
        self.shortcuts.listening = false;
        let app = &cx.global::<AppSettings>().file.value;
        let existing = editing.as_deref().and_then(|id| app.commands.custom.get(id)).cloned();
        let (title, kind, run, send_keys, open_in, when_program, initial_shortcut) = match (&editing, existing) {
            (Some(id), Some(cmd)) => {
                let action = keys::custom_command_action(id);
                let bound = keys::effective_bindings(&app.keybindings).into_iter().find_map(|(k, a)| (a == action).then_some(k));
                let kind = if cmd.is_send_keys() { CustomKind::SendKeys } else { CustomKind::Run };
                (
                    cmd.title,
                    kind,
                    cmd.run.unwrap_or_default(),
                    format_key_steps(&cmd.send_keys),
                    if kind == CustomKind::Run { cmd.open_in } else { CommandOpenIn::SplitRight },
                    cmd.when_program.join(", "),
                    bound,
                )
            }
            _ => (String::new(), CustomKind::Run, String::new(), String::new(), CommandOpenIn::SplitRight, String::new(), None),
        };
        let i = &self.shortcuts.custom_inputs;
        i.title.update(cx, |input, cx| input.set_text(title, cx));
        i.run.update(cx, |input, cx| input.set_text(run, cx));
        i.send_keys.update(cx, |input, cx| input.set_text(send_keys, cx));
        i.when_program.update(cx, |input, cx| input.set_text(when_program, cx));
        self.shortcuts.opener = window.focused(cx);
        window.focus(&i.title.focus_handle(cx), cx);
        self.shortcuts.custom_form = Some(CustomCommandForm {
            editing,
            kind,
            open_in,
            shortcut: initial_shortcut.clone(),
            initial_shortcut,
            recording: false,
            shortcut_note: None,
            error: None,
        });
        cx.notify();
    }

    pub(super) fn close_custom_command_form(&mut self, to_opener: bool, cx: &mut Context<'_, Self>) {
        self.shortcuts.custom_form = None;
        let opener = self.shortcuts.opener.take().filter(|_| to_opener);
        self.shortcuts.restore = Some(opener.unwrap_or_else(|| self.focus.clone()));
        cx.notify();
    }

    fn save_custom_command(&mut self, cx: &mut Context<'_, Self>) {
        let Some(form) = self.shortcuts.custom_form.as_ref() else { return };
        let (editing, kind, open_in, initial_shortcut, shortcut) =
            (form.editing.clone(), form.kind, form.open_in, form.initial_shortcut.clone(), form.shortcut.clone());
        let i = &self.shortcuts.custom_inputs;
        let title = i.title.read(cx).text().to_owned();
        let run = i.run.read(cx).text().to_owned();
        let send_keys = i.send_keys.read(cx).text().to_owned();
        let when_program = i.when_program.read(cx).text().to_owned();
        let mut cmd = match check_custom_command_form(&title, kind, &run, &send_keys, open_in, &when_program) {
            Ok(c) => c,
            Err(e) => {
                if let Some(f) = &mut self.shortcuts.custom_form {
                    f.error = Some(e);
                }
                cx.notify();
                return;
            }
        };
        AppSettings::update(cx, |s| {
            let id = match &editing {
                Some(existing_id) => {
                    // Preserve any keywords hand-edited in settings.json.
                    if let Some(prev) = s.commands.custom.get(existing_id) {
                        cmd.keywords = prev.keywords.clone();
                    }
                    existing_id.clone()
                }
                None => unique_custom_id(&cmd.title, &s.commands.custom),
            };
            let action = keys::custom_command_action(&id);
            s.commands.custom.insert(id, cmd);
            if shortcut != initial_shortcut {
                if let Some(old) = &initial_shortcut
                    && keys::action_for(old, &s.keybindings).as_deref() == Some(action.as_str())
                {
                    keys::set_binding(&mut s.keybindings, old, None);
                }
                if let Some(new_key) = &shortcut {
                    keys::set_binding(&mut s.keybindings, new_key, Some(&action));
                }
            }
        });
        self.close_custom_command_form(false, cx);
    }

    fn delete_custom_command(&mut self, id: &str, cx: &mut Context<'_, Self>) {
        if self.shortcuts.custom_form.as_ref().is_some_and(|f| f.editing.as_deref() == Some(id)) {
            self.close_custom_command_form(false, cx);
        }
        let action = keys::custom_command_action(id);
        let id = id.to_string();
        AppSettings::update(cx, |s| {
            s.commands.custom.remove(&id);
            keys::reset_action(&mut s.keybindings, &action);
        });
    }

    /// Take a keystroke for the shortcut being recorded, or as the key
    /// search. Returns whether it was used.
    pub(super) fn record_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<'_, Self>) -> bool {
        let modal_recording = self.shortcuts.is_modal_recording();
        let listening = self.shortcuts.listening;
        if !modal_recording && self.recording.is_none() && !listening {
            return false;
        }
        let k = &ev.keystroke;
        if k.key == "escape" && !k.modifiers.modified() {
            if modal_recording {
                if let Some(form) = &mut self.shortcuts.custom_form {
                    form.recording = false;
                }
            } else {
                self.recording = None;
                self.shortcuts.listening = false;
            }
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if matches!(k.key.as_str(), "shift" | "control" | "alt" | "platform" | "function") {
            return true; // wait for the actual key
        }
        let mut parts = Vec::new();
        for (on, name) in [
            (k.modifiers.control, "ctrl"),
            (k.modifiers.alt, "alt"),
            (k.modifiers.shift, "shift"),
            (k.modifiers.platform, "super"),
            (k.modifiers.function, "fn"),
        ] {
            if on {
                parts.push(name);
            }
        }
        parts.push(k.key.as_str());
        let keystroke = keys::normalize_keystroke(&parts.join("-"));
        cx.stop_propagation();
        if modal_recording {
            let overrides = &cx.global::<AppSettings>().file.value.keybindings;
            let self_action = self.shortcuts.custom_form.as_ref().and_then(|f| f.editing.as_deref()).map(keys::custom_command_action);
            let note = keys::action_for(&keystroke, overrides).filter(|a| self_action.as_deref() != Some(a.as_str())).map(|prev| {
                let label = action_label(&prev, cx);
                format!("{} is currently bound to \"{label}\" and will be moved here on save.", pretty_keys(&keystroke))
            });
            if let Some(form) = &mut self.shortcuts.custom_form {
                form.recording = false;
                form.shortcut = Some(keystroke);
                form.shortcut_note = note;
            }
            cx.notify();
            return true;
        }
        if listening {
            self.shortcuts.listening = false;
            self.shortcuts.query.update(cx, |i, cx| i.set_text(pretty_keys(&keystroke), cx));
            self.shortcuts.pressed = Some(keystroke);
            cx.notify();
            return true;
        }
        let Some(action) = self.recording.take() else { return true };
        let overrides = &cx.global::<AppSettings>().file.value.keybindings;
        // One keystroke runs one action: say which one this took it from.
        self.shortcut_note = keys::action_for(&keystroke, overrides).filter(|a| a != &action).map(|prev| {
            let label = action_label(&prev, cx);
            (action.clone(), format!("{} was bound to \"{label}\"; it does this now.", pretty_keys(&keystroke)))
        });
        AppSettings::update(cx, |s| keys::set_binding(&mut s.keybindings, &keystroke, Some(&action)));
        cx.notify();
        true
    }

    fn shortcut_focus_target(&self, action: &str, edit: ShortcutEdit<'_>, cx: &App) -> gpui::ElementId {
        let app = &cx.global::<AppSettings>().file.value;
        let query = self.shortcuts.query.read(cx).text().trim().to_string();
        focus_target_after_edit(
            action,
            edit,
            &app.keybindings,
            &app.commands,
            cx.try_global::<CommandRegistry>(),
            self.shortcuts.filter,
            self.shortcuts.sort,
            &self.shortcuts.collapsed,
            &query,
            self.shortcuts.pressed.as_deref(),
        )
    }

    fn shortcut_row(&self, ui: Ui, line: &Line, cx: &Context<'_, Self>) -> AnyElement {
        let app = cx.global::<AppSettings>();
        let name = line.name.as_ref().to_owned();
        let label_text: SharedString = line.label.as_ref().to_owned().into();
        let recording = self.recording.as_deref() == Some(name.as_str());
        // Problems per keystroke: unreadable ones, and keys the user added
        // that the shell would otherwise get (the defaults are curated).
        let mut warnings = Vec::new();
        let problem: Vec<bool> = line
            .keys
            .iter()
            .map(|k| {
                let gpui_error = format!("keybindings.{k}: ");
                let broken = match keys::canonical_keystroke(k) {
                    Err(e) => Some(e),
                    Ok(_) => app.key_errors.iter().find_map(|e| e.strip_prefix(&gpui_error)).map(str::to_owned),
                };
                if let Some(e) = broken {
                    warnings.push(format!("{} doesn't work: {e}", pretty_keys(k)));
                    return true;
                }
                if !keys::is_default(k, &name)
                    && let Some(c) = keys::shell_conflict(k)
                {
                    warnings.push(format!("{}: {c}. Bound here, the shell no longer gets it.", pretty_keys(k)));
                    return true;
                }
                false
            })
            .collect();
        if let Some((for_action, msg)) = &self.shortcut_note
            && for_action == &name
        {
            warnings.insert(0, msg.clone());
        }
        let group_id = SharedString::from(format!("sc-{name}"));
        // A button that removes itself hands focus back to the row it was in,
        // or to a surviving neighbor when clearing the binding removes the row.
        let row_elem = row_elem_id(&name);
        let page_focus = self.focus.clone();
        let this = cx.entity().downgrade();
        let chips = div().flex().flex_wrap().items_center().gap(px(8.)).children(line.keys.iter().enumerate().map(|(ki, k)| {
            let key = k.clone();
            let (row_elem, page_focus, this, rm_name) = (row_elem.clone(), page_focus.clone(), this.clone(), name.clone());
            let gid = SharedString::from(format!("sc-key-{name}-{ki}"));
            let rm_id: gpui::ElementId = SharedString::from(format!("sc-rm-{name}-{ki}")).into();
            let rm_focus = rm_id.clone();
            // Hidden until the key is hovered or focused, but a tab stop all along (an
            // `invisible()` one isn't), so any binding can be removed from the keyboard.
            let remove = div()
                .id(rm_id.clone())
                .role(gpui::Role::Button)
                .aria_label(SharedString::from(format!("Remove {} from {}", pretty_keys(k), line.label)))
                .size(px(18.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .opacity(0.)
                .group_hover(gid.clone(), |s| s.opacity(1.))
                .in_focus(|s| s.opacity(1.))
                .cursor_pointer()
                .hover(|s| s.bg(ui.danger.opacity(0.15)))
                .child(Icon::Close.svg().size(px(12.)).text_color(ui.muted))
                .tooltip(tooltip("Remove this shortcut"))
                .on_click(move |_, window, cx| {
                    if focus::is_focused(&rm_focus, window) {
                        let target = this
                            .update(cx, |v, cx| v.shortcut_focus_target(&rm_name, ShortcutEdit::Remove(&key), cx))
                            .unwrap_or_else(|_| row_elem.clone());
                        focus::hand_over(&page_focus, focus::Handover::Control(target), window, cx);
                    }
                    AppSettings::update(cx, |s| keys::set_binding(&mut s.keybindings, &key, None));
                });
            div()
                .id(gid.clone())
                .group(gid)
                .relative()
                .flex()
                .items_center()
                .gap(px(4.))
                .when(problem[ki], |d| d.p(px(2.)).rounded(px(8.)).border_1().border_color(ui.warning))
                .child(keycaps(k, ui))
                .child(focus::tab(remove, &rm_id, ui, Ring::Outer, Scroller::Page))
        }));
        let cell = if recording {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(28.))
                .px(px(12.))
                .rounded_full()
                .border_1()
                .border_color(ui.accent)
                .bg(ui.accent_soft)
                .text_size(px(text::SMALL))
                .text_color(ui.accent_text)
                .child(Icon::Keyboard.svg().size(px(14.)).text_color(ui.accent_text))
                .child("Press the new keys · Esc cancels")
                .into_any_element()
        } else if line.keys.is_empty() {
            div().text_size(px(text::SMALL)).text_color(ui.faint).child("Not set").into_any_element()
        } else {
            chips.into_any_element()
        };
        let is_custom = line.custom_id.is_some();
        let actions = div()
            .w(px(96.))
            .flex_shrink_0()
            .flex()
            .justify_end()
            .gap(px(2.))
            .when_some(line.custom_id.clone(), |d, cid| {
                let (row_elem, page_focus, del_action, this) = (row_elem.clone(), page_focus.clone(), name.clone(), this.clone());
                let edit_id = cid.clone();
                let del_id = cid.clone();
                let del_btn_id = SharedString::from(format!("sc-del-{name}"));
                let del_focus: gpui::ElementId = del_btn_id.clone().into();
                d.child(
                    icon_button(
                        SharedString::from(format!("sc-edit-{name}")),
                        Icon::Pencil,
                        format!("Edit {}", line.label),
                        ui,
                        self.act_window(cx, move |v, window, cx| v.open_custom_command_form(Some(edit_id.clone()), window, cx)),
                    )
                    .opacity(0.)
                    .group_hover(group_id.clone(), |s| s.opacity(1.))
                    .in_focus(|s| s.opacity(1.)),
                )
                .child(
                    icon_button(del_btn_id, Icon::Trash, format!("Delete {}", line.label), ui, move |window, cx| {
                        if focus::is_focused(&del_focus, window) {
                            let target = this
                                .update(cx, |v, cx| v.shortcut_focus_target(&del_action, ShortcutEdit::DeleteCustom, cx))
                                .unwrap_or_else(|_| row_elem.clone());
                            focus::hand_over(&page_focus, focus::Handover::Control(target), window, cx);
                        }
                        let _ = this.update(cx, |v, cx| v.delete_custom_command(&del_id, cx));
                    })
                    .opacity(0.)
                    .group_hover(group_id.clone(), |s| s.opacity(1.))
                    .in_focus(|s| s.opacity(1.)),
                )
            })
            .when(line.changed && !is_custom, |d| {
                let (row_elem, page_focus, reset_name, this) = (row_elem.clone(), page_focus.clone(), name.clone(), this.clone());
                let reset_id = SharedString::from(format!("sc-reset-{name}"));
                let reset_focus: gpui::ElementId = reset_id.clone().into();
                d.child(icon_button(reset_id, Icon::RotateCcw, "Reset to the default", ui, move |window, cx| {
                    // The button goes away once the row is back to its default.
                    if focus::is_focused(&reset_focus, window) {
                        let target = this
                            .update(cx, |v, cx| v.shortcut_focus_target(&reset_name, ShortcutEdit::Reset, cx))
                            .unwrap_or_else(|_| row_elem.clone());
                        focus::hand_over(&page_focus, focus::Handover::Control(target), window, cx);
                    }
                    AppSettings::update(cx, |s| {
                        keys::reset_action(&mut s.keybindings, &reset_name);
                        s.commands.open_in.remove(&reset_name);
                    });
                }))
            })
            .child(
                icon_button(SharedString::from(format!("sc-add-{name}")), Icon::Plus, "Add a shortcut", ui, {
                    let (rec_name, this) = (name.clone(), this.clone());
                    move |window, cx| {
                        let _ = this.update(cx, |v, cx| {
                            v.recording = Some(rec_name.clone());
                            v.shortcuts.listening = false;
                            v.shortcut_note = None;
                            window.focus(&v.focus, cx);
                            cx.notify();
                        });
                    }
                })
                .when(!line.keys.is_empty() && !line.changed, |d| {
                    // Enter on the row does the same: hidden until hover or focus, still a tab stop.
                    d.opacity(0.).group_hover(group_id.clone(), |s| s.opacity(1.)).in_focus(|s| s.opacity(1.))
                }),
            );
        // Optional placement override picker for plugin commands (#241).
        let plugin_placement = line.plugin_open_in.filter(|ov| line.available || ov.is_some()).map(|override_val| {
            let default_open = plugin_commands::plugin_default_open_in(&name, cx);
            let default_label = match default_open {
                Some(p) => format!("Plugin default ({})", p.label()),
                None => "Plugin default".to_string(),
            };
            let mut opts = vec![opt(None, default_label).desc("Use what the plugin specifies")];
            for p in CommandOpenIn::ALL {
                opts.push(opt(Some(p), p.label()).desc(p.description()));
            }
            let action_key = name.clone();
            let select_id = format!("sc-open-in-{name}");
            let select_name = format!("Where {} opens", line.label);
            let picker = self.select(&select_id, &select_name, opts, &override_val, cx, move |choice, _, cx| {
                AppSettings::update(cx, |s| match choice {
                    Some(p) => {
                        s.commands.open_in.insert(action_key.clone(), p);
                    }
                    None => {
                        s.commands.open_in.remove(&action_key);
                    }
                });
            });
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .pt(px(4.))
                .child(div().text_size(px(text::CAPTION)).text_color(ui.muted).child("Opens in"))
                .child(picker)
        });
        // The row is a tab stop: Enter records a new shortcut, Delete removes the last one.
        let row_id = row_elem;
        let last_key = line.keys.last().cloned();
        let row = div()
            .id(row_id.clone())
            .role(gpui::Role::Row)
            .aria_label(label_text.clone())
            .aria_description("Enter adds a shortcut, Delete removes the last one")
            .on_key_down({
                let (row_id, rec_name, page_focus) = (row_id.clone(), name, page_focus);
                move |ev, window, cx| {
                    // Only the row itself: a button inside it has its own Enter.
                    if ev.keystroke.modifiers.modified() || !focus::is_focused(&row_id, window) {
                        return;
                    }
                    match ev.keystroke.key.as_str() {
                        "enter" => {
                            cx.stop_propagation();
                            let _ = this.update(cx, |v, cx| {
                                v.recording = Some(rec_name.clone());
                                v.shortcuts.listening = false;
                                v.shortcut_note = None;
                                window.focus(&v.focus, cx);
                                cx.notify();
                            });
                        }
                        "delete" | "backspace" => {
                            if let Some(key) = &last_key {
                                cx.stop_propagation();
                                if let Ok(target) =
                                    this.update(cx, |v, cx| v.shortcut_focus_target(&rec_name, ShortcutEdit::Remove(key), cx))
                                    && target != row_id
                                {
                                    focus::hand_over(&page_focus, focus::Handover::Control(target), window, cx);
                                }
                                AppSettings::update(cx, |s| keys::set_binding(&mut s.keybindings, key, None));
                            }
                        }
                        _ => {}
                    }
                }
            })
            .group(group_id.clone())
            .flex()
            .flex_col()
            .px(px(16.))
            .py(px(8.))
            .min_h(px(48.))
            .justify_center()
            .when(recording, |d| d.bg(ui.accent_soft.opacity(0.5)))
            .hover(|s| s.bg(ui.surface_hover))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .flex_1()
                            .min_w(px(140.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().min_w_0().text_size(px(text::BODY)).text_color(ui.text).child(label_text))
                                    .when(line.changed && !is_custom, |d| d.child(badge("Changed", Tone::Accent, ui)))
                                    .when(!line.available, |d| d.child(badge("Unavailable", Tone::Neutral, ui))),
                            )
                            .when_some(line.detail.clone(), |d, detail| {
                                d.child(div().min_w_0().truncate().text_size(px(text::SMALL)).text_color(ui.muted).child(detail))
                            })
                            .children(plugin_placement)
                            .when(!line.available, |d| {
                                d.child(
                                    div()
                                        .text_size(px(text::SMALL))
                                        .text_color(ui.muted)
                                        .child("Not active on this host (plugin not installed or enabled, a setting is off, or its program is missing)."),
                                )
                            }),
                    )
                    .child(div().w(px(240.)).flex_shrink(1.).min_w(px(150.)).child(cell))
                    .child(actions),
            )
            .children((!warnings.is_empty()).then(|| {
                div().flex().flex_col().gap(px(2.)).pt(px(6.)).children(warnings.into_iter().map(|w| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(text::SMALL))
                        .text_color(ui.warning)
                        .child(Icon::TriangleAlert.svg().size(px(13.)).text_color(ui.warning))
                        .child(SharedString::from(w))
                }))
            }));
        focus::tab(row, &row_id, ui, Ring::Inner, Scroller::Page).into_any_element()
    }

    fn column_header(&self, ui: Ui, cx: &Context<'_, Self>) -> impl IntoElement {
        let sort = self.shortcuts.sort;
        let head = |id: &'static str, label: &'static str, mine: Sort| {
            let on = sort == mine;
            let id: gpui::ElementId = id.into();
            let el = div()
                .id(id.clone())
                .role(gpui::Role::Button)
                .aria_label(label)
                .flex()
                .items_center()
                .gap(px(4.))
                .cursor_pointer()
                .text_color(if on { ui.accent_text } else { ui.muted })
                .hover(|s| s.text_color(ui.text))
                .child(label)
                .child(Icon::ArrowUpDown.svg().size(px(12.)).text_color(if on { ui.accent_text } else { ui.faint }))
                .tooltip(tooltip(if on { "Click again to group by area" } else { "Sort by this column" }))
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.shortcuts.sort = if v.shortcuts.sort == mine { Sort::Group } else { mine };
                    cx.notify();
                }));
            focus::tab(el, &id, ui, Ring::Outer, Scroller::Page)
        };
        div()
            .flex()
            .items_center()
            .gap(px(16.))
            .h(px(38.))
            .px(px(16.))
            .text_size(px(text::CAPTION))
            .font_weight(FontWeight::SEMIBOLD)
            .child(div().flex_1().min_w(px(140.)).child(head("sort-name", "ACTION", Sort::Name)))
            .child(div().w(px(240.)).flex_shrink(1.).min_w(px(150.)).child(head("sort-keys", "SHORTCUT", Sort::Keys)))
            .child(div().w(px(96.)))
    }

    pub(super) fn shortcuts_page(&self, ui: Ui, window: &Window, cx: &Context<'_, Self>) -> AnyElement {
        let app = cx.global::<AppSettings>();
        let all = lines_with_commands(&app.file.value.keybindings, &app.file.value.commands, cx.try_global::<CommandRegistry>());
        let st = &self.shortcuts;
        let query = st.query.read(cx).text().trim().to_string();
        let changed_n = all.iter().filter(|l| l.changed).count();
        let unbound_n = all.iter().filter(|l| l.keys.is_empty()).count();
        let shown = visible_rows(&all, st.filter, st.sort, &query, st.pressed.as_deref());

        let listen = icon_button_tinted(
            "sc-listen",
            Icon::Keyboard,
            if st.listening { "Listening: press the keys to find · Esc stops" } else { "Search by pressing keys" },
            st.listening.then_some(ui.on_accent),
            ui,
            self.act(cx, |v, cx| {
                v.shortcuts.listening = !v.shortcuts.listening;
                v.recording = None;
                cx.notify();
            }),
        )
        .size(px(38.))
        .bg(ui.field)
        .when(st.listening, |d| d.bg(ui.accent).hover(|s| s.bg(ui.accent)));
        let add_cmd = button_with(
            "sc-add-custom",
            Some(Icon::Plus),
            "Add command",
            ButtonKind::Primary,
            ui,
            self.act_window(cx, |v, window, cx| v.open_custom_command_form(None, window, cx)),
        )
        .h(px(38.));
        let set_filter = |f: Filter| {
            self.act(cx, move |v, cx| {
                v.shortcuts.filter = f;
                v.sync_recording(cx);
                cx.notify();
            })
        };
        let toolbar = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().child(if st.listening {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(38.))
                            .px(px(14.))
                            .rounded_full()
                            .bg(ui.accent_soft)
                            .border_1()
                            .border_color(ui.accent)
                            .text_size(px(text::BODY))
                            .text_color(ui.accent_text)
                            .child(Icon::Keyboard.svg().size(px(15.)).text_color(ui.accent_text))
                            .child("Press the shortcut you're looking for…")
                            .into_any_element()
                    } else {
                        search_field(ui, "sc-search", &st.query, window, cx).into_any_element()
                    }))
                    .child(listen)
                    .child(add_cmd),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(chip(ui, "sc-all", "All", Some(all.len()), st.filter == Filter::All, set_filter(Filter::All)))
                    .child(chip(ui, "sc-changed", "Changed", Some(changed_n), st.filter == Filter::Changed, set_filter(Filter::Changed)))
                    .child(chip(ui, "sc-unbound", "Unbound", Some(unbound_n), st.filter == Filter::Unbound, set_filter(Filter::Unbound)))
                    .child(div().flex_1())
                    .when(changed_n > 0, |d| {
                        let confirming = st.confirm_reset;
                        d.child(button_with(
                            "sc-reset-all",
                            Some(Icon::RotateCcw),
                            if confirming { "Reset every shortcut?" } else { "Reset all" },
                            if confirming { ButtonKind::Danger } else { ButtonKind::Ghost },
                            ui,
                            self.act(cx, |v, cx| {
                                if v.shortcuts.confirm_reset {
                                    AppSettings::update(cx, |s| {
                                        s.keybindings.clear();
                                        s.commands.open_in.clear();
                                    });
                                }
                                v.shortcuts.confirm_reset = !v.shortcuts.confirm_reset;
                                cx.notify();
                            }),
                        ))
                    }),
            );

        let mut table = card(ui).overflow_hidden().child(self.column_header(ui, cx)).child(div().h(px(1.)).bg(ui.divider));
        if shown.is_empty() {
            let (title, desc) = empty_state_message(&all, st.filter, &query, st.pressed.as_deref());
            table = table.child(kit::empty(ui, Icon::Keyboard, title, desc));
        } else if st.sort == Sort::Group {
            let searching = !query.is_empty() || st.pressed.is_some();
            let groups = grouped_rows(&shown, &st.collapsed, searching);
            for (gi, (g, rows, folded)) in groups.iter().enumerate() {
                if gi > 0 {
                    table = table.child(div().h(px(1.)).bg(ui.divider));
                }
                let g = *g;
                let head_id = group_elem_id(g);
                let head = div()
                    .id(head_id.clone())
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(format!("{g} shortcuts")))
                    .aria_expanded(!folded)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(38.))
                    .px(px(14.))
                    .bg(ui.field.opacity(0.5))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.field))
                    .child((if *folded { Icon::ChevronRight } else { Icon::ChevronDown }).svg().size(px(14.)).text_color(ui.muted))
                    .child(div().text_size(px(text::LABEL)).font_weight(FontWeight::SEMIBOLD).child(g))
                    .child(div().text_size(px(text::CAPTION)).text_color(ui.faint).child(rows.len().to_string()))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        if !v.shortcuts.collapsed.remove(g) {
                            v.shortcuts.collapsed.insert(g);
                        }
                        v.sync_recording(cx);
                        cx.notify();
                    }));
                table = table.child(focus::tab(head, &head_id, ui, Ring::Inner, Scroller::Page));
                if !folded {
                    for (i, l) in rows.iter().enumerate() {
                        if i > 0 {
                            table = table.child(divider(ui));
                        }
                        table = table.child(self.shortcut_row(ui, l, cx));
                    }
                }
            }
        } else {
            for (i, l) in shown.iter().enumerate() {
                if i > 0 {
                    table = table.child(divider(ui));
                }
                table = table.child(self.shortcut_row(ui, l, cx));
            }
        }

        let mut page = div().flex().flex_col().gap(px(16.)).child(toolbar);
        if !app.key_errors.is_empty() {
            page = page.child(banner(ui, Tone::Warning, "Some shortcuts don't work", app.key_errors.clone()));
        }
        let command_issues: Vec<String> = app.issues().into_iter().filter(|i| i.concerns("commands")).map(|i| i.to_string()).collect();
        if !command_issues.is_empty() {
            page = page.child(banner(ui, Tone::Warning, "Some custom commands couldn't be used", command_issues));
        }
        page.child(table)
            .child(kit::hint(ui, "Shortcuts work in terminal panes; keys no action uses go to the shell. Custom commands live in settings.json and run on whichever host the active tab is connected to.").px(px(4.)))
            .into_any_element()
    }

    /// Add/edit custom command dialog (#242), while open.
    pub(super) fn custom_command_modal(&self, ui: Ui, _window: &Window, cx: &Context<'_, Self>) -> Option<AnyElement> {
        let form = self.shortcuts.custom_form.as_ref()?;
        let i = &self.shortcuts.custom_inputs;
        let mono = kit::mono(cx);
        let editing = form.editing.is_some();
        let title = if editing { "Edit custom command" } else { "Add custom command" };

        let kind_control =
            segmented("sc-cmd-kind", &[(CustomKind::Run, "Run program"), (CustomKind::SendKeys, "Send keys")], &form.kind, ui, {
                let this = cx.entity().downgrade();
                move |kind, _, cx| {
                    let _ = this.update(cx, |v, cx| {
                        if let Some(f) = &mut v.shortcuts.custom_form {
                            f.kind = kind;
                            f.error = None;
                            cx.notify();
                        }
                    });
                }
            });

        let action_fields = match form.kind {
            CustomKind::Run => {
                let open_opts: Vec<_> = CommandOpenIn::ALL.iter().map(|&p| opt(p, p.label()).desc(p.description())).collect();
                let open_select = self.select("sc-cmd-open-in", "Opens in", open_opts, &form.open_in, cx, {
                    let this = cx.entity().downgrade();
                    move |choice, _, cx| {
                        let _ = this.update(cx, |v, cx| {
                            if let Some(f) = &mut v.shortcuts.custom_form {
                                f.open_in = choice;
                                cx.notify();
                            }
                        });
                    }
                });
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(field(ui, "Command", div().font_family(mono.clone()).child(i.run.clone())))
                    .child(field(ui, "Opens in", open_select))
                    .into_any_element()
            }
            CustomKind::SendKeys => {
                let steps = parse_key_steps(i.send_keys.read(cx).text());
                let quick_chips =
                    div().flex().flex_wrap().gap(px(6.)).children(QUICK_KEYS.iter().enumerate().map(|(ki, (label, token))| {
                        let input = i.send_keys.clone();
                        let token = (*token).to_string();
                        let chip_id: gpui::ElementId = SharedString::from(format!("sc-cmd-qkey-{ki}")).into();
                        let el = div()
                            .id(chip_id.clone())
                            .role(gpui::Role::Button)
                            .aria_label(SharedString::from(format!("Insert {label}")))
                            .h(px(26.))
                            .px(px(10.))
                            .rounded_full()
                            .bg(ui.field)
                            .border_1()
                            .border_color(ui.border)
                            .flex()
                            .items_center()
                            .text_size(px(text::CAPTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(ui.text)
                            .cursor_pointer()
                            .hover(|s| s.bg(ui.field_hover))
                            .child(*label)
                            .on_click(move |_, window, cx| {
                                input.update(cx, |inp, cx| {
                                    let cur = inp.text().trim_end();
                                    let next = if cur.is_empty() {
                                        token.clone()
                                    } else if cur.ends_with(',') {
                                        format!("{cur} {token}")
                                    } else {
                                        format!("{cur}, {token}")
                                    };
                                    inp.set_text(next, cx);
                                });
                                window.focus(&input.focus_handle(cx), cx);
                            });
                        focus::tab(el, &chip_id, ui, Ring::Outer, Scroller::Page)
                    }));
                let preview = (!steps.is_empty()).then(|| {
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(6.))
                        .p(px(10.))
                        .rounded(px(radius::CONTROL))
                        .bg(ui.bg)
                        .border_1()
                        .border_color(ui.divider)
                        .child(div().text_size(px(text::CAPTION)).text_color(ui.muted).mr(px(4.)).child("Sends"))
                        .children(steps.iter().map(|s| {
                            div()
                                .px(px(8.))
                                .py(px(2.))
                                .rounded(px(6.))
                                .bg(ui.field)
                                .border_1()
                                .border_color(ui.border)
                                .font_family(mono.clone())
                                .text_size(px(text::CAPTION))
                                .text_color(ui.text)
                                .child(SharedString::from(pretty_step(s)))
                        }))
                });
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(field(ui, "Keys to send (comma-separated)", div().font_family(mono.clone()).child(i.send_keys.clone())))
                    .child(quick_chips)
                    .children(preview)
                    .into_any_element()
            }
        };

        let shortcut_row = if form.recording {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(34.))
                .px(px(14.))
                .rounded_full()
                .border_1()
                .border_color(ui.accent)
                .bg(ui.accent_soft)
                .text_size(px(text::SMALL))
                .text_color(ui.accent_text)
                .child(Icon::Keyboard.svg().size(px(14.)).text_color(ui.accent_text))
                .child("Press the shortcut · Esc cancels")
                .into_any_element()
        } else if let Some(key) = &form.shortcut {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(keycaps(key, ui))
                .child(button(
                    "sc-cmd-rec",
                    "Change",
                    ButtonKind::Normal,
                    ui,
                    self.act_window(cx, |v, window, cx| {
                        if let Some(f) = &mut v.shortcuts.custom_form {
                            f.recording = true;
                            window.focus(&v.shortcuts.modal_focus, cx);
                            cx.notify();
                        }
                    }),
                ))
                .child(icon_button(
                    "sc-cmd-clear-key",
                    Icon::Close,
                    "Clear shortcut",
                    ui,
                    self.act(cx, |v, cx| {
                        if let Some(f) = &mut v.shortcuts.custom_form {
                            f.shortcut = None;
                            f.shortcut_note = None;
                            cx.notify();
                        }
                    }),
                ))
                .into_any_element()
        } else {
            button_with(
                "sc-cmd-rec",
                Some(Icon::Keyboard),
                "Record shortcut…",
                ButtonKind::Normal,
                ui,
                self.act_window(cx, |v, window, cx| {
                    if let Some(f) = &mut v.shortcuts.custom_form {
                        f.recording = true;
                        window.focus(&v.shortcuts.modal_focus, cx);
                        cx.notify();
                    }
                }),
            )
            .into_any_element()
        };

        let body = div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(field(ui, "Title", i.title.clone()))
            .child(field(ui, "Action", kind_control))
            .child(action_fields)
            .child(
                div().flex().flex_col().gap(px(4.)).child(field(ui, "Only when program runs (optional)", i.when_program.clone())).child(
                    kit::hint(ui, "Comma-separated program names (e.g. vim, nvim). Shortcuts pass through to the terminal otherwise."),
                ),
            )
            .child(div().flex().flex_col().gap(px(6.)).child(field(ui, "Shortcut (optional)", shortcut_row)).when_some(
                form.shortcut_note.clone(),
                |d, note| {
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(text::SMALL))
                            .text_color(ui.warning)
                            .child(Icon::TriangleAlert.svg().size(px(13.)).text_color(ui.warning))
                            .child(SharedString::from(note)),
                    )
                },
            ))
            .children(form.error.clone().map(|e| banner(ui, Tone::Danger, e, Vec::new())));

        let footer = div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(kit::hint(ui, "Saved in settings.json; programs run on the active tab's host.").flex_1().mr(px(8.)))
            .child(button("sc-cmd-cancel", "Cancel", ButtonKind::Ghost, ui, self.act(cx, |v, cx| v.close_custom_command_form(true, cx))))
            .child(button(
                "sc-cmd-save",
                if editing { "Save" } else { "Add command" },
                ButtonKind::Primary,
                ui,
                self.act(cx, |v, cx| v.save_custom_command(cx)),
            ));

        Some(
            modal(ui, "custom-command-modal", title, body, footer)
                .track_focus(&self.shortcuts.modal_focus)
                .on_mouse_down(MouseButton::Left, cx.listener(|v, _, _, cx| v.close_custom_command_form(true, cx)))
                .into_any_element(),
        )
    }
}

#[derive(Clone, Copy)]
enum ShortcutEdit<'a> {
    Remove(&'a str),
    Reset,
    DeleteCustom,
}

fn row_elem_id(name: &str) -> gpui::ElementId {
    SharedString::from(format!("sc-{name}")).into()
}

fn group_elem_id(group: &str) -> gpui::ElementId {
    SharedString::from(format!("sc-group-{group}")).into()
}

fn line_matches_view(line: &Line, filter: Filter, query: &str, pressed: Option<&str>) -> bool {
    let passes_filter = match filter {
        Filter::All => true,
        Filter::Changed => line.changed,
        Filter::Unbound => line.keys.is_empty(),
    };
    let passes_search = match pressed {
        Some(k) => line.keys.iter().any(|key| key == k),
        None => matches(query, line),
    };
    passes_filter && passes_search
}

fn visible_rows<'a>(all: &'a [Line], filter: Filter, sort: Sort, query: &str, pressed: Option<&str>) -> Vec<&'a Line> {
    let mut shown: Vec<&Line> = all.iter().filter(|l| line_matches_view(l, filter, query, pressed)).collect();
    match sort {
        Sort::Group => {}
        Sort::Name => shown.sort_by_key(|l| l.label.to_lowercase()),
        Sort::Keys => shown.sort_by_key(|l| (l.keys.is_empty(), l.keys.first().map(|k| pretty_keys(k).to_lowercase()))),
    }
    shown
}

fn grouped_rows<'a>(shown: &[&'a Line], collapsed: &HashSet<&'static str>, searching: bool) -> Vec<(&'static str, Vec<&'a Line>, bool)> {
    std::iter::once(CUSTOM_GROUP)
        .chain(GROUPS.iter().copied())
        .chain(std::iter::once(PLUGINS_GROUP))
        .filter_map(|g| {
            let rows: Vec<&Line> = shown.iter().filter(|l| l.group == g).copied().collect();
            (!rows.is_empty()).then_some((g, rows, !searching && collapsed.contains(g)))
        })
        .collect()
}

fn empty_state_message(all: &[Line], filter: Filter, query: &str, pressed: Option<&str>) -> (String, String) {
    match (pressed, filter) {
        (Some(k), _) => {
            if let Some(owner) = all.iter().find(|l| l.keys.iter().any(|key| key == k)) {
                (format!("{} is used by \"{}\"", pretty_keys(k), owner.label), "Switch the filter to All to see it.".to_string())
            } else {
                (format!("Nothing uses {}", pretty_keys(k)), "That key goes to the shell. Add it to an action with +.".to_string())
            }
        }
        (None, Filter::Unbound) if query.is_empty() => ("Every action has a shortcut".into(), "Remove one to see it here.".into()),
        (None, Filter::Changed) if query.is_empty() => ("No changes yet".into(), "Every shortcut is goatty's default.".into()),
        _ => ("No matching shortcuts".into(), "Try another name or key, or clear the filter.".into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn focus_target_after_edit(
    action: &str,
    edit: ShortcutEdit<'_>,
    overrides: &BTreeMap<String, Option<String>>,
    commands: &CommandSettings,
    registry: Option<&CommandRegistry>,
    filter: Filter,
    sort: Sort,
    collapsed: &HashSet<&'static str>,
    query: &str,
    pressed: Option<&str>,
) -> gpui::ElementId {
    let pre_all = lines_with_commands(overrides, commands, registry);
    let pre_shown = visible_rows(&pre_all, filter, sort, query, pressed);

    let mut after_keys = overrides.clone();
    let mut after_cmds = commands.clone();
    match edit {
        ShortcutEdit::Remove(key) => keys::set_binding(&mut after_keys, key, None),
        ShortcutEdit::Reset => {
            keys::reset_action(&mut after_keys, action);
            after_cmds.open_in.remove(action);
        }
        ShortcutEdit::DeleteCustom => {
            if let Some(id) = keys::parse_custom_action(action) {
                after_cmds.custom.remove(id);
            }
            keys::reset_action(&mut after_keys, action);
        }
    }
    let post_all = lines_with_commands(&after_keys, &after_cmds, registry);
    let post_shown = visible_rows(&post_all, filter, sort, query, pressed);
    if post_shown.iter().any(|l| l.name == action) {
        return row_elem_id(action);
    }

    let searching = !query.is_empty() || pressed.is_some();
    let surviving: HashSet<&str> = post_shown.iter().map(|l| l.name.as_ref()).collect();
    let fallback = if sort == Sort::Group {
        let pre_groups = grouped_rows(&pre_shown, collapsed, searching);
        pre_groups
            .iter()
            .enumerate()
            .find_map(|(gi, (_, rows, _))| {
                rows.iter().position(|l| l.name == action).and_then(|ri| grouped_removal_fallback(&pre_groups, gi, ri, &surviving))
            })
            .or_else(|| {
                grouped_rows(&post_shown, collapsed, searching).into_iter().next().map(|(g, rows, folded)| {
                    if !folded && let Some(first) = rows.first() { row_elem_id(&first.name) } else { group_elem_id(g) }
                })
            })
    } else {
        pre_shown
            .iter()
            .position(|l| l.name == action)
            .and_then(|i| flat_removal_fallback(&pre_shown, i, &surviving))
            .or_else(|| post_shown.first().map(|l| row_elem_id(&l.name)))
    };
    fallback.unwrap_or_else(|| SharedString::from("sc-all").into())
}

fn grouped_removal_fallback(
    groups: &[(&'static str, Vec<&Line>, bool)],
    gi: usize,
    ri: usize,
    surviving: &HashSet<&str>,
) -> Option<gpui::ElementId> {
    let rows = &groups[gi].1;
    if let Some(next) = rows[ri + 1..].iter().find(|l| surviving.contains(l.name.as_ref())) {
        return Some(row_elem_id(&next.name));
    }
    if let Some(prev) = rows[..ri].iter().rev().find(|l| surviving.contains(l.name.as_ref())) {
        return Some(row_elem_id(&prev.name));
    }
    if let Some((next_g, _, _)) = groups[gi + 1..].iter().find(|(_, r, _)| r.iter().any(|l| surviving.contains(l.name.as_ref()))) {
        return Some(group_elem_id(next_g));
    }
    if let Some((prev_g, prev_rows, prev_folded)) =
        groups[..gi].iter().rev().find(|(_, r, _)| r.iter().any(|l| surviving.contains(l.name.as_ref())))
    {
        if !prev_folded && let Some(last) = prev_rows.iter().rev().find(|l| surviving.contains(l.name.as_ref())) {
            return Some(row_elem_id(&last.name));
        }
        return Some(group_elem_id(prev_g));
    }
    None
}

fn flat_removal_fallback(shown: &[&Line], i: usize, surviving: &HashSet<&str>) -> Option<gpui::ElementId> {
    if let Some(next) = shown[i + 1..].iter().find(|l| surviving.contains(l.name.as_ref())) {
        return Some(row_elem_id(&next.name));
    }
    if let Some(prev) = shown[..i].iter().rev().find(|l| surviving.contains(l.name.as_ref())) {
        return Some(row_elem_id(&prev.name));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_palette::Command;

    #[allow(clippy::too_many_arguments)]
    fn is_row_visible_no_cmds(
        action: &str,
        overrides: &BTreeMap<String, Option<String>>,
        registry: Option<&CommandRegistry>,
        filter: Filter,
        sort: Sort,
        collapsed: &HashSet<&'static str>,
        query: &str,
        pressed: Option<&str>,
    ) -> bool {
        is_row_visible(action, overrides, &CommandSettings::default(), registry, filter, sort, collapsed, query, pressed)
    }

    #[allow(clippy::too_many_arguments)]
    fn focus_target_no_cmds(
        action: &str,
        edit: ShortcutEdit<'_>,
        overrides: &BTreeMap<String, Option<String>>,
        registry: Option<&CommandRegistry>,
        filter: Filter,
        sort: Sort,
        collapsed: &HashSet<&'static str>,
        query: &str,
        pressed: Option<&str>,
    ) -> gpui::ElementId {
        focus_target_after_edit(action, edit, overrides, &CommandSettings::default(), registry, filter, sort, collapsed, query, pressed)
    }

    #[test]
    fn search_by_name_or_keys() {
        let a = keys::find_action("new_tab").unwrap();
        let l = Line {
            name: Cow::Borrowed(a.name),
            label: Cow::Borrowed(a.label),
            group: a.group,
            keys: vec!["ctrl-shift-t".into()],
            changed: false,
            available: true,
            detail: None,
            custom_id: None,
            plugin_open_in: None,
        };
        for q in ["", "new tab", "tab new", "TAB", "ctrl+shift+t", "Ctrl+Shift+T", "shift-t", "tabs"] {
            assert!(matches(q, &l), "{q:?}");
        }
        assert!(!matches("paste", &l));
        assert!(!matches("ctrl+alt", &l));
    }

    #[test]
    fn plugin_commands_and_inactive_bound_overrides_appear_under_plugins() {
        let mut reg = CommandRegistry::default();
        let mut active = Command::handler("plugin.docker.exec", "Docker", "Open a shell in a container", |_, _| {})
            .with_key_action("plugin.docker.exec");
        active.source = Source::Plugin("docker".into());
        reg.register(active);

        let overrides = BTreeMap::from([
            ("ctrl-alt-e".to_string(), Some("plugin.docker.exec".to_string())),
            ("ctrl-alt-g".to_string(), Some("plugin.git.status".to_string())),
        ]);
        let all = lines(&overrides, Some(&reg));
        let plugin_lines: Vec<&Line> = all.iter().filter(|l| l.group == PLUGINS_GROUP).collect();
        assert_eq!(plugin_lines.len(), 2);

        let docker = plugin_lines.iter().find(|l| l.name == "plugin.docker.exec").unwrap();
        assert_eq!(docker.label, "Docker: Open a shell in a container");
        assert_eq!(docker.keys, ["ctrl-alt-e"]);
        assert!(docker.changed);
        assert!(docker.available);
        assert!(matches("docker exec", docker));
        assert!(matches("plugin.docker.exec", docker));
        assert!(matches("plugins", docker));

        let git = plugin_lines.iter().find(|l| l.name == "plugin.git.status").unwrap();
        assert_eq!(git.label, "git: status");
        assert_eq!(git.keys, ["ctrl-alt-g"]);
        assert!(git.changed);
        assert!(!git.available);
        assert!(matches("plugin.git.status", git));

        let empty_collapsed = HashSet::new();
        assert!(is_row_visible_no_cmds("new_tab", &overrides, Some(&reg), Filter::All, Sort::Group, &empty_collapsed, "", None));
        assert!(is_row_visible_no_cmds(
            "plugin.docker.exec",
            &BTreeMap::new(),
            Some(&reg),
            Filter::All,
            Sort::Group,
            &empty_collapsed,
            "",
            None
        ));
        assert!(is_row_visible_no_cmds("plugin.git.status", &overrides, Some(&reg), Filter::All, Sort::Group, &empty_collapsed, "", None));
        assert!(!is_row_visible_no_cmds(
            "plugin.git.status",
            &BTreeMap::new(),
            Some(&reg),
            Filter::All,
            Sort::Group,
            &empty_collapsed,
            "",
            None
        ));
        assert!(!is_row_visible_no_cmds(
            "plugin.missing.cmd",
            &overrides,
            Some(&reg),
            Filter::All,
            Sort::Group,
            &empty_collapsed,
            "",
            None
        ));

        // Recording visibility respects active search, filter, and group collapse (e.g. when a host swap changes the label).
        assert!(is_row_visible_no_cmds(
            "plugin.docker.exec",
            &overrides,
            Some(&reg),
            Filter::All,
            Sort::Group,
            &empty_collapsed,
            "container",
            None
        ));
        assert!(!is_row_visible_no_cmds(
            "plugin.docker.exec",
            &overrides,
            None,
            Filter::All,
            Sort::Group,
            &empty_collapsed,
            "container",
            None
        ));
        assert!(!is_row_visible_no_cmds(
            "plugin.docker.exec",
            &overrides,
            Some(&reg),
            Filter::Unbound,
            Sort::Group,
            &empty_collapsed,
            "",
            None
        ));
        let plugins_collapsed = HashSet::from([PLUGINS_GROUP]);
        assert!(!is_row_visible_no_cmds(
            "plugin.docker.exec",
            &overrides,
            Some(&reg),
            Filter::All,
            Sort::Group,
            &plugins_collapsed,
            "",
            None
        ));
        assert!(is_row_visible_no_cmds(
            "plugin.docker.exec",
            &overrides,
            Some(&reg),
            Filter::All,
            Sort::Group,
            &plugins_collapsed,
            "docker",
            None
        ));
    }

    #[test]
    fn removal_focus_target_picks_surviving_neighbor() {
        let mut reg = CommandRegistry::default();
        let mut active = Command::handler("plugin.docker.exec", "Docker", "Exec", |_, _| {}).with_key_action("plugin.docker.exec");
        active.source = Source::Plugin("docker".into());
        reg.register(active);

        let overrides = BTreeMap::from([
            ("ctrl-alt-n".to_string(), Some("new_tab".to_string())),
            ("ctrl-alt-e".to_string(), Some("plugin.docker.exec".to_string())),
            ("ctrl-alt-1".to_string(), Some("plugin.alpha.one".to_string())),
            ("ctrl-alt-2".to_string(), Some("plugin.beta.two".to_string())),
            ("ctrl-alt-3".to_string(), Some("plugin.beta.two".to_string())),
        ]);
        let collapsed = HashSet::new();

        // Removing the only binding on an unavailable row drops it to the next surviving neighbor.
        assert_eq!(
            focus_target_no_cmds(
                "plugin.alpha.one",
                ShortcutEdit::Remove("ctrl-alt-1"),
                &overrides,
                Some(&reg),
                Filter::All,
                Sort::Group,
                &collapsed,
                "",
                None,
            ),
            row_elem_id("plugin.beta.two"),
        );
        // Removing one of two bindings keeps the row itself focused.
        assert_eq!(
            focus_target_no_cmds(
                "plugin.beta.two",
                ShortcutEdit::Remove("ctrl-alt-2"),
                &overrides,
                Some(&reg),
                Filter::All,
                Sort::Group,
                &collapsed,
                "",
                None,
            ),
            row_elem_id("plugin.beta.two"),
        );
        // When the only matching plugin row disappears, fall back to `sc-all` if nothing else matches.
        assert_eq!(
            focus_target_no_cmds(
                "plugin.alpha.one",
                ShortcutEdit::Remove("ctrl-alt-1"),
                &overrides,
                Some(&reg),
                Filter::Changed,
                Sort::Group,
                &collapsed,
                "alpha",
                None,
            ),
            gpui::ElementId::from(SharedString::from("sc-all")),
        );

        // Text search matching only the removed/reset binding hides the row; label match keeps it.
        assert_eq!(
            focus_target_no_cmds(
                "plugin.docker.exec",
                ShortcutEdit::Remove("ctrl-alt-e"),
                &overrides,
                Some(&reg),
                Filter::All,
                Sort::Group,
                &collapsed,
                "ctrl-alt-e",
                None,
            ),
            gpui::ElementId::from(SharedString::from("sc-all")),
        );
        assert_eq!(
            focus_target_no_cmds(
                "plugin.docker.exec",
                ShortcutEdit::Reset,
                &overrides,
                Some(&reg),
                Filter::All,
                Sort::Group,
                &collapsed,
                "ctrl-alt-e",
                None,
            ),
            gpui::ElementId::from(SharedString::from("sc-all")),
        );
        assert_eq!(
            focus_target_no_cmds(
                "plugin.docker.exec",
                ShortcutEdit::Remove("ctrl-alt-e"),
                &overrides,
                Some(&reg),
                Filter::All,
                Sort::Group,
                &collapsed,
                "docker",
                None,
            ),
            row_elem_id("plugin.docker.exec"),
        );
        assert_eq!(
            focus_target_no_cmds(
                "plugin.docker.exec",
                ShortcutEdit::Reset,
                &overrides,
                Some(&reg),
                Filter::All,
                Sort::Group,
                &collapsed,
                "docker",
                None,
            ),
            row_elem_id("plugin.docker.exec"),
        );

        // Removing a single custom binding on `new_tab` leaves its default key, so Changed drops it to the next row.
        assert_eq!(
            focus_target_no_cmds(
                "new_tab",
                ShortcutEdit::Remove("ctrl-alt-n"),
                &overrides,
                Some(&reg),
                Filter::Changed,
                Sort::Group,
                &collapsed,
                "",
                None,
            ),
            gpui::ElementId::from(SharedString::from("sc-group-Plugins")),
        );
        // Adding a second custom binding on `new_tab` keeps it in Changed when only one is removed.
        let mut two_custom = overrides.clone();
        two_custom.insert("ctrl-alt-m".to_string(), Some("new_tab".to_string()));
        assert_eq!(
            focus_target_no_cmds(
                "new_tab",
                ShortcutEdit::Remove("ctrl-alt-n"),
                &two_custom,
                Some(&reg),
                Filter::Changed,
                Sort::Group,
                &collapsed,
                "",
                None,
            ),
            row_elem_id("new_tab"),
        );

        // When removing `hyper-k` on Beta also deletes Alpha's `Hyper-K` spelling, skip Alpha and land on the surviving neighbor.
        let shared_spelling = BTreeMap::from([
            ("ctrl-alt-n".to_string(), Some("new_tab".to_string())),
            ("Hyper-K".to_string(), Some("plugin.alpha.one".to_string())),
            ("hyper-k".to_string(), Some("plugin.beta.two".to_string())),
        ]);
        assert_eq!(
            focus_target_no_cmds(
                "plugin.beta.two",
                ShortcutEdit::Remove("hyper-k"),
                &shared_spelling,
                None,
                Filter::Changed,
                Sort::Group,
                &collapsed,
                "",
                None,
            ),
            row_elem_id("new_tab"),
        );

        // Unavailable row kept alive by a raw unparseable/shadowed spelling stays focused in All, but drops out of Changed.
        let unavail_shadowed = BTreeMap::from([
            ("Hyper-K".to_string(), Some("plugin.docker.exec".to_string())),
            ("hyper-k".to_string(), None),
            ("ctrl-alt-e".to_string(), Some("plugin.docker.exec".to_string())),
        ]);
        assert_eq!(
            focus_target_no_cmds(
                "plugin.docker.exec",
                ShortcutEdit::Remove("ctrl-alt-e"),
                &unavail_shadowed,
                None,
                Filter::All,
                Sort::Group,
                &collapsed,
                "Plugins",
                None,
            ),
            row_elem_id("plugin.docker.exec"),
        );
        assert_eq!(
            focus_target_no_cmds(
                "plugin.docker.exec",
                ShortcutEdit::Remove("ctrl-alt-e"),
                &unavail_shadowed,
                None,
                Filter::Changed,
                Sort::Group,
                &collapsed,
                "Plugins",
                None,
            ),
            gpui::ElementId::from(SharedString::from("sc-all")),
        );

        // When Reset restores a default key that matches the pressed search, land on the newly revealed row instead of `sc-all`.
        let hijacked_default = BTreeMap::from([("ctrl-shift-t".to_string(), Some("plugin.docker.exec".to_string()))]);
        assert_eq!(
            focus_target_no_cmds(
                "plugin.docker.exec",
                ShortcutEdit::Reset,
                &hijacked_default,
                Some(&reg),
                Filter::All,
                Sort::Group,
                &collapsed,
                "",
                Some("ctrl-shift-t"),
            ),
            row_elem_id("new_tab"),
        );
        assert_eq!(
            focus_target_no_cmds(
                "plugin.docker.exec",
                ShortcutEdit::Reset,
                &hijacked_default,
                Some(&reg),
                Filter::All,
                Sort::Name,
                &collapsed,
                "",
                Some("ctrl-shift-t"),
            ),
            row_elem_id("new_tab"),
        );

        // Flat sort fallback picks surviving neighbor or `sc-all`.
        let only_alpha = BTreeMap::from([("ctrl-alt-1".to_string(), Some("plugin.alpha.one".to_string()))]);
        assert_eq!(
            focus_target_no_cmds(
                "plugin.alpha.one",
                ShortcutEdit::Remove("ctrl-alt-1"),
                &only_alpha,
                None,
                Filter::Changed,
                Sort::Name,
                &collapsed,
                "",
                None,
            ),
            gpui::ElementId::from(SharedString::from("sc-all")),
        );
    }

    #[test]
    fn custom_commands_and_plugin_open_in_overrides_appear_in_table() {
        let mut commands = CommandSettings::default();
        commands.custom.insert(
            "lazygit".into(),
            UserCommand { title: "Lazygit".into(), run: Some("lazygit".into()), open_in: CommandOpenIn::SplitRight, ..Default::default() },
        );
        commands.open_in.insert("plugin.git.status".into(), CommandOpenIn::Tab);
        let overrides = BTreeMap::from([("ctrl-shift-g".to_string(), Some("custom.lazygit".to_string()))]);

        let all = lines_with_commands(&overrides, &commands, None);
        let custom = all.iter().find(|l| l.name == "custom.lazygit").unwrap();
        assert_eq!(custom.group, CUSTOM_GROUP);
        assert_eq!(custom.label, "Lazygit");
        assert_eq!(custom.keys, ["ctrl-shift-g"]);
        assert_eq!(custom.detail.as_deref(), Some("lazygit · split right"));
        assert!(matches("split right", custom));

        let groups = grouped_rows(&all.iter().collect::<Vec<_>>(), &HashSet::new(), false);
        assert_eq!(groups[0].0, CUSTOM_GROUP);

        let git = all.iter().find(|l| l.name == "plugin.git.status").unwrap();
        assert_eq!(git.group, PLUGINS_GROUP);
        assert!(git.changed);
        assert_eq!(git.plugin_open_in, Some(Some(CommandOpenIn::Tab)));
    }

    #[test]
    fn custom_command_form_validation_and_slug_dedup() {
        assert!(check_custom_command_form("", CustomKind::Run, "lazygit", "", CommandOpenIn::SplitRight, "").is_err());
        assert!(check_custom_command_form("Lazygit", CustomKind::Run, "", "", CommandOpenIn::SplitRight, "").is_err());
        assert!(check_custom_command_form("Broken", CustomKind::Run, "git 'unterminated", "", CommandOpenIn::SplitRight, "").is_err());
        assert!(check_custom_command_form("Save", CustomKind::SendKeys, "", "", CommandOpenIn::Current, "").is_err());

        let run = check_custom_command_form("Lazygit", CustomKind::Run, "lazygit", "", CommandOpenIn::SplitRight, " ").unwrap();
        assert_eq!(run.run.as_deref(), Some("lazygit"));
        assert_eq!(run.open_in, CommandOpenIn::SplitRight);
        assert!(run.when_program.is_empty());

        let keys =
            check_custom_command_form("Save & quit", CustomKind::SendKeys, "", "escape, :wq, enter", CommandOpenIn::Tab, "vim, nvim")
                .unwrap();
        assert_eq!(keys.send_keys, ["escape", ":wq", "enter"]);
        assert_eq!(keys.when_program, ["vim", "nvim"]);
        assert_eq!(keys.open_in, CommandOpenIn::Current);

        let mut existing = BTreeMap::new();
        assert_eq!(unique_custom_id("Lazygit", &existing), "lazygit");
        existing.insert("lazygit".into(), run);
        assert_eq!(unique_custom_id("Lazygit", &existing), "lazygit-2");
    }

    #[test]
    fn empty_state_distinguishes_filtered_shortcuts_from_shell_input() {
        let overrides = BTreeMap::from([("ctrl-alt-e".to_string(), Some("plugin.docker.exec".to_string()))]);
        let all = lines(&overrides, None);

        let (title, desc) = empty_state_message(&all, Filter::Unbound, "", Some("ctrl-alt-e"));
        assert!(title.contains("Ctrl+Alt+E") && title.contains("docker: exec"), "{title}");
        assert!(desc.contains("All"), "{desc}");

        let (title, desc) = empty_state_message(&all, Filter::All, "", Some("ctrl-alt-q"));
        assert!(title.contains("Nothing uses Ctrl+Alt+Q"), "{title}");
        assert!(desc.contains("goes to the shell"), "{desc}");
    }
}

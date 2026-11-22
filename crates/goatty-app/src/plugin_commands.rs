//! Plugins' palette commands (`[[command]]` in `plugin.toml`, #91).
//!
//! The commands come from the plugins installed on the active tab's host
//! (the `HostLinkGlobal`): when that changes, or plugins were installed,
//! removed or switched, the host's catalog is fetched off the UI thread and
//! the registry's plugin commands are replaced. Picking one asks for its
//! inputs in the palette ([`crate::command_palette::input`]) and types its
//! command line (values quoted by `goatty_plugin::commands::command_line`)
//! into the focused pane, where the user sees it run.
//!
//! Commands with a `[command.menu]` are also in the terminal's right-click
//! menu (#186): [`MenuCommands`] lists them for the workspace, and
//! [`start`] asks for their inputs with the selected text filled in.
//!
//! An input with a `[command.input.source]` lists what to pick from (#79):
//! the host runs the plugin's own definition (`Request::CommandSuggest`;
//! the client only names the plugin, command and input) in the focused
//! pane's directory, and the palette lists the results.
//!
//! A command can send keys to the program in the focused pane instead of
//! typing a line (`keys`, #240), and say where it applies (`[command.pane]`:
//! which programs, or at a prompt or not). Elsewhere the palette and the
//! menu leave it out, and a shortcut bound to it goes to the terminal as a
//! normal key ([`passes_through`]).

use std::collections::BTreeMap;
use std::rc::Rc;

use goatty_config::CommandOpenIn;
use goatty_plugin::api::{Installed, Reply, Request};
use goatty_plugin::commands::{
    CommandInput, MenuWhen, OpenIn as PluginOpenIn, PaletteCommand, command_argv, command_line, needs_home_expansion, strokes,
};
use gpui::{App, AsyncApp, Global, Window};

use crate::command_palette::input::Loader;
use crate::command_palette::registry::Source;
use crate::command_palette::{Answers, Ask, Command, CommandRegistry, Field, ask};
use crate::settings::AppSettings;
use crate::settings_window::{HostLink, HostLinkGlobal};

/// Which host the registered plugin commands are from, and the newest fetch.
#[derive(Default)]
struct State {
    link: Option<HostLink>,
    generation: u64,
    /// True while a catalog refresh for the active host is in flight.
    stale: bool,
    active: BTreeMap<String, MenuCommand>,
    /// Active command ids in registry order (for re-registering on placement overrides).
    order: Vec<String>,
    /// Last applied `commands.open_in` overrides from `AppSettings`.
    open_in_overrides: BTreeMap<String, CommandOpenIn>,
    last_issues: Vec<String>,
}

impl Global for State {}

/// A plugin command (palette entry, and optionally right-click menu entry).
#[derive(Clone)]
pub struct MenuCommand {
    /// The plugin's id.
    pub plugin_id: String,
    /// The plugin's name.
    pub plugin: String,
    pub command: Rc<PaletteCommand>,
    pub origin: HostLink,
}

impl MenuCommand {
    pub fn when(&self) -> MenuWhen {
        self.command.menu.as_ref().map(|m| m.when).unwrap_or_default()
    }

    /// Whether it applies to a pane running `process`, `at_prompt` or not
    /// (`[command.pane]`; without one, everywhere).
    pub fn fits(&self, process: Option<&str>, at_prompt: bool) -> bool {
        self.command.pane.as_ref().is_none_or(|p| p.matches(process, at_prompt))
    }

    /// Its icon: the manifest's, else the plugin icon.
    pub fn icon(&self) -> crate::ui::icons::Icon {
        let named = self.command.menu.as_ref().and_then(|m| m.icon.as_deref()).and_then(crate::ui::icons::Icon::by_name);
        named.unwrap_or(crate::ui::icons::Icon::Puzzle)
    }
}

/// The active host's plugin commands for the right-click menu, in the
/// palette's order.
#[derive(Default)]
pub struct MenuCommands(pub Vec<MenuCommand>);

impl Global for MenuCommands {}

/// Keep the palette's plugin commands in step with the active host and placement overrides.
pub fn init(cx: &mut App) {
    cx.observe_global::<HostLinkGlobal>(|cx| {
        let link = cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone());
        let same = match (&cx.default_global::<State>().link, &link) {
            (Some(a), Some(b)) => a.same(b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            refresh(cx);
        }
    })
    .detach();
    cx.observe_global::<AppSettings>(|cx| {
        let overrides = cx.global::<AppSettings>().client.commands.open_in.clone();
        if cx.default_global::<State>().open_in_overrides != overrides {
            rebuild_from_active(overrides, cx);
        }
    })
    .detach();
    refresh(cx);
}

/// Fetch the active host's plugins again (after installing or removing one).
pub fn refresh(cx: &mut App) {
    let link = cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone());
    let state = cx.default_global::<State>();
    state.generation += 1;
    state.stale = true;
    let moved = match (&state.link, &link) {
        (Some(a), Some(b)) => !a.same(b),
        (a, b) => a.is_some() != b.is_some(),
    };
    state.link = link.clone();
    let generation = state.generation;
    // Another host: its plugins aren't the last one's, so none until fetched.
    if moved {
        cx.default_global::<State>().last_issues.clear();
        replace(None, Vec::new(), cx);
        if link.is_some() {
            cx.default_global::<State>().stale = true;
        }
    }
    let Some(link) = link else {
        cx.default_global::<State>().stale = false;
        return;
    };
    cx.spawn(async move |cx: &mut AsyncApp| {
        let l = link.clone();
        let reply = cx
            .background_executor()
            .spawn(async move {
                match l.plugin(Request::Installed) {
                    Err(e) if e.to_string().contains("unknown variant") => l.plugin(Request::Catalog { refresh: false }),
                    other => other,
                }
            })
            .await;
        cx.update(|cx| {
            if cx.default_global::<State>().generation != generation {
                return;
            }
            match reply {
                Ok(Reply::Installed { installed, issues }) => {
                    report_issues(&link, &issues, cx);
                    replace(Some(link), installed, cx);
                }
                Ok(Reply::Catalog(c)) => {
                    report_issues(&link, &c.issues, cx);
                    replace(Some(link), c.installed, cx);
                }
                Ok(_) => {
                    cx.default_global::<State>().stale = false;
                    log::warn!("plugin commands: unexpected reply to an installed plugins request");
                }
                // An older goattyd, or a host without plugins: keep none.
                Err(e) => {
                    log::info!("plugin commands: couldn't list {}'s plugins: {e:#}", link.label());
                    cx.default_global::<State>().last_issues.clear();
                    replace(None, Vec::new(), cx);
                }
            }
        });
    })
    .detach();
}

fn should_report_issues(state: &mut State, issues: &[String]) -> bool {
    if issues.is_empty() {
        state.last_issues.clear();
        return false;
    }
    if state.last_issues == issues {
        return false;
    }
    state.last_issues = issues.to_vec();
    true
}

fn report_issues(link: &HostLink, issues: &[String], cx: &mut App) {
    for issue in issues {
        log::warn!("plugin commands on {}: {issue}", link.label());
    }
    if !should_report_issues(cx.default_global::<State>(), issues) {
        return;
    }
    let body = if issues.len() == 1 { issues[0].clone() } else { format!("{} (and {} more)", issues[0], issues.len() - 1) };
    crate::toast::show(
        cx,
        crate::toast::Toast::warning(format!("Some plugins on {} couldn't be loaded", link.label()))
            .body(body)
            .key("plugin.commands.issues"),
    );
}

/// Map a plugin manifest's `open_in` value to the client settings enum.
pub fn from_plugin_open_in(open_in: PluginOpenIn) -> CommandOpenIn {
    match open_in {
        PluginOpenIn::Current => CommandOpenIn::Current,
        PluginOpenIn::SplitRight => CommandOpenIn::SplitRight,
        PluginOpenIn::SplitDown => CommandOpenIn::SplitDown,
        PluginOpenIn::SplitLeft => CommandOpenIn::SplitLeft,
        PluginOpenIn::SplitUp => CommandOpenIn::SplitUp,
        PluginOpenIn::Tab => CommandOpenIn::Tab,
    }
}

/// Effective placement for plugin command `id` (`plugin.<plugin>.<command>`),
/// preferring any user override in `settings.json` (`commands.open_in`).
/// `keys` commands always send to the focused pane.
pub fn effective_open_in(id: &str, cmd: &PaletteCommand, cx: &App) -> CommandOpenIn {
    if !cmd.keys.is_empty() {
        return CommandOpenIn::Current;
    }
    cx.try_global::<AppSettings>()
        .and_then(|s| s.client.commands.open_in.get(id).copied())
        .unwrap_or_else(|| from_plugin_open_in(cmd.open_in))
}

/// Whether the active plugin command `id` sends keys rather than running a
/// command line (so placement overrides do not apply to it).
pub fn is_keys_command(id: &str, cx: &App) -> bool {
    cx.try_global::<State>().and_then(|s| s.active.get(id)).is_some_and(|e| !e.command.keys.is_empty())
}

/// The active plugin manifest's own default placement for `id`, if loaded.
pub fn plugin_default_open_in(id: &str, cx: &App) -> Option<CommandOpenIn> {
    cx.try_global::<State>().and_then(|s| s.active.get(id)).map(|e| from_plugin_open_in(e.command.open_in))
}

/// Replace the registry's plugin commands with `installed`'s.
fn replace(link: Option<HostLink>, installed: Vec<Installed>, cx: &mut App) {
    let overrides = cx.try_global::<AppSettings>().map(|s| s.client.commands.open_in.clone()).unwrap_or_default();
    let (commands, active, order, menu) = build_with_overrides(link.as_ref(), &installed, &overrides);
    {
        let state = cx.default_global::<State>();
        state.stale = false;
        state.active = active;
        state.order = order;
        state.open_in_overrides = overrides;
    }
    cx.set_global(MenuCommands(menu));
    let reg = cx.default_global::<CommandRegistry>();
    reg.unregister_plugins();
    for c in commands {
        reg.register(c);
    }
}

/// Re-register already-loaded plugin commands after `commands.open_in` changed
/// so palette detail labels update live without re-fetching from the host.
fn rebuild_from_active(overrides: BTreeMap<String, CommandOpenIn>, cx: &mut App) {
    let commands: Vec<Command> = {
        let state = cx.default_global::<State>();
        state.open_in_overrides = overrides.clone();
        state
            .order
            .iter()
            .filter_map(|id| state.active.get(id).map(|e| (id, e)))
            .map(|(id, e)| {
                let open_in = if e.command.keys.is_empty() {
                    overrides.get(id).copied().unwrap_or_else(|| from_plugin_open_in(e.command.open_in))
                } else {
                    CommandOpenIn::Current
                };
                command(&e.origin, &e.plugin_id, &e.plugin, &e.command, open_in)
            })
            .collect()
    };
    let reg = cx.default_global::<CommandRegistry>();
    reg.unregister_plugins();
    for c in commands {
        reg.register(c);
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "used by unit tests"))]
fn build(link: Option<&HostLink>, installed: &[Installed]) -> (Vec<Command>, Vec<MenuCommand>) {
    let (commands, _, _, menu) = build_with_overrides(link, installed, &BTreeMap::new());
    (commands, menu)
}

#[cfg_attr(not(test), expect(dead_code, reason = "used by unit tests"))]
fn build_with_active(link: Option<&HostLink>, installed: &[Installed]) -> (Vec<Command>, BTreeMap<String, MenuCommand>, Vec<MenuCommand>) {
    let (commands, active, _, menu) = build_with_overrides(link, installed, &BTreeMap::new());
    (commands, active, menu)
}

fn build_with_overrides(
    link: Option<&HostLink>,
    installed: &[Installed],
    overrides: &BTreeMap<String, CommandOpenIn>,
) -> (Vec<Command>, BTreeMap<String, MenuCommand>, Vec<String>, Vec<MenuCommand>) {
    let Some(link) = link else {
        return (Vec::new(), BTreeMap::new(), Vec::new(), Vec::new());
    };
    let active_cmds = installed
        .iter()
        .filter(|p| p.enabled && p.incompatible.is_none())
        .flat_map(|p| p.manifest.commands.iter().filter(|c| !p.unavailable_commands.contains(&c.id)).map(move |c| (p, c)));
    let mut commands = Vec::new();
    let mut active = BTreeMap::new();
    let mut order = Vec::new();
    let mut menu = Vec::new();
    for (p, c) in active_cmds {
        let mut hydrated = c.clone();
        hydrated.open_in = p.command_open_in_for(c);
        let id = command_id(&p.manifest.id, &hydrated.id);
        let open_in = if hydrated.keys.is_empty() {
            overrides.get(&id).copied().unwrap_or_else(|| from_plugin_open_in(hydrated.open_in))
        } else {
            CommandOpenIn::Current
        };
        commands.push(command(link, &p.manifest.id, &p.manifest.name, &hydrated, open_in));
        let entry = MenuCommand {
            plugin_id: p.manifest.id.clone(),
            plugin: p.manifest.name.clone(),
            command: Rc::new(hydrated),
            origin: link.clone(),
        };
        order.push(id.clone());
        active.insert(id, entry.clone());
        if c.menu.is_some() {
            menu.push(entry);
        }
    }
    (commands, active, order, menu)
}

/// The palette id of a plugin's command: `plugin.<plugin>.<command>`.
pub fn command_id(plugin: &str, command: &str) -> String {
    format!("plugin.{plugin}.{command}")
}

/// Look up an active plugin command in `reg` by its `plugin.<plugin>.<command>` id,
/// returning `None` while a catalog refresh is in flight (`stale`).
pub(crate) fn lookup<'a>(reg: &'a CommandRegistry, stale: bool, id: &str) -> Option<&'a Rc<Command>> {
    (!stale).then(|| reg.get(id).filter(|c| matches!(c.source, Source::Plugin(_)))).flatten()
}

fn resolve_dispatch<'a>(
    reg: &'a CommandRegistry,
    state: &'a State,
    id: &str,
    expected: Option<(&PaletteCommand, &HostLink)>,
) -> Option<&'a MenuCommand> {
    if state.stale {
        return None;
    }
    lookup(reg, state.stale, id)?;
    let entry = state.active.get(id)?;
    if let Some((cmd, origin)) = expected
        && (entry.command.as_ref() != cmd || !entry.origin.same(origin))
    {
        return None;
    }
    Some(entry)
}

/// Whether a shortcut bound to `id` should go to the terminal as a normal
/// key: the command is the active host's and says where it applies, and
/// the focused pane (running `process`, `at_prompt` or not) isn't such a
/// place (#240).
pub fn passes_through(id: &str, process: Option<&str>, at_prompt: bool, cx: &App) -> bool {
    cx.try_global::<State>().and_then(|s| s.active.get(id)).is_some_and(|c| !c.fits(process, at_prompt))
}

/// Run the active plugin command with id `id` (`plugin.<plugin>.<command>`),
/// or tell the user why it isn't available on the current host.
pub fn run_by_id(id: &str, selection: Option<&str>, window: &mut Window, cx: &mut App) {
    run_checked(id, None, selection, window, cx);
}

/// Run a menu item's plugin command only if the active host still offers the
/// same definition that was displayed when the menu opened.
pub fn run_menu_command(
    id: &str,
    expected: &PaletteCommand,
    expected_origin: &HostLink,
    selection: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) {
    run_checked(id, Some((expected, expected_origin)), selection, window, cx);
}

fn run_checked(id: &str, expected: Option<(&PaletteCommand, &HostLink)>, selection: Option<&str>, window: &mut Window, cx: &mut App) {
    let entry = cx
        .try_global::<State>()
        .zip(cx.try_global::<CommandRegistry>())
        .and_then(|(state, reg)| resolve_dispatch(reg, state, id, expected).cloned());
    if let Some(entry) = entry {
        start(&entry.plugin_id, &entry.plugin, &entry.command, &entry.origin, selection, window, cx);
    } else {
        crate::toast::show_in(window, cx, unavailable_toast(id, expected, false, cx));
    }
}

fn unavailable_toast(id: &str, expected: Option<(&PaletteCommand, &HostLink)>, reopen: bool, cx: &App) -> crate::toast::Toast {
    let changed = expected.is_some() && cx.try_global::<State>().filter(|s| !s.stale).and_then(|s| s.active.get(id)).is_some();
    let (title, body) = if changed {
        (
            "Plugin command changed",
            if reopen {
                format!("{id} changed after the form opened. Reopen the command to run the updated version.")
            } else {
                format!("{id} changed since the menu opened. Open the menu again to run the updated command.")
            },
        )
    } else if reopen {
        ("Plugin command isn't available", format!("{id} is no longer available on this host. Reopen the command once it's active again."))
    } else {
        (
            "Plugin command isn't available",
            format!("{id} isn't available here (plugin not installed or enabled, a setting is off, or its program is missing)."),
        )
    };
    crate::toast::Toast::info(title).body(body).key("plugin.command.unavailable")
}

fn can_submit(id: &str, expected: &PaletteCommand, expected_origin: &HostLink, cx: &App) -> bool {
    cx.try_global::<State>()
        .zip(cx.try_global::<CommandRegistry>())
        .is_some_and(|(state, reg)| resolve_dispatch(reg, state, id, Some((expected, expected_origin))).is_some())
}

fn command(_link: &HostLink, plugin: &str, name: &str, c: &PaletteCommand, open_in: CommandOpenIn) -> Command {
    let id = command_id(plugin, &c.id);
    let run_id = id.clone();
    let mut out = Command::handler(&id, name, &c.title, move |window, cx| run_by_id(&run_id, None, window, cx)).with_key_action(id);
    let mut words: Vec<&str> = c.keywords.iter().map(String::as_str).collect();
    words.push(plugin);
    out = out.keywords(&words);
    out.detail = Some(if open_in == CommandOpenIn::Current {
        format!("{plugin} plugin").into()
    } else {
        format!("{plugin} plugin · {}", open_in.short_label()).into()
    });
    out.source = Source::Plugin(plugin.to_string().into());
    out.pane = c.pane.clone();
    out
}

/// Ask for `cmd`'s inputs (none: run it right away). `plugin_id`/`plugin`:
/// the plugin's id and name. `selection`: the text selected in the pane,
/// for the input its menu entry fills (still asked, so the user sees and
/// can change it).
pub fn start(
    plugin_id: &str,
    plugin: &str,
    cmd: &Rc<PaletteCommand>,
    origin: &HostLink,
    selection: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) {
    if cmd.inputs.is_empty() {
        let id = command_id(plugin_id, &cmd.id);
        run(&id, cmd, origin, &Answers::new(), window, cx);
        return;
    }
    let (plugin_id, plugin, cmd, origin, selection) =
        (plugin_id.to_string(), plugin.to_string(), cmd.clone(), origin.clone(), selection.map(str::to_string));
    // Deferred: the palette runs commands from inside the workspace.
    window.defer(cx, move |window, cx| {
        // Sources run in the focused pane's directory (on the host it reports).
        let cwd = window
            .root::<crate::workspace::Workspace>()
            .flatten()
            .and_then(|ws| ws.read(cx).focused_terminal())
            .and_then(|v| v.read(cx).cwd())
            .map(|c| c.to_string_lossy().into_owned());
        let a = ask_for(&plugin_id, &plugin, &cmd, &origin, selection.as_deref(), cwd);
        ask(window, cx, a);
    });
}

fn ask_for(
    plugin_id: &str,
    plugin: &str,
    cmd: &Rc<PaletteCommand>,
    origin: &HostLink,
    selection: Option<&str>,
    cwd: Option<String>,
) -> Ask {
    let fill = cmd.menu.as_ref().and_then(|m| m.fill.as_deref());
    let selection = selection.map(selection_value);
    let fields = cmd
        .inputs
        .iter()
        .map(|i| {
            let loader = i.source.as_ref().map(|_| source_loader(origin, plugin_id, &cmd.id, i, cwd.clone()));
            let field = Field::from_plugin(i, loader);
            match &selection {
                Some(text) if fill == Some(i.id.as_str()) => field.initial(text.clone()),
                _ => field,
            }
        })
        .collect();
    let id = command_id(plugin_id, &cmd.id);
    let (then, then_origin) = (cmd.clone(), origin.clone());
    let title = format!("{plugin}: {}", cmd.title);
    Ask::new(title, fields, move |answers, window, cx| {
        if !can_submit(&id, &then, &then_origin, cx) {
            crate::toast::show_in(window, cx, unavailable_toast(&id, Some((&then, &then_origin)), true, cx));
            return;
        }
        run(&id, &then, &then_origin, answers, window, cx);
    })
    .remote(!origin.is_local())
}

/// Fetches `input`'s suggestions from the host (`CommandSuggest`) with the
/// answers so far.
fn source_loader(link: &HostLink, plugin: &str, command: &str, input: &CommandInput, cwd: Option<String>) -> Loader {
    let (link, plugin, command, input) = (link.clone(), plugin.to_string(), command.to_string(), input.id.clone());
    Rc::new(move |answers: &Answers| {
        let req = Request::CommandSuggest {
            plugin: plugin.clone(),
            command: command.clone(),
            input: input.clone(),
            values: answers.clone(),
            cwd: cwd.clone(),
        };
        let link = link.clone();
        Box::new(move || crate::snippets::suggestions(link.plugin(req)))
    })
}

/// Selected text as one input value: the first non-empty line, trimmed,
/// without control characters (inputs are single lines).
fn selection_value(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    line.chars().filter(|c| !c.is_control()).collect()
}

/// Run `c` for `answers`: send `keys` to the focused pane's program (#240),
/// type the command line into the focused pane (`Current`), or launch it
/// directly in a split pane / new tab on `origin` (#241).
fn run(id: &str, c: &PaletteCommand, origin: &HostLink, answers: &Answers, window: &mut Window, cx: &mut App) {
    if !c.keys.is_empty() {
        let (keys, title, pane, origin) = (strokes(c, answers), c.title.clone(), c.pane.clone(), origin.clone());
        // Only empty optional values: nothing to type.
        if keys.is_empty() {
            let toast = crate::toast::Toast::info(format!("\u{201c}{title}\u{201d} had no keys to send")).body("Its values were empty.");
            crate::toast::show_in(window, cx, toast.key("plugin.command"));
            return;
        }
        window.defer(cx, move |window, cx| {
            if let Some(view) = crate::typing::keys_target(&origin, &title, pane.as_ref(), window, cx) {
                view.update(cx, |v, cx| v.send_strokes(&keys, cx));
            }
        });
        return;
    }
    let open_in = effective_open_in(id, c, cx);
    let line = command_line(c, answers);
    if open_in == CommandOpenIn::Current {
        let origin = origin.clone();
        // Deferred: the palette runs commands from inside the workspace.
        window.defer(cx, move |window, cx| {
            if let Some(view) = crate::typing::target(&origin, &line, crate::typing::What::Command, window, cx) {
                view.update(cx, |v, cx| v.run_line(&line, cx));
            }
        });
        return;
    }
    let needs_home = needs_home_expansion(c, answers);
    let cmd_owned = c.clone();
    let answers_owned = answers.clone();
    let label = c.run.first().cloned().unwrap_or_else(|| c.title.clone());
    let launch = crate::workspace::LaunchCommand {
        label,
        open_in,
        origin: Some(origin.clone()),
        needs_home,
        build_argv: Box::new(move |home| {
            let argv = command_argv(&cmd_owned, &answers_owned, home);
            if argv.is_empty() { Err("command has no program to run".into()) } else { Ok(argv) }
        }),
        fallback_line: line,
    };
    window.defer(cx, move |window, cx| {
        if let Some(Some(ws)) = window.root::<crate::workspace::Workspace>() {
            ws.update(cx, |ws, cx| ws.open_command(launch, window, cx));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selections_become_one_line() {
        assert_eq!(selection_value("\n  web-1  \nweb-2\n"), "web-1");
        assert_eq!(selection_value("a\tb\u{1b}[A"), "ab[A");
        assert_eq!(selection_value(""), "");
    }

    #[test]
    fn ids_cannot_clash_with_builtins() {
        let builtin = crate::command_palette::registry::builtins();
        assert!(builtin.iter().all(|c| !c.id.starts_with("plugin.")));
        assert_eq!(command_id("docker", "exec"), "plugin.docker.exec");
    }

    #[test]
    fn build_filters_unavailable_commands_and_sets_key_action() {
        let m = goatty_plugin::manifest::parse(
            r#"
schema = 1
id = "docker"
name = "Docker"
version = "1.0.0"
summary = "Containers"
publisher = "tests"

[requires]
commands = ["docker"]

[[command]]
id = "ps"
title = "List containers"
run = ["docker", "ps"]
[command.menu]
icon = "search"

[[command]]
id = "hidden"
title = "Hidden command"
run = ["docker", "info"]
[command.menu]
icon = "search"
"#,
            None,
        )
        .unwrap();
        let inst = Installed {
            id: "docker".into(),
            version: "1.0.0".into(),
            source: "test".into(),
            verified: true,
            manifest: m,
            enabled: true,
            incompatible: None,
            update: None,
            installed_unix: 0,
            builtin: false,
            packages: vec![],
            unavailable_commands: vec!["hidden".into()],
            command_open_in: BTreeMap::new(),
        };
        let link = HostLink::Local(std::sync::Arc::new(goatty_shell::HostService::new(goatty_shell::Launcher::new(None, None), None)));
        let (cmds, menu) = build(Some(&link), std::slice::from_ref(&inst));
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].id.as_ref(), "plugin.docker.ps");
        assert_eq!(cmds[0].key_action(), Some("plugin.docker.ps"));
        assert_eq!(menu.len(), 1);
        assert_eq!(menu[0].command.id, "ps");

        // Disabled plugin contributes nothing.
        let mut disabled = inst;
        disabled.enabled = false;
        let (cmds, menu) = build(Some(&link), &[disabled]);
        assert!(cmds.is_empty());
        assert!(menu.is_empty());
    }

    #[test]
    fn lookup_finds_only_plugin_commands() {
        let mut reg = CommandRegistry::default();
        let builtin = Command::handler("new_tab", "Tabs", "New tab", |_, _| {});
        reg.register(builtin);

        let mut plugin = Command::handler("plugin.docker.ps", "Docker", "List containers", |_, _| {});
        plugin.source = Source::Plugin("docker".into());
        reg.register(plugin);

        assert!(lookup(&reg, false, "plugin.docker.ps").is_some());
        assert!(lookup(&reg, true, "plugin.docker.ps").is_none());
        assert!(lookup(&reg, false, "plugin.docker.missing").is_none());
        assert!(lookup(&reg, false, "new_tab").is_none());
    }

    #[test]
    fn resolve_dispatch_rejects_stale_and_changed_menu_commands() {
        let link = HostLink::Local(std::sync::Arc::new(goatty_shell::HostService::new(goatty_shell::Launcher::new(None, None), None)));
        let other = HostLink::Local(std::sync::Arc::new(goatty_shell::HostService::new(goatty_shell::Launcher::new(None, None), None)));
        let m = goatty_plugin::manifest::parse(
            r#"
schema = 1
id = "docker"
name = "Docker"
version = "1.0.0"
summary = "Containers"
publisher = "tests"

[requires]
commands = ["docker"]

[[command]]
id = "ps"
title = "List containers"
run = ["docker", "ps"]
"#,
            None,
        )
        .unwrap();
        let orig_cmd = m.commands[0].clone();
        let inst = Installed {
            id: "docker".into(),
            version: "1.0.0".into(),
            source: "test".into(),
            verified: true,
            manifest: m,
            enabled: true,
            incompatible: None,
            update: None,
            installed_unix: 0,
            builtin: false,
            packages: vec![],
            unavailable_commands: vec![],
            command_open_in: BTreeMap::new(),
        };
        let (cmds, active, _) = build_with_active(Some(&link), &[inst]);
        let mut reg = CommandRegistry::default();
        for c in cmds {
            reg.register(c);
        }
        let mut state = State {
            link: Some(link.clone()),
            generation: 1,
            stale: false,
            active,
            order: vec!["plugin.docker.ps".into()],
            open_in_overrides: BTreeMap::new(),
            last_issues: Vec::new(),
        };

        assert!(resolve_dispatch(&reg, &state, "plugin.docker.ps", Some((&orig_cmd, &link))).is_some());

        let mut changed_cmd = orig_cmd.clone();
        changed_cmd.run = vec!["docker".into(), "ps".into(), "-a".into()];
        assert!(resolve_dispatch(&reg, &state, "plugin.docker.ps", Some((&changed_cmd, &link))).is_none());
        assert!(resolve_dispatch(&reg, &state, "plugin.docker.ps", Some((&orig_cmd, &other))).is_none());
        assert!(resolve_dispatch(&reg, &state, "plugin.docker.ps", None).is_some());

        state.stale = true;
        assert!(resolve_dispatch(&reg, &state, "plugin.docker.ps", None).is_none());
        assert!(resolve_dispatch(&reg, &state, "plugin.docker.ps", Some((&orig_cmd, &link))).is_none());

        state.stale = false;
        state.active.remove("plugin.docker.ps");
        reg.unregister_plugins();
        assert!(resolve_dispatch(&reg, &state, "plugin.docker.ps", Some((&orig_cmd, &link))).is_none());
    }

    #[test]
    fn should_report_issues_suppresses_repeats_until_cleared_or_changed() {
        let mut state = State::default();
        let issues = vec!["installed/lock.json is unreadable".to_string()];
        assert!(should_report_issues(&mut state, &issues));
        // Repeated identical issues on the same host are suppressed.
        assert!(!should_report_issues(&mut state, &issues));
        // Clearing on host change (or empty issues) lets the same issue report again.
        state.last_issues.clear();
        assert!(should_report_issues(&mut state, &issues));
        assert!(!should_report_issues(&mut state, &[]));
        assert!(should_report_issues(&mut state, &issues));
    }

    #[test]
    fn ask_for_builds_fields_with_source_loader_and_selection_fill() {
        let m = goatty_plugin::manifest::parse(
            r#"
schema = 1
id = "github"
name = "GitHub CLI"
version = "1.0.0"
summary = "GitHub"
publisher = "tests"

[permissions]
snippet-sources = "Lists pull requests."

[requires]
commands = ["gh"]

[[command]]
id = "pr-checkout"
title = "Check out a pull request"
run = ["gh", "pr", "checkout", "{pr}"]
[command.menu]
when = "selection"
fill = "pr"

[[command.input]]
id = "pr"
label = "Pull request"
[command.input.source]
run = ["gh", "pr", "list"]
"#,
            None,
        )
        .unwrap();
        let cmd = Rc::new(m.commands[0].clone());
        let link = HostLink::Local(std::sync::Arc::new(goatty_shell::HostService::new(goatty_shell::Launcher::new(None, None), None)));
        let ask = ask_for(&m.id, &m.name, &cmd, &link, Some("  245\nextra"), Some("/tmp/repo".into()));
        assert_eq!(ask.title.as_ref(), "GitHub CLI: Check out a pull request");
        assert!(ask.local);
        assert_eq!(ask.fields.len(), 1);
        assert_eq!(ask.fields[0].spec.id, "pr");
        assert_eq!(ask.fields[0].spec.default.as_deref(), Some("245"));
        assert!(ask.fields[0].loader.is_some());
    }

    #[test]
    fn build_with_overrides_shows_placement_in_detail() {
        let m = goatty_plugin::manifest::parse(
            r#"
schema = 1
id = "git"
name = "Git"
version = "1.0.0"
summary = "Git tools"
publisher = "tests"

[requires]
commands = ["lazygit"]

[[command]]
id = "lazygit"
title = "Open lazygit"
run = ["lazygit"]
open_in = "split-right"
"#,
            None,
        )
        .unwrap();
        let inst = Installed {
            id: "git".into(),
            version: "1.0.0".into(),
            source: "test".into(),
            verified: true,
            command_open_in: Installed::command_open_in_map(&m),
            manifest: m,
            enabled: true,
            incompatible: None,
            update: None,
            installed_unix: 0,
            builtin: false,
            packages: vec![],
            unavailable_commands: vec![],
        };
        // Round-trip through JSON to verify remote daemon replies preserve open_in.
        let inst: Installed = serde_json::from_str(&serde_json::to_string(&inst).unwrap()).unwrap();
        let link = HostLink::Local(std::sync::Arc::new(goatty_shell::HostService::new(goatty_shell::Launcher::new(None, None), None)));
        let (cmds, active, _, _) = build_with_overrides(Some(&link), std::slice::from_ref(&inst), &BTreeMap::new());
        assert_eq!(cmds[0].detail.as_deref(), Some("git plugin · split right"));
        assert_eq!(active["plugin.git.lazygit"].command.open_in, PluginOpenIn::SplitRight);

        let overrides = BTreeMap::from([("plugin.git.lazygit".to_string(), CommandOpenIn::Tab)]);
        let (cmds, _, _, _) = build_with_overrides(Some(&link), &[inst], &overrides);
        assert_eq!(cmds[0].detail.as_deref(), Some("git plugin · new tab"));
    }
}

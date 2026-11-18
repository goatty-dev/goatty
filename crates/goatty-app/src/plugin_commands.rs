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

use std::rc::Rc;

use goatty_plugin::api::{Installed, Reply, Request};
use goatty_plugin::commands::{CommandInput, MenuWhen, PaletteCommand, command_line};
use gpui::{App, AsyncApp, Global, Window};

use crate::command_palette::input::Loader;
use crate::command_palette::registry::Source;
use crate::command_palette::{Answers, Ask, Command, CommandRegistry, Field, ask};
use crate::settings_window::{HostLink, HostLinkGlobal};

/// Which host the registered plugin commands are from, and the newest fetch.
#[derive(Default)]
struct State {
    link: Option<HostLink>,
    generation: u64,
    /// True while a catalog refresh for the active host is in flight.
    stale: bool,
    active: std::collections::BTreeMap<String, MenuCommand>,
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

/// Keep the palette's plugin commands in step with the active host.
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
        let reply = cx.background_executor().spawn(async move { l.plugin(Request::Catalog { refresh: false }) }).await;
        cx.update(|cx| {
            if cx.default_global::<State>().generation != generation {
                return;
            }
            match reply {
                Ok(Reply::Catalog(c)) => replace(Some(link), c.installed, cx),
                Ok(_) => {
                    cx.default_global::<State>().stale = false;
                    log::warn!("plugin commands: unexpected reply to a catalog request");
                }
                // An older goattyd, or a host without plugins: keep none.
                Err(e) => {
                    log::info!("plugin commands: couldn't list {}'s plugins: {e:#}", link.label());
                    replace(None, Vec::new(), cx);
                }
            }
        });
    })
    .detach();
}

/// Replace the registry's plugin commands with `installed`'s.
fn replace(link: Option<HostLink>, installed: Vec<Installed>, cx: &mut App) {
    let (commands, active, menu) = build_with_active(link.as_ref(), &installed);
    {
        let state = cx.default_global::<State>();
        state.stale = false;
        state.active = active;
    }
    cx.set_global(MenuCommands(menu));
    let reg = cx.default_global::<CommandRegistry>();
    reg.unregister_plugins();
    for c in commands {
        reg.register(c);
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "used by unit tests"))]
fn build(link: Option<&HostLink>, installed: &[Installed]) -> (Vec<Command>, Vec<MenuCommand>) {
    let (commands, _, menu) = build_with_active(link, installed);
    (commands, menu)
}

fn build_with_active(
    link: Option<&HostLink>,
    installed: &[Installed],
) -> (Vec<Command>, std::collections::BTreeMap<String, MenuCommand>, Vec<MenuCommand>) {
    let Some(link) = link else {
        return (Vec::new(), std::collections::BTreeMap::new(), Vec::new());
    };
    let active_cmds = installed
        .iter()
        .filter(|p| p.enabled && p.incompatible.is_none())
        .flat_map(|p| p.manifest.commands.iter().filter(|c| !p.unavailable_commands.contains(&c.id)).map(move |c| (p, c)));
    let mut commands = Vec::new();
    let mut active = std::collections::BTreeMap::new();
    let mut menu = Vec::new();
    for (p, c) in active_cmds {
        commands.push(command(link, &p.manifest.id, &p.manifest.name, c));
        let entry = MenuCommand {
            plugin_id: p.manifest.id.clone(),
            plugin: p.manifest.name.clone(),
            command: Rc::new(c.clone()),
            origin: link.clone(),
        };
        active.insert(command_id(&p.manifest.id, &c.id), entry.clone());
        if c.menu.is_some() {
            menu.push(entry);
        }
    }
    (commands, active, menu)
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

/// Run the active plugin command with id `id` (`plugin.<plugin>.<command>`),
/// or tell the user why it isn't available on the current host.
pub fn run_by_id(id: &str, selection: Option<&str>, window: &mut Window, cx: &mut App) {
    let state = cx.try_global::<State>();
    let stale = state.is_some_and(|s| s.stale);
    let active = state.filter(|s| !s.stale).and_then(|s| s.active.get(id)).cloned();
    let in_reg = cx.try_global::<CommandRegistry>().is_some_and(|reg| lookup(reg, stale, id).is_some());
    if in_reg && let Some(entry) = active {
        start(&entry.plugin_id, &entry.plugin, &entry.command, &entry.origin, selection, window, cx);
    } else {
        crate::toast::show_in(
            window,
            cx,
            crate::toast::Toast::info("Plugin command isn't available")
                .body(format!("{id} isn't available here (plugin not installed or enabled, a setting is off, or its program is missing)."))
                .key("plugin.command.unavailable"),
        );
    }
}

fn command(link: &HostLink, plugin: &str, name: &str, c: &PaletteCommand) -> Command {
    let cmd = Rc::new(c.clone());
    let origin = link.clone();
    let (plugin_id, plugin_name) = (plugin.to_string(), name.to_string());
    let id = command_id(plugin, &c.id);
    let mut out = Command::handler(&id, name, &c.title, move |window, cx| start(&plugin_id, &plugin_name, &cmd, &origin, None, window, cx))
        .with_key_action(id);
    let mut words: Vec<&str> = c.keywords.iter().map(String::as_str).collect();
    words.push(plugin);
    out = out.keywords(&words);
    out.detail = Some(format!("{plugin} plugin").into());
    out.source = Source::Plugin(plugin.to_string().into());
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
    let fill = cmd.menu.as_ref().and_then(|m| m.fill.as_deref());
    let selection = selection.map(selection_value);
    if cmd.inputs.is_empty() {
        run(cmd, origin, &Answers::new(), window, cx);
        return;
    }
    // Sources run in the focused pane's directory (on the host it reports).
    let cwd = window
        .root::<crate::workspace::Workspace>()
        .flatten()
        .and_then(|ws| ws.read(cx).focused_terminal())
        .and_then(|v| v.read(cx).cwd())
        .map(|c| c.to_string_lossy().into_owned());
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
    let (then, then_origin) = (cmd.clone(), origin.clone());
    let title = format!("{plugin}: {}", cmd.title);
    let a = Ask::new(title, fields, move |answers, window, cx| run(&then, &then_origin, answers, window, cx)).remote(!origin.is_local());
    ask(window, cx, a);
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

/// Type `c`'s line for `answers` into the focused pane, if it's on the
/// host the command came from (`origin`) and a shell is waiting there;
/// otherwise offer to copy it.
fn run(c: &PaletteCommand, origin: &HostLink, answers: &Answers, window: &mut Window, cx: &mut App) {
    let line = command_line(c, answers);
    let origin = origin.clone();
    // Deferred: the palette runs commands from inside the workspace.
    window.defer(cx, move |window, cx| {
        if let Some(view) = crate::typing::target(&origin, &line, crate::typing::What::Command, window, cx) {
            view.update(cx, |v, cx| v.run_line(&line, cx));
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
}

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

use std::rc::Rc;

use goatty_plugin::api::{Installed, Reply, Request};
use goatty_plugin::commands::{MenuWhen, PaletteCommand, command_line};
use gpui::{App, AsyncApp, Global, Window};

use crate::command_palette::registry::Source;
use crate::command_palette::{Answers, Ask, Command, CommandRegistry, Field, ask};
use crate::settings_window::{HostLink, HostLinkGlobal};

/// Which host the registered plugin commands are from, and the newest fetch.
#[derive(Default)]
struct State {
    link: Option<HostLink>,
    generation: u64,
}

impl Global for State {}

/// A plugin command that is in the right-click menu too.
#[derive(Clone)]
pub struct MenuCommand {
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
    let moved = match (&state.link, &link) {
        (Some(a), Some(b)) => !a.same(b),
        (a, b) => a.is_some() != b.is_some(),
    };
    state.link = link.clone();
    let generation = state.generation;
    // Another host: its plugins aren't the last one's, so none until fetched.
    if moved {
        replace(None, Vec::new(), cx);
    }
    let Some(link) = link else { return };
    cx.spawn(async move |cx: &mut AsyncApp| {
        let l = link.clone();
        let reply = cx.background_executor().spawn(async move { l.plugin(Request::Catalog { refresh: false }) }).await;
        cx.update(|cx| {
            if cx.default_global::<State>().generation != generation {
                return;
            }
            match reply {
                Ok(Reply::Catalog(c)) => replace(Some(link), c.installed, cx),
                Ok(_) => log::warn!("plugin commands: unexpected reply to a catalog request"),
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
    let commands: Vec<Command> = match &link {
        Some(link) => installed
            .iter()
            .filter(|p| p.enabled && p.incompatible.is_none())
            .flat_map(|p| p.manifest.commands.iter().map(move |c| (p, c)))
            .map(|(p, c)| command(link, &p.manifest.id, &p.manifest.name, c))
            .collect(),
        None => Vec::new(),
    };
    let menu = match &link {
        Some(link) => installed
            .iter()
            .filter(|p| p.enabled && p.incompatible.is_none())
            .flat_map(|p| p.manifest.commands.iter().filter(|c| c.menu.is_some()).map(move |c| (p, c)))
            .map(|(p, c)| MenuCommand { plugin: p.manifest.name.clone(), command: Rc::new(c.clone()), origin: link.clone() })
            .collect(),
        None => Vec::new(),
    };
    cx.set_global(MenuCommands(menu));
    let reg = cx.default_global::<CommandRegistry>();
    reg.unregister_plugins();
    for c in commands {
        reg.register(c);
    }
}

/// The palette id of a plugin's command: `plugin.<plugin>.<command>`.
pub fn command_id(plugin: &str, command: &str) -> String {
    format!("plugin.{plugin}.{command}")
}

fn command(link: &HostLink, plugin: &str, name: &str, c: &PaletteCommand) -> Command {
    let cmd = Rc::new(c.clone());
    let origin = link.clone();
    let plugin_name = name.to_string();
    let mut out = Command::handler(&command_id(plugin, &c.id), name, &c.title, move |window, cx| {
        start(&plugin_name, &cmd, &origin, None, window, cx)
    });
    let mut words: Vec<&str> = c.keywords.iter().map(String::as_str).collect();
    words.push(plugin);
    out = out.keywords(&words);
    out.detail = Some(format!("{plugin} plugin").into());
    out.source = Source::Plugin(plugin.to_string().into());
    out
}

/// Ask for `cmd`'s inputs (none: run it right away). `selection`: the text
/// selected in the pane, for the input its menu entry fills (still asked,
/// so the user sees and can change it).
pub fn start(plugin: &str, cmd: &Rc<PaletteCommand>, origin: &HostLink, selection: Option<&str>, window: &mut Window, cx: &mut App) {
    let fill = cmd.menu.as_ref().and_then(|m| m.fill.as_deref());
    let selection = selection.map(selection_value);
    if cmd.inputs.is_empty() {
        run(cmd, origin, &Answers::new(), window, cx);
        return;
    }
    let fields = cmd
        .inputs
        .iter()
        .map(|i| {
            let field = Field::from_plugin(i);
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
}

//! Plugins' palette commands (`[[command]]` in `plugin.toml`, #91).
//!
//! The commands come from the plugins installed on the active tab's host
//! (the `HostLinkGlobal`): when that changes, or plugins were installed,
//! removed or switched, the host's catalog is fetched off the UI thread and
//! the registry's plugin commands are replaced. Picking one asks for its
//! inputs in the palette ([`crate::command_palette::input`]) and types its
//! command line (values quoted by `goatty_plugin::commands::command_line`)
//! into the focused pane, where the user sees it run.

use std::rc::Rc;

use goatty_plugin::api::{Installed, Reply, Request};
use goatty_plugin::commands::{PaletteCommand, command_line};
use gpui::{App, AsyncApp, ClipboardItem, Global, Window};

use crate::command_palette::registry::Source;
use crate::command_palette::{Answers, Ask, Command, CommandRegistry, Field, ask};
use crate::settings_window::{HostLink, HostLinkGlobal};
use crate::toast::{self, Toast};
use crate::workspace::Workspace;

/// Which host the registered plugin commands are from, and the newest fetch.
#[derive(Default)]
struct State {
    link: Option<HostLink>,
    generation: u64,
}

impl Global for State {}

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
    state.link = link.clone();
    let generation = state.generation;
    let Some(link) = link else {
        replace(None, Vec::new(), cx);
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
    let title = format!("{name}: {}", c.title);
    let remote = !link.is_local();
    let mut out = Command::handler(&command_id(plugin, &c.id), name, &c.title, move |window, cx| {
        if cmd.inputs.is_empty() {
            run(&cmd, &Answers::new(), window, cx);
            return;
        }
        let fields = cmd.inputs.iter().map(Field::from_plugin).collect();
        let then = cmd.clone();
        let a = Ask::new(title.clone(), fields, move |answers, window, cx| run(&then, answers, window, cx)).remote(remote);
        ask(window, cx, a);
    });
    let mut words: Vec<&str> = c.keywords.iter().map(String::as_str).collect();
    words.push(plugin);
    out = out.keywords(&words);
    out.detail = Some(format!("{plugin} plugin").into());
    out.source = Source::Plugin(plugin.to_string().into());
    out
}

/// Type `c`'s line for `answers` into the focused pane, if a shell is
/// waiting there; otherwise offer to copy it.
fn run(c: &PaletteCommand, answers: &Answers, window: &mut Window, cx: &mut App) {
    let line = command_line(c, answers);
    // Deferred: the palette runs commands from inside the workspace.
    window.defer(cx, move |window, cx| {
        let Some(view) = window.root::<Workspace>().flatten().and_then(|ws| ws.read(cx).focused_terminal()) else {
            toast::show(cx, Toast::warning("No terminal to run it in").body(line.clone()));
            return;
        };
        let busy = view.read(cx).process().filter(|p| !crate::workspace::icons::is_shell(p)).map(str::to_string);
        if let Some(program) = busy {
            let copy = line.clone();
            toast::show_in(
                window,
                cx,
                Toast::warning(format!("{program} is running in this pane"))
                    .body(format!("Run it at a prompt: {line}"))
                    .action("Copy the command", move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone())))
                    .key("plugin.command"),
            );
            return;
        }
        view.update(cx, |v, cx| v.run_line(&line, cx));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_cannot_clash_with_builtins() {
        let builtin = crate::command_palette::registry::builtins();
        assert!(builtin.iter().all(|c| !c.id.starts_with("plugin.")));
        assert_eq!(command_id("docker", "exec"), "plugin.docker.exec");
    }
}

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
    let origin = link.clone();
    let mut out = Command::handler(&command_id(plugin, &c.id), name, &c.title, move |window, cx| {
        if cmd.inputs.is_empty() {
            run(&cmd, &origin, &Answers::new(), window, cx);
            return;
        }
        let fields = cmd.inputs.iter().map(Field::from_plugin).collect();
        let (then, origin) = (cmd.clone(), origin.clone());
        let a = Ask::new(title.clone(), fields, move |answers, window, cx| run(&then, &origin, answers, window, cx)).remote(remote);
        ask(window, cx, a);
    });
    let mut words: Vec<&str> = c.keywords.iter().map(String::as_str).collect();
    words.push(plugin);
    out = out.keywords(&words);
    out.detail = Some(format!("{plugin} plugin").into());
    out.source = Source::Plugin(plugin.to_string().into());
    out
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
    fn ids_cannot_clash_with_builtins() {
        let builtin = crate::command_palette::registry::builtins();
        assert!(builtin.iter().all(|c| !c.id.starts_with("plugin.")));
        assert_eq!(command_id("docker", "exec"), "plugin.docker.exec");
    }
}

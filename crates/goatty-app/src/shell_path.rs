//! "Shell: Add a folder to PATH…" and "Remove a folder from PATH…" (#91):
//! palette commands that ask for a folder and change `shell.path` on the
//! active tab's host, through its goattyd like every host setting.

use anyhow::{Result, bail};
use goatty_config::{HostSettings, normalize_path_entry, path_entry_problem, store};
use gpui::{App, AsyncApp, Window};

use crate::command_palette::{Ask, Field, ask, input};
use crate::settings_window::{self, HostLink, HostLinkGlobal, Page};
use crate::toast::{self, Toast};
use crate::workspace::Workspace;

const KEY: &str = "shell.path";

fn link(cx: &mut App, what: &str) -> Option<HostLink> {
    let link = cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone());
    if link.is_none() {
        toast::show(cx, Toast::warning(format!("Can't {what}")).body("This window isn't connected to a goatty host.").key(KEY));
    }
    link
}

/// Ask for a folder (the focused pane's directory to start with) and add
/// it to PATH on the active tab's host.
pub fn ask_add(window: &mut Window, cx: &mut App) {
    let Some(link) = link(cx, "add a folder to PATH") else { return };
    // The workspace may be running this command: read it afterwards.
    window.defer(cx, move |window, cx| {
        let local = link.is_local();
        let home = local.then(|| std::env::var_os("HOME").map(std::path::PathBuf::from)).flatten();
        let cwd = window
            .root::<Workspace>()
            .flatten()
            .and_then(|ws| ws.read(cx).focused_terminal())
            .and_then(|view| view.read(cx).cwd())
            .map(|p| input::contract_home(&p, home.as_deref()));
        let mut field = Field::folder("dir", "Folder", true)
            .placeholder("~/bin or /opt/tool/bin")
            .help(format!("New goatty shells on {} find the programs in it", link.label()))
            .check(|v| path_entry_problem(v).map(String::from));
        if let Some(cwd) = cwd {
            field = field.initial(cwd);
        }
        let label = link.label();
        let a = Ask::new(format!("Add a folder to PATH on {label}"), vec![field], move |answers, _, cx| {
            let dir = normalize_path_entry(&answers["dir"]);
            change(link.clone(), Change::Add(dir), cx);
        })
        .remote(!local);
        ask(window, cx, a);
    });
}

/// Pick one of the added folders and remove it.
pub fn ask_remove(window: &mut Window, cx: &mut App) {
    let Some(link) = link(cx, "remove a folder from PATH") else { return };
    let handle = window.window_handle();
    let l = link.clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let path = cx.background_executor().spawn(async move { load(&l).map(|(s, _)| s.shell.path) }).await;
        cx.update(|cx| {
            let path = match path {
                Ok(p) => p,
                Err(e) => {
                    toast::show(cx, Toast::error("Couldn't read the host's settings").body(format!("{e:#}")).key(KEY));
                    return;
                }
            };
            if path.is_empty() {
                toast::show(
                    cx,
                    Toast::info(format!("No folders added to PATH on {}", link.label()))
                        .body("Only folders added in goatty's settings can be removed here; your dotfiles' PATH stays as it is.")
                        .action("Add a folder…", ask_add)
                        .key(KEY),
                );
                return;
            }
            let choices = path.into_iter().map(|d| (d.clone(), d, String::new())).collect();
            let label = link.label();
            let a = Ask::new(
                format!("Remove a folder from PATH on {label}"),
                vec![Field::choice("dir", "Folder", choices)],
                move |answers, _, cx| change(link.clone(), Change::Remove(answers["dir"].clone()), cx),
            );
            let _ = handle.update(cx, |_, window, cx| ask(window, cx, a));
        });
    })
    .detach();
}

enum Change {
    Add(String),
    Remove(String),
}

fn load(link: &HostLink) -> Result<(HostSettings, Vec<store::Unknown>)> {
    let loaded = store::parse::<HostSettings>(&link.get()?.json);
    Ok((loaded.value, loaded.unknown))
}

/// What [`apply`] did.
enum Done {
    /// Nothing to do (already there, or not on the list).
    Unchanged,
    /// Saved; `active`: goatty's managed zsh runs there, so it takes effect.
    Changed { active: bool },
}

/// One PATH change at a time: each loads the settings, changes them and
/// writes them back, so two at once would lose one of the changes.
static APPLYING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Apply `change` on `link`'s host. Blocking.
fn apply(link: &HostLink, change: &Change) -> Result<Done> {
    let _one = APPLYING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mut s, unknown) = load(link)?;
    match change {
        Change::Add(dir) if s.shell.path.contains(dir) => return Ok(Done::Unchanged),
        Change::Add(dir) => s.shell.path.push(dir.clone()),
        Change::Remove(dir) => {
            let before = s.shell.path.len();
            s.shell.path.retain(|d| d != dir);
            if s.shell.path.len() == before {
                return Ok(Done::Unchanged);
            }
        }
    }
    let applied = link.set(store::to_json(&s, &unknown), true)?;
    if !applied.applied {
        bail!("{}", if applied.issues.is_empty() { "the host didn't apply the change".into() } else { applied.issues.join("; ") });
    }
    Ok(Done::Changed { active: managed_zsh(&s) })
}

/// `shell.path` only reaches goatty's managed zsh.
pub(crate) fn managed_zsh(s: &HostSettings) -> bool {
    s.shell.managed && s.shell.program.as_deref().is_none_or(|p| p == "zsh" || p.ends_with("/zsh"))
}

fn change(link: HostLink, change: Change, cx: &mut App) {
    let id = toast::show(cx, Toast::loading("Changing PATH…").key(KEY));
    cx.spawn(async move |cx: &mut AsyncApp| {
        let l = link.clone();
        let (result, change) = cx.background_executor().spawn(async move { (apply(&l, &change), change) }).await;
        cx.update(|cx| {
            let label = link.label();
            let reload = link.clone();
            if matches!(result, Ok(Done::Changed { .. }))
                && cx.try_global::<crate::settings_window::HostLinkGlobal>().and_then(|g| g.0.as_ref()).is_some_and(|g| g.same(&link))
            {
                crate::plugin_commands::refresh(cx);
                crate::snippets::refresh(cx);
            }
            let settings = |t: Toast| t.action("PATH settings", |_, cx| settings_window::open_at(cx, Some(Page::Path))).key(KEY);
            let t = match (result, &change) {
                (Ok(Done::Changed { active: false }), Change::Add(dir)) => {
                    let body = format!(
                        "goatty's managed zsh is off on {label}, and only it adds these folders. Turn it on in the shell settings."
                    );
                    toast::update(cx, id, settings(Toast::warning(format!("Saved {dir}, but it isn't used yet")).body(body)));
                    return;
                }
                (Ok(Done::Changed { active: false }), Change::Remove(dir)) => {
                    toast::update(cx, id, Toast::success(format!("Removed {dir} from the list")).key(KEY));
                    return;
                }
                (Ok(Done::Changed { active: true }), Change::Add(dir)) => Toast::success(format!("Added {dir} to PATH"))
                    .body(format!("New goatty shells on {label} have it. Running shells get it when they reload.")),
                (Ok(Done::Changed { active: true }), Change::Remove(dir)) => Toast::success(format!("Removed {dir} from PATH"))
                    .body(format!("New goatty shells on {label} don't have it. Running shells change when they reload.")),
                (Ok(Done::Unchanged), Change::Add(dir)) => {
                    toast::update(cx, id, Toast::info(format!("{dir} is on PATH already")).key(KEY));
                    return;
                }
                (Ok(Done::Unchanged), Change::Remove(dir)) => {
                    toast::update(cx, id, Toast::info(format!("{dir} wasn't on the list")).key(KEY));
                    return;
                }
                (Err(e), _) => {
                    toast::update(cx, id, settings(Toast::error("Couldn't change PATH").body(format!("{e:#}"))));
                    return;
                }
            };
            let t = t.action("Reload shells", move |_, cx| crate::app::reload_shells_on(reload.clone(), cx)).key(KEY);
            toast::update(cx, id, t);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_only_reaches_managed_zsh() {
        let mut s = HostSettings::default();
        assert!(managed_zsh(&s));
        s.shell.program = Some("/usr/bin/zsh".into());
        assert!(managed_zsh(&s));
        s.shell.program = Some("fish".into());
        assert!(!managed_zsh(&s));
        s.shell.program = None;
        s.shell.managed = false;
        assert!(!managed_zsh(&s));
    }
}

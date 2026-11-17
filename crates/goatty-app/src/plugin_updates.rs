//! "2 plugin updates" (#95): a toast when the active tab's host has newer
//! versions of its installed plugins.
//!
//! goattyd fetches its registries in the background (once they're a day
//! old); the app asks the active tab's host (`Request::Updates`) when the
//! active host changes and every few hours, at most every [`CHECK_EVERY`]
//! per host. Each update is told once per run of the app. "Review" opens
//! the Plugins page, where every update still goes through the plan and
//! its approval: nothing is installed from here, least of all a version
//! that asks for new permissions. `plugins.notify_updates` turns it off.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use goatty_plugin::api::{Reply, Request, Update};
use gpui::{App, AsyncApp, Global};

use crate::settings::AppSettings;
use crate::settings_window::{self, HostLink, HostLinkGlobal};
use crate::toast::{self, Toast};

/// Ask one host at most this often.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
/// How often the timer looks whether a check is due.
const TICK: Duration = Duration::from_secs(30 * 60);
/// Leave the first minute after start to connecting and the first prompt.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);

#[derive(Default)]
struct State {
    /// Host key → when it was last asked.
    checked: HashMap<String, Instant>,
    /// (host key, plugin, version) already told about.
    told: HashSet<(String, String, String)>,
}

impl Global for State {}

fn host_key(link: &HostLink) -> String {
    link.remote_key().unwrap_or_else(|| "local".into())
}

pub fn init(cx: &mut App) {
    cx.observe_global::<HostLinkGlobal>(check).detach();
    cx.spawn(async move |cx: &mut AsyncApp| {
        cx.background_executor().timer(FIRST_CHECK_AFTER).await;
        loop {
            cx.update(check);
            cx.background_executor().timer(TICK).await;
        }
    })
    .detach();
}

/// Ask the active tab's host for updates, if that's due.
fn check(cx: &mut App) {
    if !cx.global::<AppSettings>().file.value.plugins.notify_updates {
        return;
    }
    let Some(link) = cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone()) else { return };
    if !link.is_connected() {
        return;
    }
    let key = host_key(&link);
    let state = cx.default_global::<State>();
    if state.checked.get(&key).is_some_and(|t| t.elapsed() < CHECK_EVERY) {
        return;
    }
    state.checked.insert(key.clone(), Instant::now());
    cx.spawn(async move |cx: &mut AsyncApp| {
        let l = link.clone();
        let reply = cx.background_executor().spawn(async move { l.plugin(Request::Updates) }).await;
        let updates = match reply {
            Ok(Reply::Updates { updates }) => updates,
            Ok(_) => return,
            // A goattyd from before #95, or no connection: try again later.
            Err(e) => {
                log::info!("plugin updates: couldn't ask {}: {e:#}", link.label());
                return;
            }
        };
        cx.update(|cx| {
            let state = cx.default_global::<State>();
            let new: Vec<Update> =
                updates.into_iter().filter(|u| state.told.insert((key.clone(), u.id.clone(), u.version.clone()))).collect();
            if !new.is_empty() {
                toast::show(cx, updates_toast(&new, &link));
            }
        });
    })
    .detach();
}

fn updates_toast(updates: &[Update], link: &HostLink) -> Toast {
    let n = updates.len();
    let mut title = if n == 1 { format!("{} {} is out", updates[0].name, updates[0].version) } else { format!("{n} plugin updates") };
    if link.remote_key().is_some() {
        title.push_str(&format!(" on {}", link.label()));
    }
    let mut lines: Vec<String> = updates.iter().map(|u| format!("{}: {} → {}", u.name, u.installed, u.version)).collect();
    if updates.iter().any(|u| !u.new_permissions.is_empty()) {
        lines.push("Some ask for new permissions: review them before updating.".into());
    }
    Toast::info(title)
        .body(lines.join("\n"))
        .key("plugins.updates")
        .duration(Duration::from_secs(20))
        .action("Review", |_, cx| settings_window::open_plugin_updates(cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_update_is_named_and_new_permissions_are_called_out() {
        let u = |id: &str, perms| Update {
            id: id.into(),
            name: id.into(),
            installed: "1.0.0".into(),
            version: "1.1.0".into(),
            new_permissions: perms,
        };
        let link = HostLink::Local(std::sync::Arc::new(goatty_shell::HostService::new(goatty_shell::Launcher::new(None, None), None)));
        let t = updates_toast(&[u("eza", vec![])], &link);
        assert_eq!(t.title.as_ref(), "eza 1.1.0 is out");
        let t = updates_toast(&[u("eza", vec![]), u("zoxide", vec![goatty_plugin::Permission::ShellCode])], &link);
        assert_eq!(t.title.as_ref(), "2 plugin updates");
        assert!(t.body.as_ref().is_some_and(|b| b.contains("new permissions")), "{:?}", t.body);
    }
}

//! goatty's own pick-from-a-list commands (#79), built on the palette's
//! [`Ask::pick`]: switch the theme (each theme shows live while you move
//! through the list; Esc puts the old one back), dark/light/follow the
//! system, and install one of the built-in shell tools on the active tab's
//! host.

use std::sync::{Arc, Mutex};

use goatty_config::ThemeEntry;
use goatty_config::ThemeSettings;
use goatty_shell::setup::{Tool, ToolLocation};
use gpui::{App, Window};

use crate::command_palette::input::{Job, Loaded, Suggestion};
use crate::command_palette::{Ask, Field, ask};
use crate::settings::AppSettings;
use crate::settings_window::HostLinkGlobal;
use crate::toast::{self, Toast};

/// Which theme setting a pick changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// `theme.name`: one theme always.
    Fixed,
    /// `theme.dark` / `theme.light`: following the system, the one in use now.
    Dark,
    Light,
}

impl Slot {
    fn of(t: &ThemeSettings, system_dark: bool) -> Self {
        match (t.follow_system, system_dark) {
            (false, _) => Slot::Fixed,
            (true, true) => Slot::Dark,
            (true, false) => Slot::Light,
        }
    }

    fn set(self, t: &mut ThemeSettings, id: &str) {
        let field = match self {
            Slot::Fixed => &mut t.name,
            Slot::Dark => &mut t.dark,
            Slot::Light => &mut t.light,
        };
        *field = id.to_string();
    }
}

/// "dark" or "light", from the background.
fn tone(t: &ThemeEntry) -> &'static str {
    if t.colors.background.luma() < 0.5 { "dark" } else { "light" }
}

/// The theme list: every theme, with whether it's dark or light, yours,
/// and the one in use now.
fn theme_items(themes: &[ThemeEntry], current: &str) -> Vec<Suggestion> {
    themes
        .iter()
        .map(|t| {
            let mut detail = vec![tone(t)];
            if t.user {
                detail.push("yours");
            }
            if t.id == current {
                detail.push("current");
            }
            Suggestion { value: t.id.clone(), label: t.name.clone(), detail: Some(detail.join(" · ")) }
        })
        .collect()
}

/// "Themes: Switch theme…". Following the system, it changes the theme for
/// the appearance in use now (dark or light).
pub fn switch_theme(window: &mut Window, cx: &mut App) {
    let s = cx.global::<AppSettings>();
    let original = s.client.theme.clone();
    let slot = Slot::of(&original, s.system_dark);
    let current = original.active_id(s.system_dark).to_string();
    let items = theme_items(&s.themes.all(), &current);
    let (title, label) = match slot {
        Slot::Fixed => ("Switch theme", "Theme"),
        Slot::Dark => ("Switch the dark theme (following the system)", "Dark theme"),
        Slot::Light => ("Switch the light theme (following the system)", "Light theme"),
    };
    // Previews start from the saved theme settings each time; Esc shows
    // those again, Enter saves the pick on top of them.
    let a = Ask::pick(title, label, items, Some(&current), move |id, _, cx| {
        AppSettings::end_theme_preview(cx, Some(&|t: &mut ThemeSettings| slot.set(t, id)));
        let name = cx.global::<AppSettings>().themes.get(id).map_or_else(|| id.to_string(), |t| t.name);
        toast::show(cx, Toast::success(format!("Theme: {name}")).key("themes.switch"));
    })
    .on_select(move |id, cx| AppSettings::preview_theme(cx, |t| slot.set(t, id)))
    .on_cancel(|cx| AppSettings::end_theme_preview(cx, None));
    ask(window, cx, a);
}

/// What "dark, light or follow the system" can be set to.
const FOLLOW: &str = "system";
const DARK: &str = "dark";
const LIGHT: &str = "light";

/// The appearance `t` has now: one of the choices, or `None` for a fixed
/// theme that's neither the dark nor the light one.
fn appearance_of(t: &ThemeSettings) -> Option<&'static str> {
    if t.follow_system {
        Some(FOLLOW)
    } else if t.name == t.dark {
        Some(DARK)
    } else if t.name == t.light {
        Some(LIGHT)
    } else {
        None
    }
}

fn set_appearance(t: &mut ThemeSettings, choice: &str) {
    match choice {
        FOLLOW => t.follow_system = true,
        DARK => {
            t.follow_system = false;
            t.name = t.dark.clone();
        }
        LIGHT => {
            t.follow_system = false;
            t.name = t.light.clone();
        }
        _ => {}
    }
}

/// "Themes: Dark, light or follow the system…".
pub fn switch_appearance(window: &mut Window, cx: &mut App) {
    let s = cx.global::<AppSettings>();
    let original = s.client.theme.clone();
    let name = |id: &str| s.themes.get(id).map_or_else(|| id.to_string(), |t| t.name);
    let now = if s.system_dark { "dark" } else { "light" };
    let items = vec![
        Suggestion { value: FOLLOW.into(), label: "Follow the system".into(), detail: Some(format!("the desktop is {now} now")) },
        Suggestion { value: DARK.into(), label: "Always dark".into(), detail: Some(name(&original.dark)) },
        Suggestion { value: LIGHT.into(), label: "Always light".into(), detail: Some(name(&original.light)) },
    ];
    let a = Ask::pick("Dark, light or follow the system", "Appearance", items, appearance_of(&original), move |choice, _, cx| {
        AppSettings::end_theme_preview(cx, Some(&|t: &mut ThemeSettings| set_appearance(t, choice)));
        let what = match choice {
            FOLLOW => "Following the system's dark or light mode",
            DARK => "Always the dark theme",
            _ => "Always the light theme",
        };
        toast::show(cx, Toast::success(what).body("Pick the themes with “Themes: Switch theme…”").key("themes.appearance"));
    })
    .on_select(|choice, cx| AppSettings::preview_theme(cx, |t| set_appearance(t, choice)))
    .on_cancel(|cx| AppSettings::end_theme_preview(cx, None));
    ask(window, cx, a);
}

/// The tools a pick can install or update: missing or outdated, with a
/// build for this host.
fn installable(tools: &[Tool]) -> Vec<Suggestion> {
    tools
        .iter()
        .filter(|t| t.unavailable.is_none() && (t.location == ToolLocation::Missing || t.outdated().is_some()))
        .map(|t| {
            let state = match t.outdated() {
                Some(old) => format!("update {old} → {}", t.version),
                None => format!("install {}", t.version),
            };
            let size = if t.download > 0 { format!(" · {:.1} MB", t.download as f64 / 1_000_000.0) } else { String::new() };
            Suggestion { value: t.id.clone(), label: t.name.clone(), detail: Some(format!("{} · {state}{size}", t.summary)) }
        })
        .collect()
}

/// "Shell: Install a built-in tool…" on the active tab's host: lists what's
/// missing (checking takes a moment: it starts a shell there), then
/// installs the pick with a progress toast.
pub fn install_tool(window: &mut Window, cx: &mut App) {
    let Some(link) = cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone()) else {
        toast::show(cx, Toast::info("No host to install on").body("This tab is still connecting.").key("shell.install_tool"));
        return;
    };
    let remote = !link.is_local();
    // What the check found, for the answer (the loader runs off the UI thread).
    let found: Arc<Mutex<Vec<Tool>>> = Arc::default();
    let (job_link, job_found) = (link.clone(), found.clone());
    let field = Field::loaded_list(
        "tool",
        "Tool",
        std::rc::Rc::new(move |_: &crate::command_palette::Answers| -> Job {
            let (link, found) = (job_link.clone(), job_found.clone());
            Box::new(move || match link.shell_status() {
                Ok(status) => {
                    let items = installable(&status.tools);
                    let note = items.is_empty().then(|| "Every built-in tool is installed here".to_string());
                    *found.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = status.tools;
                    Loaded { items, note }
                }
                Err(e) => Loaded {
                    items: Vec::new(),
                    note: Some(format!("Couldn't check the tools: {}", format!("{e:#}").lines().next().unwrap_or_default())),
                },
            })
        }),
    )
    .help(format!("Built-in shell tools goatty can install on {}", link.label()));
    let a = Ask::new("Install a built-in tool", vec![field], move |answers, _, cx| {
        let id = &answers["tool"];
        let tools = found.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let Some(tool) = tools.into_iter().find(|t| &t.id == id) else { return };
        crate::shell_setup::start_install(link.clone(), vec![tool], cx);
    })
    .remote(remote);
    ask(window, cx, a);
}

#[cfg(test)]
mod tests {
    use super::*;
    use goatty_config::{Color, ThemeColors};

    #[test]
    fn a_pick_changes_the_theme_in_use() {
        let mut t = ThemeSettings::default();
        assert_eq!(Slot::of(&t, true), Slot::Fixed);
        Slot::Fixed.set(&mut t, "nord");
        assert_eq!(t.name, "nord");
        t.follow_system = true;
        assert_eq!(Slot::of(&t, false), Slot::Light);
        Slot::of(&t, false).set(&mut t, "paper");
        assert_eq!((t.light.as_str(), t.active_id(false)), ("paper", "paper"));
        assert_eq!(t.name, "nord", "the fixed theme stays");
    }

    #[test]
    fn appearance_choices() {
        let mut t = ThemeSettings { name: "x".into(), dark: "night".into(), light: "day".into(), ..Default::default() };
        assert_eq!(appearance_of(&t), None);
        set_appearance(&mut t, LIGHT);
        assert_eq!((t.name.as_str(), t.follow_system, appearance_of(&t)), ("day", false, Some(LIGHT)));
        set_appearance(&mut t, FOLLOW);
        assert_eq!(appearance_of(&t), Some(FOLLOW));
        set_appearance(&mut t, DARK);
        assert_eq!((t.name.as_str(), t.follow_system), ("night", false));
        // Previews start from the saved settings each time: "Always dark",
        // then back to "Follow the system", leaves the fixed theme alone.
        let base = ThemeSettings { follow_system: true, name: "x".into(), dark: "night".into(), ..Default::default() };
        let mut shown = base.clone();
        set_appearance(&mut shown, DARK);
        let mut shown = base.clone();
        set_appearance(&mut shown, FOLLOW);
        assert_eq!(shown, base);
    }

    #[test]
    fn themes_say_what_they_are() {
        let colors = |bg: u32| ThemeColors { background: Color(bg), ..ThemeSettings::default().colors() };
        let entry = |id: &str, bg: u32, user: bool| ThemeEntry { id: id.into(), name: id.to_uppercase(), colors: colors(bg), user };
        let items = theme_items(&[entry("a", 0x101010, false), entry("b", 0xfafafa, true)], "b");
        assert_eq!(items[0].detail.as_deref(), Some("dark"));
        assert_eq!((items[1].label.as_str(), items[1].detail.as_deref()), ("B", Some("light · yours · current")));
    }

    #[test]
    fn only_missing_or_outdated_tools_are_offered() {
        let tool = |id: &str, location: ToolLocation, unavailable: Option<&str>| Tool {
            id: id.into(),
            name: id.into(),
            summary: "s".into(),
            enabled: true,
            default_install: true,
            location,
            download: 2_500_000,
            version: "2".into(),
            unavailable: unavailable.map(Into::into),
        };
        let tools = [
            tool("fzf", ToolLocation::Missing, None),
            tool("eza", ToolLocation::System { path: "/usr/bin/eza".into() }, None),
            tool("bat", ToolLocation::Managed { version: "1".into() }, None),
            tool("zoxide", ToolLocation::Missing, Some("no build")),
        ];
        let got: Vec<(String, Option<String>)> = installable(&tools).into_iter().map(|s| (s.value, s.detail)).collect();
        assert_eq!(got, [("fzf".into(), Some("s · install 2 · 2.5 MB".into())), ("bat".into(), Some("s · update 1 → 2 · 2.5 MB".into()))]);
    }
}

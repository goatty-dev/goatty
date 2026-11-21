//! GPUI bootstrap: settings, key bindings, the main window.

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};

use crate::actions::{OpenSettings, ReloadShells};
use crate::command_palette;
use crate::connect::ConnectPlan;
use crate::session::Spawner;
use crate::settings::AppSettings;
use crate::settings_window;
use crate::toast::{self, Toast};
use crate::workspace::Workspace;
use crate::workspace::saved::{SavedWindow, Startup};

/// What a terminal window starts with.
pub enum Initial {
    /// A session to show right away (`--local`).
    Ready { startup: Startup, spawner: Spawner },
    /// A goattyd to connect to first: the window opens at once, its tab
    /// shows the connect screen meanwhile (`crate::connect`, #37).
    Connect(ConnectPlan),
    /// A window from the last run, with its tabs on every host (#252).
    /// The second field: `--settings[=PAGE]`, opened once the active tab's
    /// host is there.
    Reopen(crate::workspace::reopen::WindowRecord, Option<String>),
}

/// `more`: further windows to open (the other windows of the last run).
pub fn run(initial: Initial, more: Vec<Initial>, open_settings: Option<String>) {
    gpui_platform::application().with_assets(crate::ui::icons::Assets).run(move |cx: &mut App| {
        AppSettings::init(cx);
        crate::agent::notify::init(cx);
        crate::workspace::reopen::init(cx);
        cx.on_action(|_: &OpenSettings, cx| settings_window::open(cx));
        cx.on_action(|_: &ReloadShells, cx| reload_shells(cx));
        command_palette::register_builtins(cx);
        crate::plugin_commands::init(cx);
        crate::plugin_updates::init(cx);
        crate::snippets::init(cx);
        // `~/.ssh/config`'s hosts, ready for the palette.
        crate::hosts::refresh(cx);

        crate::welcome::check_first_run(cx);
        cx.background_executor().spawn(async { crate::terminal::cleanup_stale_remote_cache() }).detach();
        open_terminal_window(cx, initial, open_settings);
        for initial in more {
            open_terminal_window(cx, initial, None);
        }
        crate::welcome::open_if_first_run(cx);
        // Quit with the last window (the settings window may outlive the
        // terminal window briefly, or close first).
        cx.on_window_closed(|cx, id| {
            crate::workspace::reopen::window_closed(cx, id);
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
    crate::terminal::cleanup_remote_cache();
}

/// Open a terminal window starting with `initial`. `open_settings`
/// (`--settings[=PAGE]`) opens the settings window too, once the window's
/// host is known.
pub fn open_terminal_window(cx: &mut App, initial: Initial, open_settings: Option<String>) {
    let (initial, open_settings) = match initial {
        Initial::Connect(mut plan) => {
            plan.open_settings = plan.open_settings.or(open_settings);
            (Initial::Connect(plan), None)
        }
        Initial::Reopen(record, settings) => (Initial::Reopen(record, settings.or(open_settings)), None),
        ready => (ready, open_settings),
    };
    // A restored layout resizes the window when it arrives (`Workspace`);
    // a reopened window knows its size right away.
    let (w, h) = match &initial {
        Initial::Reopen(r, _) => r.window.map_or(SavedWindow::DEFAULT, |w| w.size()),
        _ => SavedWindow::DEFAULT,
    };
    let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            app_id: Some("dev.goatty.goatty".into()),
            window_background: crate::workspace::background_appearance(&cx.global::<AppSettings>().client.terminal),
            window_decorations: Some(crate::workspace::requested_decorations(cx.global::<AppSettings>().client.window.title_bar)),
            ..Default::default()
        },
        |window, cx| {
            window.set_window_title("goatty");
            cx.new(|cx| Workspace::new(initial, window, cx))
        },
    );
    if let Err(e) = opened {
        log::error!("couldn't open the terminal window: {e:#}");
        // From the command line there's nothing else; from the palette the
        // other windows stay.
        if cx.windows().is_empty() {
            cx.quit();
        }
        return;
    }
    if let Some(page) = open_settings {
        settings_window::open_at(cx, settings_window::Page::from_name(&page));
    }
}

/// Ask the host's running managed shells to reload (the command and the
/// shortcut; the settings window has its own button). The result shows as
/// a toast.
fn reload_shells(cx: &mut App) {
    let Some(link) = cx.try_global::<settings_window::HostLinkGlobal>().and_then(|g| g.0.clone()) else {
        toast::show(cx, Toast::warning("Can't reload shells").body("This window isn't connected to a goatty host."));
        return;
    };
    reload_shells_on(link, cx);
}

/// Ask `link`'s host to reload its running managed shells, with a progress
/// toast that turns into the result (the settings window's button too).
/// One per host: reloading two hosts shows two toasts.
pub fn reload_shells_on(link: settings_window::HostLink, cx: &mut App) {
    let key = format!("shell.reload:{}", link.label());
    let id = toast::show(cx, Toast::loading("Reloading shells…").key(key.clone()));
    let retry = link.clone();
    cx.spawn(async move |cx| {
        let result = cx.background_executor().spawn(async move { link.reload_shells() }).await;
        cx.update(|cx| toast::update(cx, id, reload_result(result, retry).key(key)));
    })
    .detach();
}

fn reload_result(result: anyhow::Result<u32>, link: settings_window::HostLink) -> Toast {
    match result {
        Ok(0) => Toast::info("No goatty shells to reload").body("Only managed shells (goatty's zsh) can reload; none are running."),
        Ok(n) => Toast::success(if n == 1 { "Asked 1 shell to reload".to_string() } else { format!("Asked {n} shells to reload") })
            .body("Idle prompts restart now, busy shells after their current command."),
        Err(e) => Toast::error("Couldn't reload shells")
            .body(format!("{e:#}"))
            .action("Retry", move |_, cx| reload_shells_on(link.clone(), cx))
            .action("Shell settings", |_, cx| settings_window::open_at(cx, Some(settings_window::Page::Shell))),
    }
}
